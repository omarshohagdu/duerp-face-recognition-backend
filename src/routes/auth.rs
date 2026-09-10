use std::env;
use crate::models::auth::{LoginRequest, LoginResponse};
use crate::utils::jwt::create_jwt;
use actix_web::{post, web, HttpResponse, Responder};
use reqwest::Client;
use serde_json::Value;
use sqlx::PgPool;
use crate::utils::constants::{LOGIN_ENDPOINT, SSL_SECRET_KEY};

#[post("/login")]
pub async fn login(
    req: web::Json<LoginRequest>,
    db: web::Data<PgPool>,
) -> impl Responder {
    let client = Client::new();

    // Prepare form data for external API
    let form_data = [
        ("email", req.username.clone()),
        ("password", req.password.clone()),
    ];

    // Call external login API
    let response = client
        .post(env::var("SSL_API_ENDPOINT").unwrap() + LOGIN_ENDPOINT)
        .header("secret-key", SSL_SECRET_KEY)
        .form(&form_data)
        .send()
        .await;

    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                // Deserialize DU API response
                let api_json: Value = match resp.json().await {
                    Ok(json) => json,
                    Err(_) => {
                        return HttpResponse::InternalServerError()
                            .json("Invalid API response");
                    }
                };

                //println!("DU API response: {}", api_json);

                if let Some(user_obj) = api_json.get("user") {
                    let username = user_obj["username"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();

                    let user_id = user_obj["user_id"]
                        .as_u64()
                        .unwrap_or(0);

                    let user_data = user_obj.clone();

                    // Choose the person_id that goes into the token's sub.
                    // For non-students the person_id is the employee id
                    // (user.emp_id, e.g. "2020111007"); students keep user_id.
                    // wow-attendance enroll/verify match this sub against the
                    // supplied person_id.
                    let user_role = user_obj["user_role"]
                        .as_str()
                        .unwrap_or("");

                    let person_id: u64 =
                        if user_role.eq_ignore_ascii_case("student") {
                            user_id
                        } else {
                            user_obj["emp_id"]
                                .as_str()
                                .and_then(|s| s.trim().parse::<u64>().ok())
                                .unwrap_or(user_id)
                        };

                    // Create your own internal JWT (local token)
                    let token = create_jwt(person_id);

                    return HttpResponse::Ok().json(LoginResponse {
                        token,
                        username,
                        user_data,
                    });
                }

                HttpResponse::Unauthorized()
                    .json("Invalid user data from external API")
            } else {
                // Diagnostic information from the external DU API.
                // This helps identify why the external login is failing.
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();

                eprintln!(
                    "External login failed: status={}, body={}",
                    status, body
                );

                // Keep the response to the frontend generic.
                HttpResponse::Unauthorized()
                    .json("Login failed at external API")
            }
        }

        Err(err) => {
            eprintln!("External login error: {:?}", err);

            HttpResponse::InternalServerError()
                .json("Login service unavailable")
        }
    }
}

// use std::env;
// use crate::models::auth::{LoginRequest, LoginResponse};
// use crate::routes::wow_attendance::{full_path, log_local_response};
// use crate::utils::jwt::create_jwt;
// use crate::utils::step_logger::StepLogger;
// use actix_web::{post, web, HttpRequest, HttpResponse};
// use reqwest::Client;
// use serde_json::{json, Value};
// use sqlx::PgPool;
// use crate::utils::constants::{LOGIN_ENDPOINT, SSL_SECRET_KEY};
//
// /// Folder the per-login step logs are written to, `./uploads/login` by default
// /// and overridable with `LOGIN_LOG_DIR`.
// ///
// /// Deliberately NOT `WOW_LOG_DIR`: every sign-in writes a file here, so mixing
// /// them into the attendance stream would bury the face calls that folder exists
// /// to explain. Keeping them apart also means the retention decision can differ —
// /// logins are high-volume and neither folder is rotated.
// ///
// /// SECURITY: like the attendance logs, this sits INSIDE the folder `/uploads`
// /// is served from, so the 404 block main.rs registers on `/uploads/login`
// /// before that static route is load-bearing — these files carry usernames,
// /// client IPs and employee ids. Move this path out of `<uploads>` and the block
// /// no longer covers it: re-check exposure.
// fn login_log_dir() -> String {
//     env::var("LOGIN_LOG_DIR")
//         .ok()
//         .filter(|s| !s.trim().is_empty())
//         .unwrap_or_else(|| "./uploads/login".to_string())
// }
//
// #[post("/login")]
// pub async fn login(
//     http_req: HttpRequest,
//     req: web::Json<LoginRequest>,
//     db: web::Data<PgPool>,
// ) -> HttpResponse {
//     let log = StepLogger::new_in("login", &login_log_dir());
//     log.set_endpoint(http_req.method().as_str(), &full_path(&http_req));
//     // The submitted credentials are the whole input here. `redact` masks values
//     // by key name, so the file records THAT a password was sent and never what
//     // it was.
//     log.params(
//         "json",
//         &json!({ "username": req.username, "password": req.password }),
//     );
//     // The only id known before DU answers. Replaced by the token subject on
//     // success, so a failed attempt is still filed under something searchable
//     // rather than `unknown`.
//     log.set_id(&req.username);
//
//     // Split so the logger outlives every early return in the handler body: it
//     // flushes on Drop, and `log_local_response` records whichever branch won.
//     let resp = login_inner(&log, &http_req, req, db).await;
//     log_local_response(&log, resp).await
// }
//
// async fn login_inner(
//     log: &StepLogger,
//     http_req: &HttpRequest,
//     req: web::Json<LoginRequest>,
//     _db: web::Data<PgPool>,
// ) -> HttpResponse {
//     let client_ip = http_req
//         .connection_info()
//         .realip_remote_addr()
//         .unwrap_or("")
//         .to_string();
//     log.step(format!("request received (client_ip={client_ip})"));
//
//     let client = Client::new();
//
//     // Prepare form data for external API
//     let form_data = [
//         ("email", req.username.clone()),
//         ("password", req.password.clone()),
//     ];
//
//     let endpoint = env::var("SSL_API_ENDPOINT").unwrap() + LOGIN_ENDPOINT;
//     log.step(format!("calling DU login API {endpoint}"));
//
//     // Call external login API
//     let response = client
//         .post(&endpoint)
//         .header("secret-key", SSL_SECRET_KEY)
//         .form(&form_data)
//         .send()
//         .await;
//
//     match response {
//         Ok(resp) => {
//             let status = resp.status();
//             log.step(format!("DU login API responded {status}"));
//
//             if status.is_success() {
//                 // Deserialize DU API response
//                 let api_json: Value = match resp.json().await {
//                     Ok(json) => json,
//                     Err(_) => {
//                         log.step("DU response body was not valid JSON — rejecting");
//                         return HttpResponse::InternalServerError().json("Invalid API response");
//                     }
//                 };
//
//                 //println!("DU API response: {}", api_json);
//                 if let Some(user_obj) = api_json.get("user") {
//                     let username = user_obj["username"].as_str().unwrap_or("unknown").to_string();
//                     let user_id = user_obj["user_id"].as_u64().unwrap_or(0);
//                     let user_data = user_obj.clone();
//
//                     // Choose the person_id that goes into the token's `sub`.
//                     // For non-students the person_id is the employee id
//                     // (`user.emp_id`, e.g. "2020111007"); students keep `user_id`.
//                     // wow-attendance enroll/verify match this `sub` against the
//                     // supplied person_id.
//                     let user_role = user_obj["user_role"].as_str().unwrap_or("");
//                     let emp_id = user_obj["emp_id"].as_str().unwrap_or("");
//                     // A one-line summary of the fields the token decision is
//                     // made from, so "why did this login get that person_id?"
//                     // is answerable from the Steps section alone. The full DU
//                     // user object still appears verbatim further down, in the
//                     // backend response — this is the readable index into it.
//                     log.step(format!(
//                         "DU user resolved: username={username}, user_id={user_id}, \
//                          emp_id={emp_id}, user_role={user_role}"
//                     ));
//
//                     let is_student = user_role.eq_ignore_ascii_case("student");
//                     let person_id: u64 = if is_student {
//                         user_id
//                     } else {
//                         user_obj["emp_id"]
//                             .as_str()
//                             .and_then(|s| s.trim().parse::<u64>().ok())
//                             .unwrap_or(user_id)
//                     };
//                     log.step(format!(
//                         "token subject chosen: person_id={person_id} (from {})",
//                         if is_student {
//                             "user_id — role is student"
//                         } else if person_id == user_id {
//                             "user_id — emp_id missing or non-numeric"
//                         } else {
//                             "emp_id — role is not student"
//                         }
//                     ));
//                     // The log file is named after whatever id is set at Drop, so
//                     // a successful login files under the same person_id the
//                     // wow-attendance logs use — one id searches both folders.
//                     log.set_id(&person_id.to_string());
//
//                     // create your own internal JWT (local token)
//                     let token = create_jwt(person_id);
//                     log.step("local JWT issued — login OK");
//
//                     return HttpResponse::Ok().json(LoginResponse {
//                         token,
//                         username,
//                         user_data,
//                     });
//                 }
//
//                 log.step("DU response carried no `user` object — rejecting");
//                 HttpResponse::Unauthorized().json("Invalid user data from external API")
//             } else {
//                 log.step("DU login API rejected the credentials");
//                 HttpResponse::Unauthorized().json("Login failed at external API")
//             }
//         }
//         Err(err) => {
//             log.step(format!("DU login API unreachable: {err}"));
//             eprintln!("External login error: {:?}", err);
//             HttpResponse::InternalServerError().json("Login service unavailable")
//         }
//     }
// }
