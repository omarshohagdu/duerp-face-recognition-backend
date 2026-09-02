use actix_multipart::Multipart;
use actix_web::{post, web, HttpResponse};
use actix_web_httpauth::extractors::bearer::BearerAuth;
use base64::Engine as _;
use futures_util::StreamExt;
use jsonwebtoken::{decode, errors::ErrorKind, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::utils::constants::{GET_BY_EMPLOYEE_ID_ENDPOINT, SSL_SECRET_KEY};
use crate::utils::jwt::Claims;
use crate::utils::step_logger::{query_to_json, StepLogger};

// Shared HTTP client for AI-platform calls: reuses connections (pool + TLS)
// across requests and applies a timeout so a hung upstream can't block forever.
static AI_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

// Shared HTTP client for DU backend calls (getByEmployeeId). Kept separate from
// AI_CLIENT so the identity lookup, which sits in front of every enroll/verify,
// gets a tighter timeout than the image uploads.
static DU_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

// ---------------------------------------------------------------------
// Query param structs.
//
// `id` / `id_type` are documented as query params, but several HTTP clients
// drop the query string on a multipart POST. Fields are therefore Optional so
// extraction never fails; handlers fall back to the multipart body and then
// validate that the required values were supplied somewhere.
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EnrollQuery {
    pub id: Option<String>,
    pub id_type: Option<String>, // "Student" | "Employee"
}

#[derive(Deserialize)]
pub struct VerifyQuery {
    pub id: Option<String>,
    pub id_type: Option<String>,
}

#[derive(Deserialize)]
pub struct EnrolledListQuery {
    pub id_type: Option<String>,
}

#[derive(Deserialize)]
pub struct CheckEnrolledQuery {
    pub person_id: Option<String>,
}

#[derive(Deserialize)]
pub struct RecordsByDateQuery {
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub id_type: Option<String>,
    pub page: Option<i32>,
    pub limit: Option<i32>,
}

#[derive(Deserialize)]
pub struct RecordsByPersonQuery {
    pub person_id: Option<String>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub page: Option<i32>,
    pub limit: Option<i32>,
}

// Attendance reports accept dates as `YYYY-MM-DD`.
fn parse_report_date(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

// Extract the required attendance token from the `Authorization: Bearer <token>`
// header. Returns Err(401 response) when the header is missing or the token is
// empty. The token is no longer accepted as a multipart body field.
fn require_bearer_token(req: &actix_web::HttpRequest) -> Result<String, HttpResponse> {
    req.headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            HttpResponse::Unauthorized().json(json!({
                "success": false,
                "message": "Missing or empty token. Send it as an `Authorization: Bearer <token>` header."
            }))
        })
}

// Which kind of bearer token the caller sent.
#[derive(PartialEq)]
enum TokenSource {
    // Minted by this service's `POST /login`. Signature and expiry are both
    // verified, and `sub` is the person id enroll/verify take as `id`: the
    // 10-digit `emp_id` for staff, the DU `user_id` for students.
    Ours,
    // DU's own Laravel token (`local.duwebadmin.com/api/login`). Signed with DU's
    // secret, which this service does not hold, so its signature CANNOT be
    // verified here — a well-formed forged token passes. Its `sub` is the DU
    // `user_id` (e.g. 45320), which `getByEmployeeId` rejects outright (422,
    // "must be 10 digits"), so a caller on this token can never be confirmed
    // against DU and always lands on the `id_type` fallback.
    //
    // Accepted only for the migration window — see `du_token_accepted()`.
    LegacyDu,
}

struct TokenUser {
    person_id: i64,
    source: TokenSource,
}

// Is DU's own token still accepted? True unless `WOW_ACCEPT_DU_TOKEN` is set to
// a falsy value, so no existing client breaks on deploy. Flip it to `false` once
// every client has moved to `POST /login` — the step logs name each caller still
// arriving on a legacy token, so they can be chased down first.
fn du_token_accepted() -> bool {
    !matches!(
        std::env::var("WOW_ACCEPT_DU_TOKEN")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "false" | "0" | "no" | "off"
    )
}

// Extract the person id (`sub`) from the bearer token.
//
// Preferred path: the token this service minted in `POST /login`
// (`utils::jwt::create_jwt`), signed with `JWT_SECRET` — signature and expiry are
// both verified.
//
// Transition path: a token whose signature is not ours is retried as DU's Laravel
// token, which can only be decoded, not verified (see `TokenSource::LegacyDu`).
// An expired one is still rejected. Callers must treat this source as weaker —
// it carries no proof the token is genuine.
fn user_from_token(token: &str) -> Result<TokenUser, HttpResponse> {
    let unauth = |msg: &str| {
        HttpResponse::Unauthorized().json(json!({ "success": false, "message": msg }))
    };

    let secret = match std::env::var("JWT_SECRET") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            return Err(HttpResponse::InternalServerError().json(json!({
                "success": false,
                "message": "JWT_SECRET not configured"
            })))
        }
    };

    match decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    ) {
        Ok(data) => Ok(TokenUser {
            person_id: data.claims.sub as i64,
            source: TokenSource::Ours,
        }),
        // Signed by someone else — the only case where a DU token can still be
        // meant. Everything else (expired, malformed, wrong algorithm) is a
        // straight rejection: `jsonwebtoken` checks the signature before the
        // claims, so an expired token here is one of ours and stays expired.
        Err(e) if *e.kind() == ErrorKind::InvalidSignature => {
            if !du_token_accepted() {
                return Err(unauth(
                    "Token signature is invalid — the token was not issued by this service. \
                     Log in through this service's `POST /login` and send the token it returns.",
                ));
            }
            let person_id = person_id_from_du_token(token)?;
            Ok(TokenUser {
                person_id,
                source: TokenSource::LegacyDu,
            })
        }
        Err(e) if *e.kind() == ErrorKind::ExpiredSignature => Err(unauth("Token has expired")),
        Err(_) => Err(unauth("Invalid token")),
    }
}

// One step-log line naming which token the caller arrived on. A legacy line is
// the signal that this caller has not migrated to `POST /login` yet — grep the
// logs for it to find who is left before turning `WOW_ACCEPT_DU_TOKEN` off.
fn token_step(user: &TokenUser) -> String {
    match user.source {
        TokenSource::Ours => format!(
            "token validation OK — /login token, signature verified (token user id={})",
            user.person_id
        ),
        TokenSource::LegacyDu => format!(
            "token validation OK — LEGACY DU token, signature NOT verified (token user id={}). \
             This caller must migrate to this service's POST /login.",
            user.person_id
        ),
    }
}

// Decode DU's Laravel token WITHOUT verifying its signature (DU's secret is not
// held here) and return its `sub`. Expiry is still enforced. The `/ext-api` scope
// is guarded by `ExtAuthMiddleware` (X-App-Id/X-App-Password + IP allow-list),
// which is the only real protection behind a token from this source.
fn person_id_from_du_token(token: &str) -> Result<i64, HttpResponse> {
    let unauth = |msg: String| {
        HttpResponse::Unauthorized().json(json!({ "success": false, "message": msg }))
    };

    // A JWT is `header.payload.signature`; decode the middle (payload) segment.
    let payload_b64 = token
        .split('.')
        .nth(1)
        .ok_or_else(|| unauth("Malformed token".into()))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| unauth(format!("Malformed token payload: {e}")))?;
    let claims: Value = serde_json::from_slice(&decoded)
        .map_err(|e| unauth(format!("Malformed token payload: {e}")))?;

    // Reject an expired token (best-effort — there is no signature guarantee).
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
        if chrono::Utc::now().timestamp() >= exp {
            return Err(unauth("Token has expired".into()));
        }
    }

    // `sub` may be a JSON number or a numeric string.
    let sub = claims.get("sub");
    sub.and_then(|v| v.as_i64())
        .or_else(|| sub.and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()))
        .ok_or_else(|| unauth("Token is missing a numeric `sub` claim".into()))
}

// What DU's getByEmployeeId had to say about a person id.
enum DuLookup {
    // DU confirmed the id belongs to an employee.
    Employee(Value),
    // DU was asked and answered that no such employee exists.
    NotEmployee,
    // DU was never asked: the id is not shaped like an employee id, so DU would
    // have rejected it outright. Distinct from `NotEmployee` — this is "unknown",
    // not "no", and must never be logged as DU having denied the person.
    NotAnEmployeeId,
}

// Ask DU whether `person_id` belongs to an employee.
//
//   POST {SSL_API_ENDPOINT}getByEmployeeId   header: secret-key
//   form: employee_id=<10 digits>
//   200 { "status": "success", "data": { employee_id, employee_name_en, ... } }
//   404 { "status": "error", "message": "Employee not found." }
//   422 { "status": "error", ... }   — id is not 10 digits
//
// Err = the lookup itself failed (DU unreachable / unexpected status), so the
// answer is unknown and the caller must not read it as "not an employee".
async fn du_get_employee(person_id: &str) -> Result<DuLookup, String> {
    // DU only accepts a 10-digit employee id (a student carries the shorter DU
    // `user_id`), and answers anything else with a 422. Skip the round-trip for
    // an id it would reject outright.
    let person_id = person_id.trim();
    if person_id.len() != 10 || !person_id.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(DuLookup::NotAnEmployeeId);
    }

    let base = match std::env::var("SSL_API_ENDPOINT") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Err("SSL_API_ENDPOINT not configured".into()),
    };
    let url = format!("{}/{}", base.trim_end_matches('/'), GET_BY_EMPLOYEE_ID_ENDPOINT);

    let resp = DU_CLIENT
        .post(&url)
        .header("secret-key", SSL_SECRET_KEY)
        .form(&[("employee_id", person_id)])
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON from {url} (HTTP {status}): {e}"))?;

    if status.is_success() && body.get("status").and_then(|v| v.as_str()) == Some("success") {
        return Ok(DuLookup::Employee(
            body.get("data").cloned().unwrap_or(Value::Null),
        ));
    }
    // 404 "Employee not found" and 422 (failed DU's own id validation) both mean
    // this id is not an employee; anything else is a genuine lookup failure.
    if status.as_u16() == 404 || status.as_u16() == 422 {
        return Ok(DuLookup::NotEmployee);
    }
    Err(format!("HTTP {status} from {url}: {body}"))
}

// What DU could tell us about the `id` a request is acting on.
//
// TODO(student lookup): DU has no student-by-id endpoint yet, so a non-employee
// cannot be positively identified as a Student here; that is what keeps the
// `X-Id-Type` / `WOW_IDTYPE_FALLBACK` fallback alive. Once the student endpoint
// exists, resolve "Student" from it and drop the fallback.
struct Identified {
    // The `id` belongs to the token holder, so the request may proceed.
    owned: bool,
    // DU confirmed `id` is an employee — `id_type` follows from that, no fallback.
    is_employee: bool,
}

// Decide whether `id` belongs to the holder of the token, and what kind of person
// it is — one `getByEmployeeId` call answers both.
//
// An `id` is the token holder's if EITHER holds:
//
//   * `sub` IS the id. The token this service issues carries the person id
//     directly (the 10-digit `emp_id` for staff, the DU `user_id` for students).
//
//   * DU says employee `id` has `user_id == sub`. `getByEmployeeId` returns the
//     employee's `user_id`, which is exactly what DU's own Laravel token carries
//     in `sub`. The two ids look nothing alike (`45320` vs `2020111007`) — DU is
//     what links them. Without this, a caller on a DU token could never enroll
//     under their own employee id.
//
// A DU lookup that fails leaves only the direct match: authorization fails closed
// (a legacy-token caller cannot enroll by `emp_id` while DU is down), but the
// `id_type` fallback still keeps a plain self-enroll working.
async fn du_identify(id: &str, token_user_id: i64, log: &StepLogger) -> Identified {
    let direct = id.trim() == token_user_id.to_string();

    match du_get_employee(id).await {
        Ok(DuLookup::Employee(data)) => {
            let du_user_id = data.get("user_id").and_then(|v| v.as_i64());
            let linked = du_user_id == Some(token_user_id);
            match du_user_id {
                Some(uid) if linked => log.step(format!(
                    "DU getByEmployeeId: {id} is an employee whose user_id={uid} — the token holder"
                )),
                Some(uid) => log.step(format!(
                    "DU getByEmployeeId: {id} is an employee, but its user_id={uid} \
                     is not the token holder ({token_user_id})"
                )),
                None => log.step(format!(
                    "DU getByEmployeeId: {id} confirmed as an employee, but the record carries \
                     no user_id — cannot link it to the token holder"
                )),
            }
            Identified {
                owned: direct || linked,
                is_employee: true,
            }
        }
        Ok(DuLookup::NotEmployee) => {
            log.step(format!(
                "DU getByEmployeeId: {id} is not an employee (DU returned not-found)"
            ));
            Identified {
                owned: direct,
                is_employee: false,
            }
        }
        Ok(DuLookup::NotAnEmployeeId) => {
            log.step(format!(
                "DU getByEmployeeId NOT called — {id} is not a 10-digit employee id \
                 (a student's id); DU cannot confirm either way"
            ));
            Identified {
                owned: direct,
                is_employee: false,
            }
        }
        Err(e) => {
            eprintln!("DU getByEmployeeId lookup failed for {id}: {e}");
            log.step(format!(
                "DU getByEmployeeId lookup FAILED ({e}) — falling back to a direct token/id match"
            ));
            Identified {
                owned: direct,
                is_employee: false,
            }
        }
    }
}

// Fallback `id_type` for a person DU could not confirm as an employee: the
// `X-Id-Type` request header, then the `WOW_IDTYPE_FALLBACK` env default.
// Returns None when neither yields a recognized value.
fn header_or_env_id_type(req: &actix_web::HttpRequest) -> Option<String> {
    let raw = req
        .headers()
        .get("X-Id-Type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("WOW_IDTYPE_FALLBACK").ok())
        .unwrap_or_default();
    normalize_id_type(&raw)
}

// Record one token/face-ownership mismatch to the audit table. Best-effort: a
// failure here (e.g. table not yet migrated) is logged but never blocks the
// request, which is already returning 401.
//
// `ai_recognized_id` is the identifier the request acted on (verify: the AI
// match; enroll: the id being enrolled). `ai_requested_id` (the AI platform's
// request_id) and `ai_similarity` come from the AI response and are None on
// flows that never called the AI (enroll rejects before any AI call).
async fn record_token_mismatch(
    db: &PgPool,
    action: &str,
    ai_recognized_id: &str,
    requested_user_id: i64,
    ai_requested_id: Option<&str>,
    ai_similarity: Option<f64>,
    log: &StepLogger,
) {
    let res = sqlx::query(
        "INSERT INTO ictcell.wow_attendance_token_mismatch_record \
             (action, ai_recognized_id, requested_user_id, ai_requested_id, ai_similarity) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(action)
    .bind(ai_recognized_id)
    .bind(requested_user_id.to_string())
    .bind(ai_requested_id)
    .bind(ai_similarity)
    .execute(db)
    .await;
    match res {
        Ok(_) => log.step("token mismatch recorded (wow_attendance_token_mismatch_record)"),
        Err(e) => {
            eprintln!("failed to record token mismatch: {e}");
            log.step(format!("WARN: could not record token mismatch: {e}"));
        }
    }
}

// Pull the device's GPS position out of the `device_info` JSON that
// enroll/verify already accept as a multipart field.
//
// Accepts either a number or a numeric string for each value, and the common
// key spellings phones send (`device_lat` / `lat` / `latitude`), so a client
// that already reports coordinates in `device_info` needs no change. Returns
// None unless BOTH coordinates are present and in range.
fn device_coords(device: Option<&Value>) -> Option<(f64, f64)> {
    let device = device?;
    let num = |keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| {
            let v = device.get(*k)?;
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
    };

    let lat = num(&["device_lat", "lat", "latitude"])?;
    let long = num(&["device_long", "long", "lng", "longitude"])?;
    // A phone that failed to get a fix often reports 0/0 (off West Africa);
    // treat that as "no fix" rather than a real position.
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&long) {
        return None;
    }
    if lat == 0.0 && long == 0.0 {
        return None;
    }
    Some((lat, long))
}

// Base upload directory, configurable via WOW_UPLOAD_DIR. Defaults to a path
// relative to the process working directory so it works both locally and in
// the container (set WOW_UPLOAD_DIR=/app/uploads/wow_attendance to override).
fn upload_base() -> String {
    std::env::var("WOW_UPLOAD_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "./uploads/wow_attendance".to_string())
}

fn enrolled_dir() -> String {
    format!("{}/enrolled/", upload_base().trim_end_matches('/'))
}

fn live_dir() -> String {
    format!("{}/live/", upload_base().trim_end_matches('/'))
}

// The `/uploads/...` URL a saved file can be opened at, or None when the file
// landed outside the folder this service serves.
//
// Recorded next to the filesystem path in every step log: the fs path is what
// an operator needs on the box, this is what an admin needs from the log viewer,
// and after the 2026-08-19 move they are no longer trivially derivable from one
// another (face images live under duerp-attendance/uploads, served at :8083).
fn browsable_path(fs_path: &str) -> Option<String> {
    let serve_dir = std::env::var("WOW_UPLOADS_SERVE_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "./uploads".to_string());
    let serve = serve_dir.trim_end_matches('/');

    let rest = match fs_path.strip_prefix(serve) {
        Some(rest) => rest.trim_start_matches('/').to_string(),
        // Relative (`./uploads/...`) or container (`/app/uploads/...`) paths
        // don't share the configured prefix; take everything from the last
        // `/uploads/` segment instead, which is what the URL exposes anyway.
        None => match fs_path.find("/uploads/") {
            Some(i) => return Some(url_escape(&fs_path[i..])),
            None => return None,
        },
    };
    Some(url_escape(&format!("/uploads/{rest}")))
}

// Escape only what actually breaks a pasted URL. Uploaded names routinely
// contain spaces ("WhatsApp Image 2026-07-09 at 12.54.26 PM.jpeg"), which would
// otherwise truncate the link when copied out of the log viewer.
fn url_escape(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '"' => "%22".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            c => c.to_string(),
        })
        .collect()
}

// The public origin this service is reached at, used to turn a saved image's
// served path into a URL that opens straight from the log viewer.
//
// `WOW_PUBLIC_BASE_URL` wins because behind a reverse proxy the request's own
// Host is the internal one (`127.0.0.1:8083`), which no admin can open. Without
// it, fall back to how this request arrived — `connection_info` honours
// X-Forwarded-Proto/Host, so a correctly configured proxy still yields the
// public origin.
fn public_base_url(req: &actix_web::HttpRequest) -> String {
    if let Ok(v) = std::env::var("WOW_PUBLIC_BASE_URL") {
        let v = v.trim().trim_end_matches('/');
        if !v.is_empty() {
            return v.to_string();
        }
    }
    let info = req.connection_info();
    format!("{}://{}", info.scheme(), info.host())
}

// The path as called, query string included — what a reader needs to reproduce
// the request. `HttpRequest::uri()` already carries both, but the query is
// optional, so it is rebuilt rather than unwrapped.
fn full_path(req: &actix_web::HttpRequest) -> String {
    let path = req.path();
    match req.query_string() {
        "" => path.to_string(),
        q => format!("{path}?{q}"),
    }
}

// Record what this service answered, then hand the response back untouched.
//
// Handlers below have many early returns; wrapping the whole handler is what
// makes every one of them land in the log, rather than only the success path.
// The body is buffered to read it, then rebuilt byte-for-byte with the original
// status and content type.
async fn log_local_response(log: &StepLogger, resp: HttpResponse) -> HttpResponse {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(actix_web::http::header::CONTENT_TYPE)
        .cloned();
    let bytes = actix_web::body::to_bytes(resp.into_body())
        .await
        .unwrap_or_default();

    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => log.backend_response(status.as_u16(), &v),
        Err(_) => log.backend_response_text(status.as_u16(), &String::from_utf8_lossy(&bytes)),
    }

    let mut builder = HttpResponse::build(status);
    if let Some(ct) = content_type {
        builder.insert_header((actix_web::http::header::CONTENT_TYPE, ct));
    }
    builder.body(bytes)
}

// Maximum accepted size for a single uploaded image, in bytes. Configurable via
// WOW_MAX_IMAGE_MB (megabytes, may be fractional); defaults to 5 MB.
fn max_image_bytes() -> usize {
    std::env::var("WOW_MAX_IMAGE_MB")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|mb| *mb > 0.0)
        .map(|mb| (mb * 1024.0 * 1024.0) as usize)
        .unwrap_or(5 * 1024 * 1024)
}

// Human-friendly MB figure for the configured limit, for error messages.
fn max_image_mb_display() -> String {
    let mb = max_image_bytes() as f64 / (1024.0 * 1024.0);
    // Show whole numbers without a trailing ".0" (e.g. "5 MB", "1.5 MB").
    if (mb.fract()).abs() < f64::EPSILON {
        format!("{} MB", mb as u64)
    } else {
        format!("{mb:.1} MB")
    }
}

// Target size for the live image actually sent to the AI platform, in bytes.
// Kept a safety margin *below* the AI hard limit (`max_image_bytes`) so
// re-encoding variance never pushes the multipart body back over the limit and
// triggers the platform's "image too large" rejection. Configurable via
// WOW_AI_TARGET_MB; defaults to ~98% of the AI limit (≈4.9 MB for a 5 MB limit).
fn ai_target_bytes() -> usize {
    let limit = max_image_bytes();
    std::env::var("WOW_AI_TARGET_MB")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|mb| *mb > 0.0)
        .map(|mb| (mb * 1024.0 * 1024.0) as usize)
        .unwrap_or((limit as f64 * 0.98) as usize)
        // Never at or above the hard limit — always leave headroom.
        .min(limit.saturating_sub(1))
}

// Maximum accepted upload size, in bytes. Uploads between the AI limit
// (`max_image_bytes`) and this ceiling are compressed down before being sent
// to the AI platform, so this only guards against unbounded uploads — it is
// deliberately larger than the AI limit. Configurable via WOW_MAX_UPLOAD_MB
// (megabytes, may be fractional); defaults to 25 MB.
fn max_upload_bytes() -> usize {
    std::env::var("WOW_MAX_UPLOAD_MB")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|mb| *mb > 0.0)
        .map(|mb| (mb * 1024.0 * 1024.0) as usize)
        // Never below the AI limit — the ceiling must leave room to accept
        // images that will then be compressed down.
        .map(|b| b.max(max_image_bytes()))
        .unwrap_or(25 * 1024 * 1024)
}

// Human-friendly MB figure for the upload ceiling, for error messages.
fn max_upload_mb_display() -> String {
    let mb = max_upload_bytes() as f64 / (1024.0 * 1024.0);
    if (mb.fract()).abs() < f64::EPSILON {
        format!("{} MB", mb as u64)
    } else {
        format!("{mb:.1} MB")
    }
}

// ---------------------------------------------------------------------
// Face recognition — delegated to the AI platform's /recognize endpoint.
//
// Sends the live image and returns the platform's JSON response:
//   { recognized: bool, name, identifier, id_type, error }
// The platform performs the 1:N identification itself.
// ---------------------------------------------------------------------
// Map the AI platform's id_type (e.g. "EMPLOYEE"/"STUDENT") back to the
// canonical casing used by the database ("Employee"/"Student").
fn normalize_id_type(raw: &str) -> Option<String> {
    match raw.trim().to_lowercase().as_str() {
        "student" => Some("Student".to_string()),
        "employee" | "faculty" | "teacher" => Some("Employee".to_string()),
        _ => None,
    }
}

async fn ai_recognize(live_image: &str, log: &StepLogger) -> Result<Value, String> {
    let base = match std::env::var("WOW_AI_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Err("WOW_AI_BASE_URL not configured".into()),
    };
    let api_key = std::env::var("WOW_AI_API_KEY").unwrap_or_default();
    let url = format!("{}/recognize", base.trim_end_matches('/'));

    let bytes = tokio::fs::read(live_image)
        .await
        .map_err(|e| format!("read {live_image}: {e}"))?;
    // Only reduce when the image is actually over the AI platform's hard limit
    // (default 5 MB); anything at or under it is fine and is forwarded untouched.
    // When it IS over, shrink/re-encode down to a target a small margin below the
    // limit (default 4.9 MB) so re-encoding variance never lands back over.
    let original_len = bytes.len();
    let limit = max_image_bytes();
    let target = ai_target_bytes();
    // Log the TRUE format (from magic bytes) so oversized-image problems are
    // diagnosable from the log alone — a large JPEG can be shrunk here, HEIC
    // cannot (the image decoder can't read it).
    let detected_fmt = detect_image_format(&bytes[..bytes.len().min(16)]);
    let (bytes, reencoded) = if original_len > limit {
        tokio::task::spawn_blocking(move || compress_to_fit(bytes, target))
            .await
            .map_err(|e| format!("image resize task failed: {e}"))?
    } else {
        (bytes, false)
    };
    if reencoded {
        eprintln!(
            "AI recognize: {detected_fmt} image over limit ({original_len} > {limit}); compressed to {} bytes (target {target}) for {live_image}",
            bytes.len()
        );
    } else if original_len > limit {
        // Could not be decoded/compressed (HEIC is the usual culprit — the
        // image crate only reads JPEG/PNG/WEBP). The AI platform will reject it.
        // Log the format loudly so the cause is unambiguous.
        eprintln!(
            "AI recognize: {detected_fmt} live image is {original_len} bytes (over limit {limit}) but could NOT be compressed \
             (format not decodable by server — HEIC/HEIF must be converted to JPEG on the client); forwarding as-is for {live_image}"
        );
    }
    // Sanitized ASCII filename + real image MIME so the AI platform's multipart
    // parser never trips over mobile-supplied filenames (see ai_enroll). A
    // re-encoded image is always JPEG regardless of the original extension.
    let (ext, mime) = if reencoded {
        ("jpg".to_string(), "image/jpeg")
    } else {
        let ext = Path::new(live_image)
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| e.to_ascii_lowercase())
            .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_else(|| "jpg".to_string());
        let mime = match ext.as_str() {
            "png" => "image/png",
            "webp" => "image/webp",
            "heic" | "heif" => "image/heic",
            "bmp" => "image/bmp",
            _ => "image/jpeg",
        };
        (ext, mime)
    };
    let filename = format!("live.{ext}");
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename)
        .mime_str(mime)
        .map_err(|e| e.to_string())?;

    let mut form = reqwest::multipart::Form::new().part("image", part);
    // Optional recognition model, if configured (e.g. "ensemble").
    if let Ok(m) = std::env::var("WOW_AI_MODEL_NAME") {
        if !m.trim().is_empty() {
            form = form.text("model_name", m);
        }
    }
    // Optional recognition threshold, if configured.
    if let Ok(t) = std::env::var("WOW_AI_THRESHOLD") {
        if !t.trim().is_empty() {
            form = form.text("threshold", t);
        }
    }

    let resp = AI_CLIENT
        .post(&url)
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("parse response: {e}"))?;
    // Logged before the status check so a REJECTED recognition is recorded in
    // full too — that reply is exactly the one worth reading afterwards.
    log.ai_response("/recognize", status.as_u16(), &body);

    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("recognize failed with status {status}"));
        return Err(err);
    }
    Ok(body)
}

// ---------------------------------------------------------------------
// AI platform — face learning / recognition service.
//
// Configured via env:
//   WOW_AI_BASE_URL   e.g. http://103.221.255.12
//   WOW_AI_API_KEY    sent as the `x-api-key` header
//
// Enroll forwards the captured images to `{base}/enroll` so the platform
// can learn the person's face. Returns Ok(()) on success, Err(msg) on
// failure, or Ok(()) (with a log line) when the platform is not configured.
// ---------------------------------------------------------------------

// Look up a person on the AI platform by `identifier` (+ `id_type`) and delete
// them, which removes all of their stored face embeddings. Used before a
// re-enroll so the platform keeps only the freshly supplied images instead of
// accumulating stale embeddings (the platform appends on every /enroll).
// Returns the number of person records removed.
async fn ai_delete_person(
    base: &str,
    api_key: &str,
    identifier: &str,
    id_type: &str,
) -> Result<usize, String> {
    let client = &*AI_CLIENT;
    let list_url = format!("{}/persons", base.trim_end_matches('/'));

    let resp = client
        .get(&list_url)
        .header("x-api-key", api_key)
        .send()
        .await
        .map_err(|e| format!("list persons failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("parse persons response: {e}"))?;
    if !status.is_success() {
        return Err(format!("list persons failed with status {status}"));
    }

    let persons = body
        .get("persons")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut deleted = 0usize;
    for p in persons {
        let id_match = p.get("identifier").and_then(|v| v.as_str()) == Some(identifier);
        let type_match = p
            .get("id_type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case(id_type))
            .unwrap_or(false);
        if !(id_match && type_match) {
            continue;
        }
        let person_id = match p.get("id").and_then(|v| v.as_str()) {
            Some(pid) => pid,
            None => continue,
        };

        let del_url = format!("{}/persons/{}", base.trim_end_matches('/'), person_id);
        let del = client
            .delete(&del_url)
            .header("x-api-key", api_key)
            .send()
            .await
            .map_err(|e| format!("delete person {person_id} failed: {e}"))?;
        if !del.status().is_success() {
            return Err(format!(
                "delete person {person_id} failed with status {}",
                del.status()
            ));
        }
        deleted += 1;
    }

    Ok(deleted)
}

// Identify an image's true format from its first bytes (magic number),
// independent of the filename extension the client claimed. Used to log what a
// phone actually uploads — Android frequently captures HEIC, which the AI
// platform can't read.
fn detect_image_format(head: &[u8]) -> &'static str {
    if head.len() >= 3 && head[0] == 0xFF && head[1] == 0xD8 && head[2] == 0xFF {
        "JPEG"
    } else if head.len() >= 8 && head[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        "PNG"
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // ISO-BMFF container: HEIC/HEIF/AVIF — the brand is at bytes 8..12.
        match &head[8..12] {
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" => "HEIC",
            b"mif1" | b"msf1" => "HEIF",
            b"avif" => "AVIF",
            _ => "ISO-BMFF (HEIC/HEIF family)",
        }
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        "WEBP"
    } else if head.len() >= 2 && &head[0..2] == b"BM" {
        "BMP"
    } else if head.is_empty() {
        "EMPTY (0 bytes)"
    } else {
        "UNKNOWN/non-image"
    }
}

// True when the leading bytes match a known image format. Used to reject
// non-image uploads (PDFs, videos, arbitrary files) before they are forwarded
// to the AI platform. Relies on the magic number, not the client-supplied
// filename/Content-Type, so it can't be fooled by a renamed file.
fn is_supported_image(head: &[u8]) -> bool {
    !matches!(detect_image_format(head), "EMPTY (0 bytes)" | "UNKNOWN/non-image")
}

// Downscale an image so its largest side is at most `max_dim` px and re-encode
// it as JPEG (quality 85). Phone cameras produce multi-megapixel, multi-MB
// photos; the AI platform rejects oversized multipart bodies with a parse
// error. HEIC/HEIF input (which the image crate can't read) is first converted
// via heic_to_dynimage. Returns (bytes, true) with the re-encoded JPEG on
// success, or (original_bytes, false) when the input still can't be decoded so
// the caller forwards it untouched. CPU-bound — run via spawn_blocking.
fn downscale_jpeg(bytes: Vec<u8>, max_dim: u32) -> (Vec<u8>, bool) {
    use image::ImageReader;
    use std::io::Cursor;

    let decoded = match ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.decode().ok())
        // Fallback for HEIC/HEIF/AVIF: convert via the system heif-thumbnailer.
        .or_else(|| heic_to_dynimage(&bytes))
    {
        Some(img) => img,
        None => return (bytes, false), // undecodable format; forward as-is
    };

    let img = if decoded.width() > max_dim || decoded.height() > max_dim {
        decoded.resize(max_dim, max_dim, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    // JPEG has no alpha channel; flatten to RGB before encoding.
    let img = image::DynamicImage::ImageRgb8(img.to_rgb8());

    let mut out = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    match img.write_with_encoder(encoder) {
        Ok(()) => (out.into_inner(), true),
        Err(_) => (bytes, false),
    }
}

// Decode a HEIC/HEIF/AVIF image (which the `image` crate cannot read) by
// shelling out to the system `heif-thumbnailer`, which renders the file's
// primary image to a PNG at up to `max_dim` px (configurable via
// WOW_HEIF_MAX_DIM, default 2000). Returns the decoded image, or None when the
// input isn't a HEIF-family file, the tool is missing/fails, or its output
// can't be read. Runs in a blocking context (called from compress_to_fit via
// spawn_blocking), so the synchronous std::process/std::fs calls are fine.
fn heic_to_dynimage(bytes: &[u8]) -> Option<image::DynamicImage> {
    use image::ImageReader;
    use std::io::Cursor;

    // Only attempt for ISO-BMFF / HEIF-family inputs; anything else the image
    // crate already handled (or genuinely can't be decoded).
    let head = &bytes[..bytes.len().min(16)];
    if !matches!(
        detect_image_format(head),
        "HEIC" | "HEIF" | "AVIF" | "ISO-BMFF (HEIC/HEIF family)"
    ) {
        return None;
    }

    let max_dim = std::env::var("WOW_HEIF_MAX_DIM")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(2000);

    let stamp = Uuid::new_v4();
    let dir = std::env::temp_dir();
    let in_path = dir.join(format!("wow_heic_{stamp}.heic"));
    let out_path = dir.join(format!("wow_heic_{stamp}.png"));

    let decoded = (|| {
        std::fs::write(&in_path, bytes).ok()?;
        // heif-thumbnailer [-s size] -p renders the primary image to PNG.
        let status = std::process::Command::new("heif-thumbnailer")
            .arg("-p")
            .arg("-s")
            .arg(max_dim.to_string())
            .arg(&in_path)
            .arg(&out_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let png = std::fs::read(&out_path).ok()?;
        ImageReader::new(Cursor::new(png))
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()
    })();

    // Best-effort temp cleanup regardless of outcome.
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);

    match &decoded {
        Some(img) => eprintln!(
            "HEIC decode: converted HEIF-family image to {}x{} via heif-thumbnailer (max {max_dim}px)",
            img.width(),
            img.height()
        ),
        None => eprintln!(
            "HEIC decode: heif-thumbnailer conversion failed or unavailable (is the 'heif-thumbnailer' binary on PATH?)"
        ),
    }
    decoded
}

// Re-encode `bytes` as JPEG, progressively reducing quality and then the
// longest side, until the result fits within `max_bytes`. The AI platform
// rejects multipart bodies larger than its limit and phone photos routinely
// exceed it, so oversized uploads are shrunk down to fit instead of being
// rejected. HEIC/HEIF input (which the image crate can't read) is first
// converted to a decodable image via heic_to_dynimage. Returns (bytes, true)
// with the fitted JPEG, or (original_bytes, false) when the input already fits,
// still can't be decoded, or can't be squeezed under the limit — the caller
// then forwards it and lets the AI platform decide. CPU-bound — run via
// spawn_blocking.
fn compress_to_fit(bytes: Vec<u8>, max_bytes: usize) -> (Vec<u8>, bool) {
    use image::ImageReader;
    use std::io::Cursor;

    if bytes.len() <= max_bytes {
        return (bytes, false); // already within the limit; forward untouched
    }

    let decoded = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.decode().ok())
        // Fallback for HEIC/HEIF/AVIF, unreadable by the image crate: convert
        // it to a decodable image via the system heif-thumbnailer.
        .or_else(|| heic_to_dynimage(&bytes));

    let decoded = match decoded {
        // JPEG has no alpha channel; flatten to RGB before encoding.
        Some(img) => image::DynamicImage::ImageRgb8(img.to_rgb8()),
        None => return (bytes, false), // still undecodable; forward as-is
    };

    // Shrink the longest side in halving steps, trying several quality levels at
    // each, and keep the first encoding that lands under the limit.
    let mut max_dim = decoded.width().max(decoded.height()).max(1);
    for _ in 0..10 {
        let img = if decoded.width() > max_dim || decoded.height() > max_dim {
            decoded.resize(max_dim, max_dim, image::imageops::FilterType::Triangle)
        } else {
            decoded.clone()
        };
        for &quality in &[85u8, 70, 55, 40] {
            let mut out = Cursor::new(Vec::new());
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
            if img.write_with_encoder(encoder).is_ok() {
                let encoded = out.into_inner();
                if encoded.len() <= max_bytes {
                    return (encoded, true);
                }
            }
        }
        if max_dim <= 320 {
            break;
        }
        max_dim = (max_dim / 2).max(320);
    }
    // Couldn't get under the limit; forward the original untouched.
    (bytes, false)
}

// If the image saved at `path` is larger than the AI limit (default 5 MB),
// compress it down to the target (default 4.9 MB) — converting HEIC/HEIF to
// JPEG as needed — and REPLACE it on disk with the reduced JPEG (renamed to a
// `.jpg` extension), deleting the oversized original. Returns the (possibly
// new) path to use for both the AI upload and DB persistence, so our storage
// keeps the same reduced image the AI platform receives. Files already within
// the limit, or that can't be decoded/compressed, are left untouched and their
// original path returned.
async fn reduce_saved_image(path: &str) -> String {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("reduce_saved_image: read {path} failed: {e}; leaving as-is");
            return path.to_string();
        }
    };
    let original_len = bytes.len();
    if original_len <= max_image_bytes() {
        return path.to_string(); // already within the limit
    }

    let target = ai_target_bytes();
    let (reduced, reencoded) =
        match tokio::task::spawn_blocking(move || compress_to_fit(bytes, target)).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("reduce_saved_image: compress task for {path} failed: {e}; leaving as-is");
                return path.to_string();
            }
        };
    if !reencoded {
        eprintln!(
            "reduce_saved_image: {path} ({original_len} bytes) could not be reduced (undecodable?); leaving original on disk"
        );
        return path.to_string();
    }

    // Write the reduced JPEG next to the original with a `.jpg` extension, then
    // remove the original so only the reduced file remains in storage.
    let p = Path::new(path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let dir = p.parent().and_then(|d| d.to_str()).unwrap_or(".");
    let new_path = format!("{dir}/{stem}.jpg");

    if let Err(e) = tokio::fs::write(&new_path, &reduced).await {
        eprintln!("reduce_saved_image: write {new_path} failed: {e}; keeping original {path}");
        return path.to_string();
    }
    if new_path != path {
        if let Err(e) = tokio::fs::remove_file(path).await {
            eprintln!("reduce_saved_image: could not remove original {path}: {e}");
        }
    }
    eprintln!(
        "reduce_saved_image: reduced {path} ({original_len} bytes) -> {new_path} ({} bytes)",
        reduced.len()
    );
    new_path
}

async fn ai_enroll(
    name: &str,
    identifier: &str,
    id_type: &str,
    image_paths: &[String],
    log: &StepLogger,
) -> Result<(), String> {
    // AI enrollment is mandatory: without a confirmed success from the AI
    // endpoint nothing may be persisted. A missing WOW_AI_BASE_URL is therefore
    // a hard failure (not a silent skip) so the caller aborts before the DB write.
    let base = match std::env::var("WOW_AI_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!("WOW_AI_BASE_URL not configured; refusing to enroll without AI success");
            return Err("Face enrollment endpoint (WOW_AI_BASE_URL) is not configured".into());
        }
    };
    let api_key = std::env::var("WOW_AI_API_KEY").unwrap_or_default();
    let url = format!("{}/enroll", base.trim_end_matches('/'));

    // Re-enroll = replace: drop any existing person record for this identifier
    // first so the platform keeps only the images supplied in this request,
    // matching the DB-side versioning model. Best-effort — a cleanup failure is
    // logged but does not abort the (re-)enroll.
    match ai_delete_person(&base, &api_key, identifier, id_type).await {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "AI cleanup: removed {n} existing record(s) for {identifier}/{id_type} before re-enroll"
        ),
        Err(e) => eprintln!("AI cleanup failed for {identifier}/{id_type} (continuing): {e}"),
    }

    let mut form = reqwest::multipart::Form::new()
        .text("name", name.to_string())
        .text("identifier", identifier.to_string())
        .text("id_type", id_type.to_uppercase());

    // Read + downscale every image concurrently. Each downscale is CPU-bound and
    // runs on the blocking pool, so multiple photos are processed in parallel
    // instead of one-at-a-time. `try_join_all` preserves input order.
    let prepared = futures_util::future::try_join_all(image_paths.iter().enumerate().map(
        |(i, path)| {
            let path = path.clone();
            async move {
                let raw = tokio::fs::read(&path)
                    .await
                    .map_err(|e| format!("read {path}: {e}"))?;
                let original_len = raw.len();
                let (bytes, reencoded) =
                    tokio::task::spawn_blocking(move || downscale_jpeg(raw, 1280))
                        .await
                        .map_err(|e| format!("image resize task failed: {e}"))?;
                Ok::<_, String>((i, path, bytes, reencoded, original_len))
            }
        },
    ))
    .await?;

    let mut total_bytes = 0usize;
    for (i, path, bytes, reencoded, original_len) in prepared {
        // Forward with a sanitized ASCII filename + real image MIME. Mobile
        // captures can carry filenames with spaces, non-ASCII chars, colons or
        // quotes that corrupt the Content-Disposition header and make the AI
        // platform reject the request ("Error parsing multipart/form-data").
        let (ext, mime) = if reencoded {
            ("jpg".to_string(), "image/jpeg")
        } else {
            // Couldn't decode (e.g. HEIC) — keep the original extension/MIME.
            let ext = Path::new(&path)
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| e.to_ascii_lowercase())
                .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()))
                .unwrap_or_else(|| "jpg".to_string());
            let mime = match ext.as_str() {
                "png" => "image/png",
                "webp" => "image/webp",
                "heic" | "heif" => "image/heic",
                "bmp" => "image/bmp",
                _ => "image/jpeg",
            };
            (ext, mime)
        };
        let filename = format!("image_{}.{ext}", i + 1);
        total_bytes += bytes.len();
        eprintln!(
            "AI enroll: forwarding image {} as '{filename}' ({} bytes{}, {mime}) from {path}",
            i + 1,
            bytes.len(),
            if reencoded {
                format!(", resized from {original_len}")
            } else {
                String::new()
            }
        );
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| e.to_string())?;
        form = form.part("images", part);
    }
    eprintln!(
        "AI enroll: POST {url} for {identifier}/{id_type} with {} image(s), {total_bytes} bytes total",
        image_paths.len()
    );

    let resp = AI_CLIENT
        .post(&url)
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    log.ai_response("/enroll", status.as_u16(), &body);

    // The AI endpoint's return is authoritative: persist only when it confirms
    // success explicitly (2xx AND `success: true`). Anything else — a non-2xx
    // status, `success: false`, a missing `success` field, or an unparseable
    // body — is treated as a failure so nothing is written to the database.
    let ai_confirmed =
        status.is_success() && body.get("success").and_then(|v| v.as_bool()) == Some(true);
    if !ai_confirmed {
        eprintln!("AI enroll: platform did not confirm success (status {status}): {body}");
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("AI enroll did not confirm success (status {status})"));
        return Err(err);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// 1. Enroll  — POST /ext-api/wow-attendance/enroll?id=&id_type=
// ---------------------------------------------------------------------
#[post("/wow-attendance/enroll")]
pub async fn wow_enroll(
    req: actix_web::HttpRequest,
    auth: BearerAuth,
    db: web::Data<PgPool>,
    query: web::Query<EnrollQuery>,
    payload: Multipart,
) -> HttpResponse {
    // Per-call step log written to <WOW_LOG_DIR>/{id}_{time}.log on return.
    let log = StepLogger::new("ext-api/wow-attendance/enroll");
    log.set_base_url(&public_base_url(&req));
    log.set_endpoint(req.method().as_str(), &full_path(&req));
    log.set_base_url(&public_base_url(&req));
    log.set_endpoint(req.method().as_str(), &full_path(&req));
    log.set_base_url(&public_base_url(&req));
    log.set_endpoint(req.method().as_str(), &full_path(&req));
    log.params("query", &query_to_json(req.query_string()));
    let resp = wow_enroll_inner(&log, req, auth, db, query, payload).await;
    log_local_response(&log, resp).await
}

async fn wow_enroll_inner(
    log: &StepLogger,
    req: actix_web::HttpRequest,
    auth: BearerAuth,
    db: web::Data<PgPool>,
    query: web::Query<EnrollQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    // Token handling mirrors the announcement-board / `/api/course/*` routes:
    // the Bearer access_token is extracted with the `BearerAuth` extractor. A
    // missing Authorization header is rejected by the extractor before the
    // handler runs.
    let token = auth.token().to_string();
    log.step("bearer token extracted from Authorization header");
    // Validate the bearer token: signature (our `JWT_SECRET`) and expiry, falling
    // back to DU's legacy token while clients migrate. No network call. Keep the
    // `sub` (person id) so it can be matched against the enroll `id` below.
    let token_user = match user_from_token(&token) {
        Ok(u) => u,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    let token_user_id = token_user.person_id;
    log.step(token_step(&token_user));
    // Ownership and `id_type` are both settled below, once the multipart body has
    // yielded the `id` being enrolled — DU is asked about that `id`, not about the
    // token's `sub`, because the two are not the same number for a caller on a DU
    // token (see `du_identify`).

    let enrolled_dir = enrolled_dir();
    if !Path::new(&enrolled_dir).exists() {
        if let Err(e) = tokio::fs::create_dir_all(&enrolled_dir).await {
            log.step(format!("enrolled dir create FAILED: {e}"));
            return HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": format!("Directory create failed: {e}") }));
        }
        log.step(format!("enrolled dir created: {enrolled_dir}"));
    }

    let mut id: Option<String> = query.id.clone().filter(|s| !s.trim().is_empty());
    let mut device_info: Option<String> = None;
    let mut name: Option<String> = None;
    let mut image_paths: Vec<String> = Vec::new();

    log.step("parsing multipart body");
    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                log.step(format!("multipart field error: {e}"));
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };

        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();
        let filename = cd.and_then(|c| c.get_filename()).unwrap_or("").to_string();

        match field_name.as_str() {
            "id" | "id_type" | "token" | "device_info" | "name" => {
                let mut data = Vec::new();
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(bytes) => data.extend_from_slice(&bytes),
                        Err(e) => {
                            return HttpResponse::BadRequest().json(
                                json!({ "success": false, "message": format!("Field read error: {e}") }),
                            );
                        }
                    }
                }
                let text = String::from_utf8_lossy(&data).to_string();
                // The value, not just the name — a wrong `id_type` or a
                // malformed `device_info` is invisible otherwise. Secret-ish
                // field names are masked by the logger.
                let mut param = serde_json::Map::new();
                param.insert(field_name.clone(), Value::String(text.clone()));
                log.params("form", &Value::Object(param));
                match field_name.as_str() {
                    "id" => id = Some(text).filter(|s| !s.trim().is_empty()).or(id),
                    // `id_type` and `token` are intentionally ignored here — the
                    // token comes from the Authorization header and id_type is
                    // derived from it. Both fields are still drained above.
                    "device_info" => device_info = Some(text),
                    "name" => name = Some(text).filter(|s| !s.trim().is_empty()),
                    _ => {}
                }
            }
            "images" if !filename.is_empty() => {
                let part_ct = field
                    .content_type()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "<none>".into());
                let safe_filename = format!("{}_{}", Uuid::new_v4(), filename);
                let filepath = format!("{enrolled_dir}{safe_filename}");
                log.step(format!("receiving image '{filename}' (part_ct={part_ct}) -> {safe_filename}"));

                let mut f = match File::create(&filepath).await {
                    Ok(file) => file,
                    Err(e) => {
                        log.step(format!("image file create FAILED: {e}"));
                        return HttpResponse::InternalServerError().json(
                            json!({ "success": false, "message": format!("File create error: {e}") }),
                        );
                    }
                };
                let mut written = 0usize;
                let mut head: Vec<u8> = Vec::new();
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if head.len() < 16 {
                                head.extend_from_slice(&bytes[..bytes.len().min(16 - head.len())]);
                            }
                            written += bytes.len();
                            // Only reject above the upload ceiling. Images between
                            // the AI limit and the ceiling are accepted here and
                            // compressed down (and HEIC converted to JPEG) before
                            // being sent to the AI platform (see ai_enroll). The
                            // partial file and any earlier images are kept on disk
                            // for audit even when the upload is rejected.
                            if written > max_upload_bytes() {
                                log.step(format!(
                                    "image '{filename}' exceeds upload ceiling ({} bytes) — rejecting (file kept)",
                                    written
                                ));
                                return HttpResponse::PayloadTooLarge().json(json!({
                                    "success": false,
                                    "message": format!("Each image must be at most {}", max_upload_mb_display())
                                }));
                            }
                            if let Err(e) = f.write_all(&bytes).await {
                                log.step(format!("image write FAILED: {e}"));
                                return HttpResponse::InternalServerError().json(json!({
                                    "success": false,
                                    "message": format!("File write error: {e}")
                                }));
                            }
                        }
                        Err(e) => {
                            log.step(format!("image chunk read error: {e}"));
                            return HttpResponse::BadRequest().json(
                                json!({ "success": false, "message": format!("File chunk error: {e}") }),
                            );
                        }
                    }
                }
                // Detect the real format from the leading bytes so we can tell
                // what the phone actually uploaded (Android 15 often sends HEIC).
                let magic = detect_image_format(&head);
                let head_hex: String = head.iter().map(|b| format!("{b:02x} ")).collect();
                eprintln!(
                    "wow_enroll: received image filename='{filename}' part_ct='{part_ct}' \
                     bytes={written} detected={magic} head=[{}]",
                    head_hex.trim_end()
                );
                // Accept image files only — reject anything whose magic bytes
                // don't match a known image format (e.g. a .md/.pdf/text file).
                // The uploaded file is kept on disk for audit.
                if !is_supported_image(&head) {
                    log.step(format!("image '{filename}' rejected — not a supported image (detected={magic}, file kept)"));
                    return HttpResponse::BadRequest().json(json!({
                        "success": false,
                        "message": format!(
                            "Only image files are allowed (JPEG, PNG, WEBP, BMP, HEIC/HEIF); \
                             '{filename}' is not an image"
                        )
                    }));
                }
                log.step(format!("image saved ({written} bytes, detected={magic})"));
                log.file(&filepath, browsable_path(&filepath).as_deref());
                image_paths.push(filepath);
            }
            _ => {}
        }
    }

    log.step(format!("multipart parsed — {} image(s) saved", image_paths.len()));

    // Reduce any oversized (>5 MB) saved image to ≤4.9 MB on disk (HEIC->JPEG
    // included) so BOTH our storage and the AI platform get the reduced file.
    for p in image_paths.iter_mut() {
        let reduced = reduce_saved_image(p).await;
        if &reduced != p {
            log.step(format!("enroll image reduced on disk: {p} -> {reduced}"));
            log.file(&reduced, browsable_path(&reduced).as_deref());
            *p = reduced;
        }
    }

    let id = match id {
        Some(id) => id,
        None => {
            log.step("`id` missing — rejecting request");
            return HttpResponse::BadRequest().json(json!({
                "success": false,
                "message": "`id` is required (query param or form field)"
            }));
        }
    };
    log.set_id(&id);
    log.step(format!("resolved id={id}"));

    // The enroll `id` must belong to the authenticated token holder, so a valid
    // token can only enroll its own face. It qualifies either by being the token's
    // `sub` outright, or by DU reporting that employee `id` has `user_id == sub`
    // — the link that lets a DU-token caller (whose `sub` is a DU user_id) enroll
    // under their employee id. The same lookup settles `id_type`.
    let identified = du_identify(&id, token_user_id, &log).await;
    if !identified.owned {
        // Same shape as the verify mismatch, but enroll has no face recognition:
        // the id being enrolled comes from the request, not from a recognized face.
        let message = format!(
            "Person mismatch. enroll id={id} but Logged in User id={token_user_id}"
        );
        log.step(&message);
        // Enroll rejects before any AI call, so request_id / similarity are NULL.
        record_token_mismatch(db.get_ref(), "Enroll", &id, token_user_id, None, None, &log).await;
        return HttpResponse::Unauthorized().json(json!({
            "success": false,
            "message": message,
            "enroll_id": id,
            "logged_in_user_id": token_user_id
        }));
    }
    log.step(format!("token holder owns enroll id={id}"));

    // DU confirming `id` as an employee settles `id_type`. Otherwise — every
    // student, since DU has no student lookup — fall back to the `X-Id-Type`
    // header and then WOW_IDTYPE_FALLBACK.
    let id_type = match identified
        .is_employee
        .then(|| "Employee".to_string())
        .or_else(|| header_or_env_id_type(&req))
    {
        Some(t) => t,
        None => {
            log.step("id_type could not be resolved — rejecting request");
            return HttpResponse::BadRequest().json(json!({
                "success": false,
                "message": "id_type could not be determined — send an `X-Id-Type: Student|Employee` \
                            header (or set WOW_IDTYPE_FALLBACK)"
            }));
        }
    };
    log.step(format!("id_type resolved: {id_type}"));

    let device_json: Option<Value> = device_info
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    // Forward the captured images to the AI platform FIRST so it can learn the
    // face. Only when the platform accepts the enrollment do we touch the DB —
    // a failed AI enroll must leave the enrollment table unchanged. The
    // platform requires a name; fall back to the identifier when not given.
    let learn_name = name.unwrap_or_else(|| id.clone());
    log.step(format!("calling AI platform enroll (name='{learn_name}', images={})", image_paths.len()));
    if let Err(e) = ai_enroll(&learn_name, &id, &id_type, &image_paths, log).await {
        eprintln!("AI enroll failed for {id}/{id_type}: {e}");
        log.step(format!("AI enroll FAILED: {e}"));
        // Keep the uploaded image files on disk even though the AI platform
        // rejected the enrollment, so they remain available for retry/audit.
        log.step(format!("kept {} uploaded image(s) after AI failure: {}", image_paths.len(), image_paths.join(", ")));
        return HttpResponse::BadGateway().json(json!({
            "success": false,
            "ai_enrolled": false,
            "ai_error": e,
            "message": "Face enrollment failed on the AI platform; nothing was saved"
        }));
    }

    log.step("AI enroll OK");

    // AI platform accepted the face — persist the enrollment.
    log.step("persisting enrollment via ictcell.wow_attendance_enroll");
    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_enroll($1, $2, $3, $4, $5)",
    )
    .bind(&id)
    .bind(&id_type)
    .bind(&token)
    .bind(device_json)
    .bind(&image_paths)
    .fetch_one(db.get_ref())
    .await;

    // Keep the enrolled image files on disk; their paths are persisted with the
    // enrollment (bound as $5 above) and remain available as a reference.
    log.step(format!("stored {} enrolled image(s): {}", image_paths.len(), image_paths.join(", ")));

    let mut body = match result {
        Ok(json) => json,
        Err(err) => {
            eprintln!("DB error in wow_enroll: {err}");
            log.step(format!("DB enroll FAILED: {err}"));
            return HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }));
        }
    };
    log.step("DB enroll OK");

    if let Some(obj) = body.as_object_mut() {
        obj.insert("ai_enrolled".into(), json!(true));
    }

    log.step("enrollment complete — returning 200");
    HttpResponse::Ok().json(body)
}

// ---------------------------------------------------------------------
// 2. Enrolled list  — POST /ext-api/wow-attendance/enrolled?id_type=
// ---------------------------------------------------------------------
#[post("/wow-attendance/enrolled")]
pub async fn wow_enrolled_list(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<EnrolledListQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    let token = match require_bearer_token(&req) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let mut id_type: Option<String> = query.id_type.clone().filter(|s| !s.trim().is_empty());
    let mut page: i32 = 1;
    let mut limit: i32 = 20;

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };

        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();

        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => {
                    return HttpResponse::BadRequest().json(
                        json!({ "success": false, "message": format!("Field read error: {e}") }),
                    );
                }
            }
        }
        let text = String::from_utf8_lossy(&data).to_string();

        match field_name.as_str() {
            "id_type" => id_type = Some(text).filter(|s| !s.trim().is_empty()).or(id_type),
            // `token` now comes from the Authorization header; ignore the field.
            "page" => page = text.trim().parse().unwrap_or(1),
            "limit" => limit = text.trim().parse().unwrap_or(20),
            _ => {}
        }
    }

    let id_type = match id_type {
        Some(v) => v,
        None => {
            return HttpResponse::BadRequest().json(json!({
                "success": false,
                "message": "`id_type` is required (query param or form field)"
            }));
        }
    };

    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_enrolled_list($1, $2, $3, $4)",
    )
    .bind(&id_type)
    .bind(&token)
    .bind(page)
    .bind(limit)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(err) => {
            eprintln!("DB error in wow_enrolled_list: {err}");
            HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }))
        }
    }
}

// ---------------------------------------------------------------------
// 2b. Check enrolled — POST /ext-api/wow-attendance/check?person_id=
//
// Returns whether a person (by person_id) has an active enrollment, along
// with the enrollment details when they do.
// ---------------------------------------------------------------------
#[post("/wow-attendance/check")]
pub async fn wow_check_enrolled(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<CheckEnrolledQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    if let Err(resp) = require_bearer_token(&req) {
        return resp;
    }

    let mut person_id: Option<String> = query.person_id.clone().filter(|s| !s.trim().is_empty());

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };

        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();

        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => {
                    return HttpResponse::BadRequest().json(
                        json!({ "success": false, "message": format!("Field read error: {e}") }),
                    );
                }
            }
        }
        let text = String::from_utf8_lossy(&data).to_string();

        match field_name.as_str() {
            "person_id" | "id" => {
                person_id = Some(text).filter(|s| !s.trim().is_empty()).or(person_id)
            }
            // `token` now comes from the Authorization header; ignore the field.
            _ => {}
        }
    }

    let person_id = match person_id {
        Some(v) => v,
        None => {
            return HttpResponse::BadRequest().json(json!({
                "success": false,
                "message": "`person_id` is required (query param or form field)"
            }));
        }
    };

    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_check_enrolled($1)",
    )
    .bind(&person_id)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(err) => {
            eprintln!("DB error in wow_check_enrolled: {err}");
            HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }))
        }
    }
}

// ---------------------------------------------------------------------
// 2c. Records report by date range
//     POST /ext-api/wow-attendance/reports/by-date?from_date=&to_date=&id_type=
//
// Lists attendance records (newest first, paginated) whose created_at date
// falls within [from_date, to_date] inclusive. Optional id_type filter.
// ---------------------------------------------------------------------
#[post("/wow-attendance/reports/by-date")]
pub async fn wow_records_by_date(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<RecordsByDateQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    if let Err(resp) = require_bearer_token(&req) {
        return resp;
    }

    let mut from_date = query.from_date.clone().filter(|s| !s.trim().is_empty());
    let mut to_date = query.to_date.clone().filter(|s| !s.trim().is_empty());
    let mut id_type = query.id_type.clone().filter(|s| !s.trim().is_empty());
    let mut page = query.page.unwrap_or(1);
    let mut limit = query.limit.unwrap_or(20);

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };
        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();

        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => {
                    return HttpResponse::BadRequest().json(
                        json!({ "success": false, "message": format!("Field read error: {e}") }),
                    );
                }
            }
        }
        let text = String::from_utf8_lossy(&data).to_string();
        match field_name.as_str() {
            "from_date" => from_date = Some(text).filter(|s| !s.trim().is_empty()).or(from_date),
            "to_date" => to_date = Some(text).filter(|s| !s.trim().is_empty()).or(to_date),
            "id_type" => id_type = Some(text).filter(|s| !s.trim().is_empty()).or(id_type),
            "page" => page = text.trim().parse().unwrap_or(page),
            "limit" => limit = text.trim().parse().unwrap_or(limit),
            _ => {}
        }
    }

    let (from_date, to_date) = match (from_date, to_date) {
        (Some(f), Some(t)) => (f, t),
        _ => {
            return HttpResponse::BadRequest().json(json!({
                "success": false,
                "message": "`from_date` and `to_date` are required (YYYY-MM-DD)"
            }));
        }
    };
    let from = match parse_report_date(&from_date) {
        Some(d) => d,
        None => {
            return HttpResponse::BadRequest()
                .json(json!({ "success": false, "message": "Invalid `from_date`; expected YYYY-MM-DD" }));
        }
    };
    let to = match parse_report_date(&to_date) {
        Some(d) => d,
        None => {
            return HttpResponse::BadRequest()
                .json(json!({ "success": false, "message": "Invalid `to_date`; expected YYYY-MM-DD" }));
        }
    };

    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_records_by_date($1, $2, $3, $4, $5)",
    )
    .bind(from)
    .bind(to)
    .bind(&id_type)
    .bind(page)
    .bind(limit)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(err) => {
            eprintln!("DB error in wow_records_by_date: {err}");
            HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }))
        }
    }
}

// ---------------------------------------------------------------------
// 2d. Records report by person
//     POST /ext-api/wow-attendance/reports/by-person?person_id=&from_date=&to_date=
//
// Lists one person's attendance records (newest first, paginated) whose
// created_at date falls within [from_date, to_date] inclusive.
// ---------------------------------------------------------------------
#[post("/wow-attendance/reports/by-person")]
pub async fn wow_records_by_person(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<RecordsByPersonQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    if let Err(resp) = require_bearer_token(&req) {
        return resp;
    }

    let mut person_id = query.person_id.clone().filter(|s| !s.trim().is_empty());
    let mut from_date = query.from_date.clone().filter(|s| !s.trim().is_empty());
    let mut to_date = query.to_date.clone().filter(|s| !s.trim().is_empty());
    let mut page = query.page.unwrap_or(1);
    let mut limit = query.limit.unwrap_or(20);

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };
        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();

        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => {
                    return HttpResponse::BadRequest().json(
                        json!({ "success": false, "message": format!("Field read error: {e}") }),
                    );
                }
            }
        }
        let text = String::from_utf8_lossy(&data).to_string();
        match field_name.as_str() {
            "person_id" | "id" => {
                person_id = Some(text).filter(|s| !s.trim().is_empty()).or(person_id)
            }
            "from_date" => from_date = Some(text).filter(|s| !s.trim().is_empty()).or(from_date),
            "to_date" => to_date = Some(text).filter(|s| !s.trim().is_empty()).or(to_date),
            "page" => page = text.trim().parse().unwrap_or(page),
            "limit" => limit = text.trim().parse().unwrap_or(limit),
            _ => {}
        }
    }

    let (person_id, from_date, to_date) = match (person_id, from_date, to_date) {
        (Some(p), Some(f), Some(t)) => (p, f, t),
        _ => {
            return HttpResponse::BadRequest().json(json!({
                "success": false,
                "message": "`person_id`, `from_date` and `to_date` are required (dates as YYYY-MM-DD)"
            }));
        }
    };
    let from = match parse_report_date(&from_date) {
        Some(d) => d,
        None => {
            return HttpResponse::BadRequest()
                .json(json!({ "success": false, "message": "Invalid `from_date`; expected YYYY-MM-DD" }));
        }
    };
    let to = match parse_report_date(&to_date) {
        Some(d) => d,
        None => {
            return HttpResponse::BadRequest()
                .json(json!({ "success": false, "message": "Invalid `to_date`; expected YYYY-MM-DD" }));
        }
    };

    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_records_by_person($1, $2, $3, $4, $5)",
    )
    .bind(&person_id)
    .bind(from)
    .bind(to)
    .bind(page)
    .bind(limit)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(err) => {
            eprintln!("DB error in wow_records_by_person: {err}");
            HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }))
        }
    }
}

// ---------------------------------------------------------------------
// 2e. SSL image verify — POST /ext-api/wow-attendance/ssl_image_verfiy
//
// Accepts one or more uploaded images (multipart field `images`) and forwards
// them, untouched, to the third-party AI upload service. The service's response
// (JSON when parseable, otherwise raw text) is relayed back to the caller.
//
// The target URL defaults to http://103.221.253.226/ai/upload.php and can be
// overridden with WOW_SSL_VERIFY_URL.
// ---------------------------------------------------------------------
fn ssl_verify_url() -> String {
    std::env::var("WOW_SSL_VERIFY_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://103.221.253.226/ai/upload.php".to_string())
}

#[post("/wow-attendance/ssl_image_verfiy")]
pub async fn wow_ssl_image_verify(
    req: actix_web::HttpRequest,
    mut payload: Multipart,
) -> HttpResponse {
    if let Err(resp) = require_bearer_token(&req) {
        return resp;
    }

    // Validate the request is multipart/form-data up front so a missing or
    // malformed Content-Type produces an actionable message instead of the
    // opaque "Could not find Content-Type header" the multipart parser emits.
    let content_type = req
        .headers()
        .get(actix_web::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.is_empty() {
        return HttpResponse::BadRequest().json(json!({
            "success": false,
            "message": "Missing Content-Type header. Send the request as multipart/form-data \
                        with the images in the `images` field. Do not set the Content-Type \
                        header manually — let your HTTP client add it (with the boundary)."
        }));
    }
    if !content_type.to_ascii_lowercase().contains("multipart/form-data") {
        return HttpResponse::BadRequest().json(json!({
            "success": false,
            "message": format!(
                "Unsupported Content-Type '{content_type}'. Send the request as \
                 multipart/form-data with the images in the `images` field."
            )
        }));
    }
    if !content_type.to_ascii_lowercase().contains("boundary") {
        return HttpResponse::BadRequest().json(json!({
            "success": false,
            "message": "multipart/form-data Content-Type is missing its `boundary` parameter. \
                        Do not set the Content-Type header manually — let your HTTP client add \
                        it so the boundary is included."
        }));
    }

    // Collect each uploaded image into a multipart part forwarded to the
    // third-party service. upload.php expects the `images[]` array field name.
    let mut form = reqwest::multipart::Form::new();
    let mut image_count = 0usize;

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };

        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();
        let filename = cd.and_then(|c| c.get_filename()).unwrap_or("").to_string();

        // Only the file fields are forwarded; anything else is ignored.
        if !matches!(field_name.as_str(), "images" | "image" | "file" | "photo") || filename.is_empty()
        {
            // Drain the field so the stream advances to the next part.
            while let Some(chunk) = field.next().await {
                if let Err(e) = chunk {
                    return HttpResponse::BadRequest().json(
                        json!({ "success": false, "message": format!("Field read error: {e}") }),
                    );
                }
            }
            continue;
        }

        let part_mime = field
            .content_type()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => {
                    return HttpResponse::BadRequest().json(
                        json!({ "success": false, "message": format!("File chunk error: {e}") }),
                    );
                }
            }
        }

        // Sanitize the filename to plain ASCII so the third-party multipart
        // parser never trips on mobile-supplied names (spaces, non-ASCII, etc.).
        let ext = Path::new(&filename)
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| e.to_ascii_lowercase())
            .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_else(|| "jpg".to_string());
        let safe_filename = format!("image_{}.{ext}", image_count + 1);

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(safe_filename)
            .mime_str(&part_mime)
            .unwrap_or_else(|_| {
                reqwest::multipart::Part::bytes(Vec::new())
            });
        form = form.part("images[]", part);
        image_count += 1;
    }

    if image_count == 0 {
        return HttpResponse::BadRequest().json(json!({
            "success": false,
            "message": "At least one image is required in the `images` field"
        }));
    }

    let url = ssl_verify_url();
    let resp = match AI_CLIENT.post(&url).multipart(form).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ssl_image_verify: forward to {url} failed: {e}");
            return HttpResponse::BadGateway().json(json!({
                "success": false,
                "message": format!("Failed to reach verification service: {e}")
            }));
        }
    };

    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return HttpResponse::BadGateway().json(json!({
                "success": false,
                "message": format!("Failed to read verification response: {e}")
            }));
        }
    };

    // Relay the upstream response. Prefer JSON passthrough; fall back to raw text.
    let status_code = actix_web::http::StatusCode::from_u16(status.as_u16())
        .unwrap_or(actix_web::http::StatusCode::OK);
    match serde_json::from_str::<Value>(&text) {
        Ok(body) => HttpResponse::build(status_code).json(body),
        Err(_) => HttpResponse::build(status_code)
            .content_type("text/plain; charset=utf-8")
            .body(text),
    }
}

// ---------------------------------------------------------------------
// 3. Verify & mark attendance — POST /ext-api/wow-attendance/verify?id=&id_type=
// ---------------------------------------------------------------------
#[post("/wow-attendance/verify")]
pub async fn wow_verify(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<VerifyQuery>,
    payload: Multipart,
) -> HttpResponse {
    // Per-call step log written to <WOW_LOG_DIR>/{id}_{time}.log on return.
    let log = StepLogger::new("ext-api/wow-attendance/verify");
    log.params("query", &query_to_json(req.query_string()));
    let resp = wow_verify_inner(&log, req, db, query, payload).await;
    log_local_response(&log, resp).await
}

async fn wow_verify_inner(
    log: &StepLogger,
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<VerifyQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    let token = match require_bearer_token(&req) {
        Ok(t) => t,
        Err(resp) => {
            log.step("bearer token missing/empty — rejecting request");
            return resp;
        }
    };
    log.step("bearer token extracted from Authorization header");
    // Reject the token if it is malformed or expired; one of ours also has its
    // signature verified, while DU's legacy token is still accepted unverified
    // during the migration window. Keep the `sub` (person id) so the recognized
    // person can be matched against the token holder below.
    let token_user = match user_from_token(&token) {
        Ok(u) => u,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    let token_user_id = token_user.person_id;
    log.step(token_step(&token_user));

    let live_dir = live_dir();
    if !Path::new(&live_dir).exists() {
        if let Err(e) = tokio::fs::create_dir_all(&live_dir).await {
            log.step(format!("live dir create FAILED: {e}"));
            return HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": format!("Directory create failed: {e}") }));
        }
        log.step(format!("live dir created: {live_dir}"));
    }

    let mut id: Option<String> = query.id.clone().filter(|s| !s.trim().is_empty());
    let mut id_type: Option<String> = query.id_type.clone().filter(|s| !s.trim().is_empty());
    let mut device_info: Option<String> = None;
    let mut live_image = String::new();
    if let Some(qid) = &id {
        log.set_id(qid);
    }

    log.step("parsing multipart body");
    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(field) => field,
            Err(e) => {
                log.step(format!("multipart field error: {e}"));
                return HttpResponse::BadRequest()
                    .json(json!({ "success": false, "message": format!("Multipart error: {e}") }));
            }
        };

        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();
        let filename = cd.and_then(|c| c.get_filename()).unwrap_or("").to_string();

        match field_name.as_str() {
            "id" | "id_type" | "token" | "device_info" => {
                let mut data = Vec::new();
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(bytes) => data.extend_from_slice(&bytes),
                        Err(e) => {
                            log.step(format!("field '{field_name}' read error: {e}"));
                            return HttpResponse::BadRequest().json(
                                json!({ "success": false, "message": format!("Field read error: {e}") }),
                            );
                        }
                    }
                }
                let text = String::from_utf8_lossy(&data).to_string();
                // The value, not just the name — a wrong `id_type` or a
                // malformed `device_info` is invisible otherwise. Secret-ish
                // field names are masked by the logger.
                let mut param = serde_json::Map::new();
                param.insert(field_name.clone(), Value::String(text.clone()));
                log.params("form", &Value::Object(param));
                match field_name.as_str() {
                    "id" => id = Some(text).filter(|s| !s.trim().is_empty()).or(id),
                    "id_type" => id_type = Some(text).filter(|s| !s.trim().is_empty()).or(id_type),
                    // `token` is intentionally ignored here — it now comes from
                    // the Authorization header. The field is still drained above.
                    "device_info" => device_info = Some(text),
                    _ => {}
                }
            }
            "image" | "images" | "file" | "photo" if !filename.is_empty() => {
                let safe_filename = format!("{}_{}", Uuid::new_v4(), filename);
                let filepath = format!("{live_dir}{safe_filename}");
                log.step(format!("receiving live image '{filename}' -> {safe_filename}"));

                let mut f = match File::create(&filepath).await {
                    Ok(file) => file,
                    Err(e) => {
                        log.step(format!("live image file create FAILED: {e}"));
                        return HttpResponse::InternalServerError().json(
                            json!({ "success": false, "message": format!("File create error: {e}") }),
                        );
                    }
                };
                let mut written = 0usize;
                let mut head: Vec<u8> = Vec::new();
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if head.len() < 16 {
                                head.extend_from_slice(&bytes[..bytes.len().min(16 - head.len())]);
                            }
                            written += bytes.len();
                            // Only reject uploads above the upload ceiling. Images
                            // between the AI limit and the ceiling are accepted here
                            // and compressed down before being sent to the AI
                            // platform (see ai_recognize). The partial file is kept
                            // on disk for audit.
                            if written > max_upload_bytes() {
                                log.step(format!("live image exceeds upload ceiling ({written} bytes) — rejecting (file kept)"));
                                return HttpResponse::PayloadTooLarge().json(json!({
                                    "success": false,
                                    "message": format!("Image must be at most {}", max_upload_mb_display())
                                }));
                            }
                            if let Err(e) = f.write_all(&bytes).await {
                                log.step(format!("live image write FAILED: {e}"));
                                return HttpResponse::InternalServerError().json(json!({
                                    "success": false,
                                    "message": format!("File write error: {e}")
                                }));
                            }
                        }
                        Err(e) => {
                            log.step(format!("live image chunk read error: {e}"));
                            return HttpResponse::BadRequest().json(
                                json!({ "success": false, "message": format!("File chunk error: {e}") }),
                            );
                        }
                    }
                }
                // Allow image files only — reject anything whose magic bytes don't
                // match a known image format. The file is kept on disk for audit.
                if !is_supported_image(&head) {
                    log.step("live image rejected — not a supported image format (file kept)");
                    return HttpResponse::BadRequest().json(json!({
                        "success": false,
                        "message": "Only image files are allowed (JPEG, PNG, WEBP, BMP, HEIC/HEIF)"
                    }));
                }
                log.step(format!("live image saved ({written} bytes)"));
                log.file(&filepath, browsable_path(&filepath).as_deref());
                live_image = filepath;
            }
            // Same field names but no filename → treat as a base64-encoded image
            // sent in a plain text field (optionally with a data-URL prefix).
            "image" | "images" | "file" | "photo" => {
                let mut data = Vec::new();
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(bytes) => data.extend_from_slice(&bytes),
                        Err(e) => {
                            log.step(format!("base64 image field read error: {e}"));
                            return HttpResponse::BadRequest().json(
                                json!({ "success": false, "message": format!("Field read error: {e}") }),
                            );
                        }
                    }
                }
                let text = String::from_utf8_lossy(&data);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                log.step("receiving live image as base64 field");
                // Strip an optional `data:image/...;base64,` prefix.
                let b64 = trimmed.rsplit(',').next().unwrap_or(trimmed);
                let decoded = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        log.step(format!("base64 decode FAILED: {e}"));
                        return HttpResponse::BadRequest().json(json!({
                            "success": false,
                            "message": format!("Invalid base64 image: {e}")
                        }));
                    }
                };

                // Only reject above the upload ceiling; images between the AI
                // limit and the ceiling are compressed down in ai_recognize.
                if decoded.len() > max_upload_bytes() {
                    log.step(format!("base64 image exceeds upload ceiling ({} bytes) — rejecting", decoded.len()));
                    return HttpResponse::PayloadTooLarge().json(json!({
                        "success": false,
                        "message": format!("Image must be at most {}", max_upload_mb_display())
                    }));
                }

                // Allow image data only — reject decoded bytes that aren't a
                // known image format.
                if !is_supported_image(&decoded) {
                    log.step("base64 image rejected — not a supported image format");
                    return HttpResponse::BadRequest().json(json!({
                        "success": false,
                        "message": "Only image files are allowed (JPEG, PNG, WEBP, BMP, HEIC/HEIF)"
                    }));
                }

                let safe_filename = format!("{}.jpg", Uuid::new_v4());
                let filepath = format!("{live_dir}{safe_filename}");
                if let Err(e) = tokio::fs::write(&filepath, &decoded).await {
                    log.step(format!("base64 image write FAILED: {e}"));
                    return HttpResponse::InternalServerError().json(
                        json!({ "success": false, "message": format!("File write error: {e}") }),
                    );
                }
                log.step(format!("live image saved from base64 ({} bytes)", decoded.len()));
                log.file(&filepath, browsable_path(&filepath).as_deref());
                live_image = filepath;
            }
            _ => {}
        }
    }

    // Verify needs only the live image (+ optional token). `id` / `id_type` are
    // both optional: the AI platform identifies the person and returns the
    // identifier and id_type. When provided, `id` acts as a 1:1 guard and
    // `id_type` is used as a fallback if the platform omits it.
    log.step("multipart parsed");
    if live_image.is_empty() {
        log.step("no live image supplied — rejecting request");
        return HttpResponse::BadRequest()
            .json(json!({ "success": false, "message": "A live face image is required" }));
    }

    // Reduce an oversized (>5 MB) live image to ≤4.9 MB on disk (HEIC->JPEG
    // included) so BOTH our storage and the AI platform get the reduced file.
    let reduced = reduce_saved_image(&live_image).await;
    if reduced != live_image {
        log.step(format!("live image reduced on disk: {live_image} -> {reduced}"));
        log.file(&reduced, browsable_path(&reduced).as_deref());
        live_image = reduced;
    }

    let device_json: Option<Value> = device_info
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    // Identify the live face via the AI platform's /recognize endpoint.
    log.step("calling AI platform /recognize");
    let recog_result = ai_recognize(&live_image, log).await;
    // Keep the live image on disk; the `live_image` path is persisted with the
    // attendance record and returned in the response as a stored reference.
    log.step(format!("stored live image: {live_image}"));
    let recog = match recog_result {
        Ok(r) => r,
        Err(e) => {
            let el = e.to_lowercase();
            // An unenrolled / undetected face is a clean non-match, not an error.
            if el.contains("not recognized")
                || el.contains("no match")
                || el.contains("no face")
                || el.contains("face not detected")
            {
                log.step(format!("AI recognize: no match ({e})"));
                return HttpResponse::Ok().json(json!({
                    "success": false,
                    "matched": false,
                    "message": "No matching enrolled person found",
                    // The AI platform's own reason (e.g. "Face not recognized")
                    // — the same text written to the step log — so the caller
                    // can tell an unenrolled face from an undetected one.
                    "reason": e,
                    "live_image": live_image
                }));
            }
            eprintln!("AI recognize failed: {e}");
            log.step(format!("AI recognize FAILED: {e}"));
            return HttpResponse::InternalServerError().json(json!({
                "success": false,
                "message": format!("Face recognition failed its server: {e}")
            }));
        }
    };
    log.step("AI recognize returned a response");

    let recognized = recog.get("recognized").and_then(|v| v.as_bool()).unwrap_or(false);
    let recog_identifier = recog
        .get("identifier")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let recog_identifier = match (recognized, recog_identifier) {
        (true, Some(identifier)) => identifier,
        _ => {
            // The platform answered 200 but didn't identify anyone. Pass along
            // whatever reason it gave (`error`/`message`), matching the reason
            // field on the Err no-match path above.
            let reason = recog
                .get("error")
                .or_else(|| recog.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("Face not recognized")
                .to_string();
            log.step(format!("no matching enrolled person found ({reason})"));
            return HttpResponse::Ok().json(json!({
                "success": false,
                "matched": false,
                "message": "No matching enrolled person found",
                "reason": reason,
                "live_image": live_image
            }));
        }
    };
    log.set_id(&recog_identifier);
    log.step(format!("face recognized as identifier={recog_identifier}"));

    // The recognized person must be the token holder, so a valid token can only
    // mark its own attendance. As on enroll, the recognized identifier qualifies
    // either by being the token's `sub` or by DU reporting it as the employee
    // whose `user_id` is that `sub`. The same lookup settles `id_type` below.
    let identified = du_identify(&recog_identifier, token_user_id, &log).await;
    if !identified.owned {
        let message = format!(
            "Person identified but mismatch. face recognized as identifier={recog_identifier} \
             but Logged in User id={token_user_id}"
        );
        log.step(&message);
        // Audit the mismatch. request_id / similarity come from the /recognize
        // response when the AI platform supplies them; stored as NULL otherwise.
        let ai_requested_id = recog.get("request_id").and_then(|v| v.as_str());
        let ai_similarity = recog.get("similarity").and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        });
        record_token_mismatch(
            db.get_ref(),
            "Verify",
            &recog_identifier,
            token_user_id,
            ai_requested_id,
            ai_similarity,
            &log,
        )
        .await;
        return HttpResponse::Unauthorized().json(json!({
            "success": false,
            "matched": false,
            "message": message,
            "recognized_identifier": recog_identifier,
            "logged_in_user_id": token_user_id,
            "live_image": live_image
        }));
    }
    log.step(format!("token holder owns recognized identifier={recog_identifier}"));

    // If a specific person was requested, the recognized face must be theirs.
    if let Some(req_id) = &id {
        if req_id != &recog_identifier {
            log.step(format!("recognized identifier != requested id ({req_id}) — mismatch"));
            return HttpResponse::Ok().json(json!({
                "success": false,
                "matched": false,
                "message": "Face did not match the requested person",
                "recognized_identifier": recog_identifier,
                "live_image": live_image
            }));
        }
    }

    // Use the recognized person's id/id_type for the attendance record. DU's
    // answer (from the ownership lookup above) wins; the platform's own id_type,
    // and then the request's, are only used when DU does not confirm an employee.
    let id = recog_identifier;
    let id_type = match identified
        .is_employee
        .then(|| "Employee".to_string())
        .or_else(|| {
            recog
                .get("id_type")
                .and_then(|v| v.as_str())
                .and_then(normalize_id_type)
        })
        .or(id_type)
    {
        Some(id_type) => id_type,
        None => {
            log.step("could not determine id_type for recognized person");
            return HttpResponse::InternalServerError().json(json!({
                "success": false,
                "message": "Could not determine id_type for the recognized person"
            }));
        }
    };
    log.step(format!("resolved id_type={id_type} for id={id}"));

    // Match the recognized person against ictcell.wow_attendance_enrollments:
    // the `person_id` returned by the face platform must exist as an active
    // enrollment. If it does not, the recognized identity is not enrolled here
    // and no attendance is recorded.
    log.step(format!("matching recognized person_id={id} against ictcell.wow_attendance_enrollments"));
    let enrolled: Result<Option<String>, _> = sqlx::query_scalar::<_, String>(
        "SELECT person_id \
           FROM ictcell.wow_attendance_enrollments \
          WHERE person_id = $1 AND id_type = $2 AND is_active = true \
          ORDER BY enrolled_at DESC \
          LIMIT 1",
    )
    .bind(&id)
    .bind(&id_type)
    .fetch_optional(db.get_ref())
    .await;

    match enrolled {
        Ok(Some(_)) => {
            log.step(format!("person_id={id} matched an active enrollment"));
        }
        Ok(None) => {
            log.step(format!("person_id={id} not found in enrollments — mismatch"));
            return HttpResponse::Ok().json(json!({
                "success": false,
                "matched": false,
                "message": "Recognized person is not enrolled",
                "recognized_identifier": id,
                "live_image": live_image
            }));
        }
        Err(err) => {
            eprintln!("DB error checking enrollment in wow_verify: {err}");
            log.step(format!("enrollment lookup FAILED: {err}"));
            return HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }));
        }
    }

    // ── Location gate ────────────────────────────────────────────────
    // The recognized person must also be physically at a building mapped to
    // their office. Runs here, after recognition, so the geofence is applied
    // to the person actually present rather than to a client-supplied `id`.
    //
    // Employees only: the mapping hangs off `employees.office`, which a student
    // has no row for, so students are recognized and recorded without a
    // location check until student location data exists.
    let mut location_data: Option<Value> = None;
    if id_type == "Employee" {
        let coords = device_coords(device_json.as_ref());
        let (device_lat, device_long) = match coords {
            Some(c) => c,
            None => {
                // Fail closed: a geofence that can be skipped by omitting the
                // coordinates is not a geofence.
                log.step("no usable GPS coordinates in device_info — rejecting employee check-in");
                return HttpResponse::BadRequest().json(json!({
                    "success": false,
                    "matched": true,
                    "verified": false,
                    "message": "Device location is required: send `device_info` as JSON \
                                containing `device_lat` and `device_long`",
                    "recognized_identifier": id,
                    "live_image": live_image
                }));
            }
        };
        log.step(format!(
            "checking location via ictcell.wow_attendance_location_verify \
             (lat={device_lat}, long={device_long})"
        ));

        let loc = sqlx::query_scalar::<_, Value>(
            "SELECT ictcell.wow_attendance_location_verify($1, $2, $3)",
        )
        .bind(&id)
        .bind(device_lat)
        .bind(device_long)
        .fetch_one(db.get_ref())
        .await;

        let loc = match loc {
            Ok(v) => v,
            Err(err) => {
                eprintln!("DB error checking location in wow_verify: {err}");
                log.step(format!("location check FAILED: {err}"));
                return HttpResponse::InternalServerError()
                    .json(json!({ "success": false, "message": "Internal server error" }));
            }
        };

        let verified = loc.get("verified").and_then(|v| v.as_bool()).unwrap_or(false);
        if !verified {
            let message = loc
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Device location does not match any mapped building");
            log.step(format!("location NOT verified ({message}) — no attendance recorded"));
            return HttpResponse::Forbidden().json(json!({
                "success": false,
                "matched": true,
                "verified": false,
                "message": message,
                "recognized_identifier": id,
                "location": loc.get("data").cloned().unwrap_or(Value::Null),
                "live_image": live_image
            }));
        }
        log.step("location verified");
        location_data = loc.get("data").cloned();
    } else {
        log.step(format!("location check skipped — id_type={id_type} is not an Employee"));
    }

    let matched = true;
    // /recognize does not return a numeric score; record 1.0 on a match.
    let confidence = 1.0_f64;

    log.step("recording attendance via ictcell.wow_attendance_verify");
    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_verify($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&id)
    .bind(&id_type)
    .bind(&token)
    .bind(device_json)
    .bind(&live_image)
    .bind(matched)
    .bind(confidence)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(mut json) => {
            // Report which building the check-in was accepted at, so a caller
            // can show it without a second round-trip.
            if let (Some(obj), Some(loc)) = (json.as_object_mut(), location_data) {
                obj.insert("location".into(), loc);
            }
            log.step("attendance recorded — returning 200");
            HttpResponse::Ok().json(json)
        }
        Err(err) => {
            eprintln!("DB error in wow_verify: {err}");
            log.step(format!("DB verify FAILED: {err}"));
            HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": err.to_string() }))
        }
    }
}

// ---------------------------------------------------------------------
// 4. Admin: save a body -> building mapping
//     POST /ext-api/wow-attendance/mapping-save
//
// Sets the GPS position and radius that the location gate in `wow_verify`
// checks against, so this endpoint defines the geofence for every employee
// in an office.
// ---------------------------------------------------------------------

// Admin gate for the mapping writes.
//
// `Claims` carries only `sub` and `exp` — there is no role in the token — so
// admin cannot be established from the bearer token alone. Until roles exist,
// writes require a shared `X-Admin-Key` matching `WOW_ADMIN_KEY`, on top of
// the bearer token and ExtAuthMiddleware that already guard `/ext-api`.
//
// The key is shared, so it identifies "someone holding the key", not a person;
// callers are logged with the token's `sub` so a write can still be traced back
// to a login. Replace this with a role claim once the token carries one.
fn require_admin_key(req: &actix_web::HttpRequest) -> Result<(), HttpResponse> {
    let configured = match std::env::var("WOW_ADMIN_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        // Fail closed: with no key configured the endpoint is unusable rather
        // than open. An unset env var must never mean "allow everyone".
        _ => {
            eprintln!("WOW_ADMIN_KEY not configured; refusing mapping write");
            return Err(HttpResponse::ServiceUnavailable().json(json!({
                "success": false,
                "message": "Admin operations are not configured on this server"
            })));
        }
    };

    let supplied = req
        .headers()
        .get("X-Admin-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();

    // Constant-time compare so a wrong key can't be recovered byte-by-byte by
    // timing the response.
    let a = supplied.as_bytes();
    let b = configured.trim().as_bytes();
    let equal = a.len() == b.len()
        && a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;

    if !equal {
        return Err(HttpResponse::Forbidden().json(json!({
            "success": false,
            "message": "Valid `X-Admin-Key` header required for this operation"
        })));
    }
    Ok(())
}

// Serialize is derived only so the request can be echoed into the step log as
// `Params (json)`; nothing serialises it onto the wire.
#[derive(Deserialize, serde::Serialize)]
pub struct MappingSaveRequest {
    // `ictcell.body.body_code` ("490010"), which is what `employees.office`
    // holds — NOT `body.body_id` ("OES").
    pub body_code: String,
    // Target an existing building by id, or omit it and pass `building_name`
    // to find-or-create one by name.
    pub building_id: Option<i32>,
    pub building_name: Option<String>,
    pub lat: f64,
    pub long: f64,
    // Metres; defaults to 50 in the DB function when omitted.
    pub radius: Option<f64>,
    pub is_active: Option<bool>,
}

#[post("/wow-attendance/mapping-save")]
pub async fn wow_mapping_save(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<MappingSaveRequest>,
) -> HttpResponse {
    let log = StepLogger::new("ext-api/wow-attendance/mapping-save");
    log.params("query", &query_to_json(req.query_string()));
    // A JSON body is the whole input here, so it is the Params that matter.
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let resp = wow_mapping_save_inner(&log, req, db, body).await;
    log_local_response(&log, resp).await
}

async fn wow_mapping_save_inner(
    log: &StepLogger,
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<MappingSaveRequest>,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    let token = match require_bearer_token(&req) {
        Ok(t) => t,
        Err(resp) => {
            log.step("bearer token missing/empty — rejecting request");
            return resp;
        }
    };
    let token_user = match user_from_token(&token) {
        Ok(u) => u,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    log.step(token_step(&token_user));

    if let Err(resp) = require_admin_key(&req) {
        log.step("admin key check FAILED — rejecting request");
        return resp;
    }
    // Attribution for a geofence change: the shared key says only that the
    // caller is an admin, so record which login performed the write.
    log.step(format!(
        "admin key OK — mapping write by token user id={}",
        token_user.person_id
    ));

    let body_code = body.body_code.trim().to_string();
    if body_code.is_empty() {
        return HttpResponse::BadRequest().json(json!({
            "success": false,
            "message": "`body_code` is required"
        }));
    }
    if body.building_id.is_none()
        && body
            .building_name
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    {
        return HttpResponse::BadRequest().json(json!({
            "success": false,
            "message": "Either `building_id` or `building_name` is required"
        }));
    }
    log.set_id(&body_code);

    log.step("saving via ictcell.wow_attendance_body_building_mapping_save");
    let result = sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_body_building_mapping_save($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&body_code)
    .bind(body.building_id)
    .bind(body.building_name.as_deref().map(str::trim))
    .bind(body.lat)
    .bind(body.long)
    .bind(body.radius)
    .bind(body.is_active)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(json) => {
            let ok = json
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if ok {
                log.step("mapping saved — returning 200");
                HttpResponse::Ok().json(json)
            } else {
                // Validation rejections from the function (bad coords, unknown
                // building, missing body_code) are caller errors, not failures.
                log.step("mapping rejected by validation — returning 400");
                HttpResponse::BadRequest().json(json)
            }
        }
        Err(err) => {
            eprintln!("DB error in wow_mapping_save: {err}");
            log.step(format!("DB mapping save FAILED: {err}"));
            HttpResponse::InternalServerError().json(json!({
                "success": false,
                "message": "Internal server error"
            }))
        }
    }
}

// ---------------------------------------------------------------------
// Tests
//
// `device_coords` is the contract between the mobile app and the location
// gate: whatever it returns None for becomes a 400 for an employee, so its
// accepted shapes are worth pinning down.
// ---------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn coords(v: Value) -> Option<(f64, f64)> {
        device_coords(Some(&v))
    }

    #[test]
    fn reads_numeric_lat_long() {
        assert_eq!(
            coords(json!({ "device_lat": 23.72815, "device_long": 90.39925 })),
            Some((23.72815, 90.39925))
        );
    }

    #[test]
    fn reads_numeric_strings() {
        // Phones commonly serialize GPS values as strings.
        assert_eq!(
            coords(json!({ "device_lat": "23.72815", "device_long": " 90.39925 " })),
            Some((23.72815, 90.39925))
        );
    }

    #[test]
    fn accepts_alternate_key_spellings() {
        assert_eq!(
            coords(json!({ "lat": 23.5, "lng": 90.5 })),
            Some((23.5, 90.5))
        );
        assert_eq!(
            coords(json!({ "latitude": 23.5, "longitude": 90.5 })),
            Some((23.5, 90.5))
        );
    }

    #[test]
    fn negative_coordinates_survive() {
        // Nothing in the campus data is negative, but the parser must not
        // quietly treat a southern/western hemisphere value as invalid.
        assert_eq!(
            coords(json!({ "device_lat": -33.86, "device_long": -70.66 })),
            Some((-33.86, -70.66))
        );
    }

    #[test]
    fn rejects_null_island() {
        // A phone that failed to get a fix reports 0/0. Treating that as a real
        // position would measure the distance from campus to the Atlantic and
        // reject the check-in with a nonsense distance instead of a clear error.
        assert_eq!(coords(json!({ "device_lat": 0, "device_long": 0 })), None);
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(coords(json!({ "device_lat": 91, "device_long": 90.0 })), None);
        assert_eq!(coords(json!({ "device_lat": 23.0, "device_long": 181 })), None);
    }

    #[test]
    fn rejects_partial_or_missing() {
        assert_eq!(coords(json!({ "device_lat": 23.72815 })), None);
        assert_eq!(coords(json!({ "device_long": 90.39925 })), None);
        assert_eq!(coords(json!({ "battery": 80 })), None);
        assert_eq!(device_coords(None), None);
    }

    #[test]
    fn rejects_unparseable_values() {
        assert_eq!(coords(json!({ "device_lat": "N/A", "device_long": "N/A" })), None);
        assert_eq!(coords(json!({ "device_lat": null, "device_long": null })), None);
    }

    #[test]
    fn device_info_without_coords_is_rejected() {
        // The shape a client sends today, before adding GPS. This is exactly the
        // case that must 400 rather than silently skip the geofence.
        assert_eq!(
            coords(json!({ "model": "Pixel 7", "os": "Android 15" })),
            None
        );
    }

    // ---- browsable_path -------------------------------------------------
    //
    // These run in-process with other tests, so they must not depend on
    // WOW_UPLOADS_SERVE_DIR being any particular value. Only the cases that
    // hold for BOTH the configured-prefix branch and the `/uploads/` fallback
    // are asserted here.

    #[test]
    fn browsable_path_maps_a_served_file_to_its_url() {
        // Absolute path containing /uploads/ — the shape every deployment uses.
        assert_eq!(
            browsable_path("/var/www/Rust/duerp/duerp-attendance/uploads/wow_attendance/live/a.jpg"),
            Some("/uploads/wow_attendance/live/a.jpg".to_string())
        );
        // Container path, different prefix, same URL.
        assert_eq!(
            browsable_path("/app/uploads/wow_attendance/enrolled/b.png"),
            Some("/uploads/wow_attendance/enrolled/b.png".to_string())
        );
        // Relative default.
        assert_eq!(
            browsable_path("./uploads/wow_attendance/live/c.jpg"),
            Some("/uploads/wow_attendance/live/c.jpg".to_string())
        );
    }

    #[test]
    fn browsable_path_escapes_what_would_break_a_pasted_url() {
        // Uploaded names routinely carry spaces; a raw space truncates the link.
        assert_eq!(
            browsable_path("/app/uploads/wow_attendance/enrolled/WhatsApp Image 2026-07-09.jpeg"),
            Some("/uploads/wow_attendance/enrolled/WhatsApp%20Image%202026-07-09.jpeg".to_string())
        );
        // `?` and `#` would otherwise start a query or fragment.
        assert_eq!(
            browsable_path("/app/uploads/wow_attendance/live/a#b?c.jpg"),
            Some("/uploads/wow_attendance/live/a%23b%3Fc.jpg".to_string())
        );
    }

    #[test]
    fn browsable_path_is_none_outside_the_served_folder() {
        // A file written somewhere with no /uploads/ segment has no URL, and
        // saying so beats inventing one that 404s.
        assert_eq!(browsable_path("/tmp/scratch/a.jpg"), None);
        assert_eq!(browsable_path("relative/a.jpg"), None);
    }
}
