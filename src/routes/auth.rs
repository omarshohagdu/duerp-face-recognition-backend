//! `POST /login` — exchanges DU credentials for this service's own JWT.
//!
//! WHY THIS HANDLER WRITES A STEP LOG: every failure mode here collapses into
//! the same `401` on the wire — wrong credentials, a wrong `secret-key`, an
//! `SSL_API_ENDPOINT` that answers with a redirect — and the SPA renders all of
//! them as "That username or password wasn't right" (`api/auth.ts`). Without a
//! per-login file there is nothing to tell those apart afterwards; the plain
//! version this replaced put DU's real status on stderr only.
//!
//! The cost is deliberate: one file per sign-in under `LOGIN_LOG_DIR`, carrying
//! the username, the client IP and DU's whole user object. `StepLogger` redacts
//! the password and the issued token by key name, nothing else. Those files are
//! PII — `main.rs` 404s `/uploads/login` ahead of the static route so they are
//! never web-readable, and that ordering is load-bearing.

use std::env;
use crate::models::auth::{LoginRequest, LoginResponse};
use crate::routes::wow_attendance::{full_path, log_local_response};
use crate::utils::jwt::create_jwt;
use crate::utils::step_logger::StepLogger;
use actix_web::{post, web, HttpRequest, HttpResponse};
use reqwest::Client;
use serde_json::{json, Value};
use sqlx::PgPool;
use crate::utils::constants::{LOGIN_ENDPOINT, SSL_SECRET_KEY};

/// Folder the per-login step logs are written to, `./uploads/login` by default
/// and overridable with `LOGIN_LOG_DIR`.
///
/// Deliberately NOT `WOW_LOG_DIR`: every sign-in writes a file here, so mixing
/// them into the attendance stream would bury the face calls that folder exists
/// to explain. Keeping them apart also means the retention decision can differ —
/// logins are high-volume and neither folder is rotated.
///
/// SECURITY: like the attendance logs, this sits INSIDE the folder `/uploads`
/// is served from, so the 404 block main.rs registers on `/uploads/login`
/// before that static route is load-bearing — these files carry usernames,
/// client IPs and employee ids. Move this path out of `<uploads>` and the block
/// no longer covers it: re-check exposure.
fn login_log_dir() -> String {
    env::var("LOGIN_LOG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "./uploads/login".to_string())
}

/// Base URL of the service that checks DU credentials, and which request shape
/// that service speaks.
///
/// Two upstreams exist and they do NOT share a contract:
///
/// * `LOGIN_API_ENDPOINT` — DU's ITS gateway (`https://api.its.du.ac.bd/`).
///   Takes JSON `{username, password}`, ignores `secret-key`, and answers
///   `{token, username, user_data}`.
/// * `SSL_API_ENDPOINT` — DU's Laravel backend: the historical upstream, and
///   still the one `getByEmployeeId` calls (the gateway does not serve it —
///   it 404s). Takes a form body keyed `email`, requires `secret-key`, and
///   answers `{access_token, ..., user}`.
///
/// Sending either shape to the other host fails as a plain 401 on the wire —
/// the gateway 400s a form body with "Content type error", Laravel 401s a JSON
/// body keyed `username` — which is precisely the confusion this handler's step
/// log exists to resolve. So the shape is chosen from WHICH variable supplied
/// the base and never guessed per request; the returned flag carries that
/// choice. `LOGIN_API_ENDPOINT` wins when both are set.
fn login_upstream() -> Option<(String, bool)> {
    let var = |k: &str| env::var(k).ok().filter(|s| !s.trim().is_empty());
    let (mut base, via_gateway) = match var("LOGIN_API_ENDPOINT") {
        Some(base) => (base, true),
        None => (var("SSL_API_ENDPOINT")?, false),
    };
    // `LOGIN_ENDPOINT` is a bare path segment, so the base has to end in `/`.
    // Appending it here rather than trusting the `.env` value: the old code
    // concatenated blindly, and a base written without the slash silently
    // produced `https://hostlogin` — a DNS error reaching the SPA as a 401.
    if !base.ends_with('/') {
        base.push('/');
    }
    Some((base, via_gateway))
}

#[post("/login")]
pub async fn login(
    http_req: HttpRequest,
    req: web::Json<LoginRequest>,
    db: web::Data<PgPool>,
) -> HttpResponse {
    let log = StepLogger::new_in("login", &login_log_dir());
    log.set_endpoint(http_req.method().as_str(), &full_path(&http_req));
    // The submitted credentials are the whole input here. `redact` masks values
    // by key name, so the file records THAT a password was sent and never what
    // it was.
    log.params(
        "json",
        &json!({ "username": req.username, "password": req.password }),
    );
    // The only id known before DU answers. Replaced by the token subject on
    // success, so a failed attempt is still filed under something searchable
    // rather than `unknown`.
    log.set_id(&req.username);

    // Split so the logger outlives every early return in the handler body: it
    // flushes on Drop, and `log_local_response` records whichever branch won.
    let resp = login_inner(&log, &http_req, req, db).await;
    log_local_response(&log, resp).await
}

async fn login_inner(
    log: &StepLogger,
    http_req: &HttpRequest,
    req: web::Json<LoginRequest>,
    _db: web::Data<PgPool>,
) -> HttpResponse {
    let client_ip = http_req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    let client = Client::new();

    // Which upstream checks the password, and in which dialect. Unset config
    // used to `.unwrap()` here, panicking the worker on a request that a 500
    // answers just as well — and the step log now says so.
    let (base, via_gateway) = match login_upstream() {
        Some(upstream) => upstream,
        None => {
            log.step("no upstream configured — set LOGIN_API_ENDPOINT (or SSL_API_ENDPOINT)");
            return HttpResponse::InternalServerError().json("Login service unavailable");
        }
    };
    let endpoint = base + LOGIN_ENDPOINT;
    log.step(format!(
        "calling DU login API {endpoint} ({})",
        if via_gateway {
            "ITS gateway — JSON body keyed `username`"
        } else {
            "DU Laravel — form body keyed `email`"
        }
    ));

    // Call external login API. `secret-key` goes on both branches: Laravel
    // rejects the call without it, the gateway ignores it.
    let request = client.post(&endpoint).header("secret-key", SSL_SECRET_KEY);
    let request = if via_gateway {
        request.json(&json!({ "username": req.username, "password": req.password }))
    } else {
        request.form(&[
            ("email", req.username.clone()),
            ("password", req.password.clone()),
        ])
    };
    let response = request.send().await;

    match response {
        Ok(resp) => {
            let status = resp.status();
            log.step(format!("DU login API responded {status}"));

            if status.is_success() {
                // Deserialize DU API response
                let api_json: Value = match resp.json().await {
                    Ok(json) => json,
                    Err(_) => {
                        log.step("DU response body was not valid JSON — rejecting");
                        return HttpResponse::InternalServerError().json("Invalid API response");
                    }
                };

                //println!("DU API response: {}", api_json);
                // The gateway nests the DU user under `user_data`, Laravel
                // under `user`. Both keys are accepted on both branches rather
                // than keyed off `via_gateway`: the two shapes cannot be
                // confused for each other, and a proxy that rewrapped the body
                // would otherwise 401 with the credentials perfectly good.
                if let Some(user_obj) = api_json
                    .get("user_data")
                    .or_else(|| api_json.get("user"))
                {
                    let username = user_obj["username"].as_str().unwrap_or("unknown").to_string();
                    let user_id = user_obj["user_id"].as_u64().unwrap_or(0);
                    let user_data = user_obj.clone();

                    // Choose the person_id that goes into the token's `sub`.
                    // For non-students the person_id is the employee id
                    // (`user.emp_id`, e.g. "2020111007"); students keep `user_id`.
                    // wow-attendance enroll/verify match this `sub` against the
                    // supplied person_id.
                    let user_role = user_obj["user_role"].as_str().unwrap_or("");
                    let emp_id = user_obj["emp_id"].as_str().unwrap_or("");
                    // A one-line summary of the fields the token decision is
                    // made from, so "why did this login get that person_id?"
                    // is answerable from the Steps section alone. The full DU
                    // user object still appears verbatim further down, in the
                    // backend response — this is the readable index into it.
                    log.step(format!(
                        "DU user resolved: username={username}, user_id={user_id}, \
                         emp_id={emp_id}, user_role={user_role}"
                    ));

                    let is_student = user_role.eq_ignore_ascii_case("student");
                    let person_id: u64 = if is_student {
                        user_id
                    } else {
                        user_obj["emp_id"]
                            .as_str()
                            .and_then(|s| s.trim().parse::<u64>().ok())
                            .unwrap_or(user_id)
                    };
                    log.step(format!(
                        "token subject chosen: person_id={person_id} (from {})",
                        if is_student {
                            "user_id — role is student"
                        } else if person_id == user_id {
                            "user_id — emp_id missing or non-numeric"
                        } else {
                            "emp_id — role is not student"
                        }
                    ));
                    // The log file is named after whatever id is set at Drop, so
                    // a successful login files under the same person_id the
                    // wow-attendance logs use — one id searches both folders.
                    log.set_id(&person_id.to_string());

                    // create your own internal JWT (local token)
                    let token = create_jwt(person_id);
                    log.step("local JWT issued — login OK");

                    return HttpResponse::Ok().json(LoginResponse {
                        token,
                        username,
                        user_data,
                    });
                }

                log.step("DU response carried neither `user_data` nor `user` — rejecting");
                HttpResponse::Unauthorized().json("Invalid user data from external API")
            } else {
                log.step("DU login API rejected the credentials");
                HttpResponse::Unauthorized().json("Login failed at external API")
            }
        }
        Err(err) => {
            log.step(format!("DU login API unreachable: {err}"));
            eprintln!("External login error: {:?}", err);
            HttpResponse::InternalServerError().json("Login service unavailable")
        }
    }
}
