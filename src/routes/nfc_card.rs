//! NFC card ↔ student mapping.
//!
//!   GET|POST /ext-api/nfc-card/get_card_info?card_number=...  (or in the body)
//!   POST /ext-api/nfc-card/save_card_info   (multipart/form-data)
//!
//! Requirement: `docs/nfc-card-reader-api-spec.md`. API reference:
//! `docs/nfc_card.md`. Schema and business rules: `sql/004_nfc_card.sql`.
//!
//! WHAT THIS MODULE DOES AND DOESN'T DECIDE
//! The rules that define a valid mapping — normalisation, the 1/2 enums, the
//! forced `is_verified = 2` for self-registration, card ownership and the
//! audit row — all live in `attendance.nfc_card_save_info`, because that
//! function is the only writer and must hold whether the caller is this
//! handler or a DBA at a psql prompt. This module owns the HTTP shape: parse
//! the multipart body, store the images on disk, map the function's `code`
//! onto a status, and turn stored paths into public URLs.
//!
//! It owns one rule of its own: a save must carry a `card_image` and a
//! `student_selfie`, and the two must be the same face. That check is a call
//! to the face-match service (`NFC_FACE_VERIFY_URL`) and happens BEFORE the
//! SQL function is reached, so a rejected pair leaves no row. It lives here
//! rather than in SQL because Postgres cannot make the HTTP call — which means
//! a DBA writing the row directly bypasses it, unlike every other rule above.
//!
//! AUTH is the bearer token, on top of the `X-App-Id` / `X-App-Password` +
//! IP allow-list gate `ExtAuthMiddleware` already applies to everything under
//! `/ext-api`. Note what that means for `force_reassign` — any holder of a
//! valid token, calling from an allow-listed IP, can move a card off another
//! student. The allow-list is what keeps that to trusted readers, so keep
//! those rows tight.
//!
//! THE RESPONSE ENVELOPE
//! Both endpoints answer with `status` ("success" | "error") plus a `message`,
//! and this module is the ONLY place in the service that does — everything
//! under `/ext-api/wow-attendance/*` still answers with a `success` boolean.
//! That is a deliberate, requested divergence for the NFC contract, not drift;
//! a client cannot share a response parser across the two modules.
//!
//! The envelope is built in SQL, not here, so a direct `psql` caller sees the
//! same shape the HTTP client does. This module only maps the `code` onto a
//! status line. Match on `code` — it is the stable machine-readable form; the
//! `message` is prose and may be reworded.

use actix_multipart::Multipart;
use actix_web::{post, route, web, HttpResponse};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::routes::wow_attendance::{
    browsable_path, detect_image_format, full_path, is_supported_image, log_local_response,
    max_upload_bytes, max_upload_mb_display, public_base_url, reduce_saved_image,
    require_token_caller,
};
use crate::utils::step_logger::{query_to_json, StepLogger};

// ---------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------

/// Base directory for card uploads, configurable via `NFC_UPLOAD_DIR`.
///
/// A sibling of the face-capture folder (`WOW_UPLOAD_DIR`), not a child of it:
/// both sit under the `uploads` tree the `/uploads` route serves, so a saved
/// file still resolves to a URL, but a card photo is not a face capture and
/// mixing them would put non-enrollment images inside the folder the AI
/// enrollment flow treats as its own.
fn nfc_upload_base() -> String {
    std::env::var("NFC_UPLOAD_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "./uploads/nfc_card".to_string())
}

fn card_image_dir() -> String {
    format!("{}/cards/", nfc_upload_base().trim_end_matches('/'))
}

fn selfie_dir() -> String {
    format!("{}/selfies/", nfc_upload_base().trim_end_matches('/'))
}

/// Reduce a client-supplied filename to something safe to append to a
/// directory path: basename only, and nothing outside `[A-Za-z0-9._-]`.
///
/// The uploaded name is attacker-controlled and never trusted for anything but
/// readability — a UUID supplies uniqueness. Stripping the directory part and
/// the separator character means no value can walk out of the upload folder,
/// rather than relying on the UUID prefix to happen to neutralise a leading
/// `..`. Long names are clipped so the result cannot exceed the filesystem's
/// per-component limit once the UUID is prepended.
fn safe_file_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('.');

    let cleaned: String = base
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .take(80)
        .collect();

    if cleaned.is_empty() {
        "upload".to_string()
    } else {
        cleaned
    }
}

/// Public URL for a stored image path, or `None` when the file landed outside
/// the served tree.
///
/// The spec's success body shows absolute URLs, so the request's own origin is
/// prepended to the `/uploads/...` path. Rows store the filesystem path (see
/// `sql/004_nfc_card.sql`), which is why this is computed per response rather
/// than persisted.
fn public_image_url(base_url: &str, stored: Option<&str>) -> Option<String> {
    let path = stored?;
    let rel = browsable_path(path)?;
    Some(format!("{}{}", base_url.trim_end_matches('/'), rel))
}

// ---------------------------------------------------------------------
// Error shaping
// ---------------------------------------------------------------------

/// A locally-produced failure (nothing reached the database), in the same
/// shape the SQL function's failures arrive in.
fn fail(code: &str, message: impl Into<String>) -> Value {
    json!({
        "status":  "error",
        "code":    code,
        "message": message.into(),
    })
}

/// Did the SQL layer report success?
///
/// One place, because `status` is a string now rather than a boolean: a typo in
/// the comparison would read as a silent failure on the happy path, and there
/// are two call sites that must not drift.
fn is_ok(body: &Value) -> bool {
    body.get("status").and_then(Value::as_str) == Some("success")
}

/// Map an error `code` from the SQL layer onto its HTTP status.
///
/// An unrecognised code is a 400, not a 500: every code the function emits is
/// a caller error, so a new one added there should read as a rejection rather
/// than as this service breaking.
fn status_for_code(code: &str) -> actix_web::http::StatusCode {
    use actix_web::http::StatusCode;
    match code {
        "card_not_found" => StatusCode::NOT_FOUND,
        "card_conflict" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    }
}

// ---------------------------------------------------------------------
// Face-match gate
//
// A card photo and a selfie only mean something together: the pair is what
// ties the student in front of the reader to the card being registered. That
// comparison is not made here — it is delegated to the face-match service at
// `NFC_FACE_VERIFY_URL`, which answers `match: true|false` for a `card_image`
// + `selfie_image` pair.
//
// The gate FAILS CLOSED. Anything short of an explicit `match: true` — a
// mismatch, a photo with no detectable face, an unreachable service, an unset
// URL — leaves the row unwritten. A card mapping that was never face-checked
// is the thing this gate exists to prevent, so "the checker was down" must not
// quietly become "saved anyway"; a save during an outage is an operator
// problem, not a silent one.
// ---------------------------------------------------------------------

/// Face-match endpoint, e.g. `http://10.224.224.101:8089/verify`.
///
/// `None` when unset, which rejects every save rather than letting unchecked
/// pairs through — see the fail-closed note above.
fn face_verify_url() -> Option<String> {
    std::env::var("NFC_FACE_VERIFY_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Sent as `X-API-Key`. A missing key is forwarded as an empty header rather
/// than omitted, so the failure surfaces as the service's own 401 in the step
/// log instead of as a differently-shaped request.
fn face_verify_api_key() -> String {
    std::env::var("NFC_FACE_VERIFY_API_KEY").unwrap_or_default()
}

/// Request timeout, `NFC_FACE_VERIFY_TIMEOUT_SECS` (default 30s).
///
/// It bounds a save: the caller is standing at a reader with the images
/// already on disk, so a stalled comparison has to become a 503 promptly
/// rather than hold the request open.
fn face_verify_timeout() -> Duration {
    let secs = std::env::var("NFC_FACE_VERIFY_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(30);
    Duration::from_secs(secs)
}

/// Shared client for the face-match service: connection + TLS reuse across
/// saves, and the timeout applied in one place.
static FACE_VERIFY_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(face_verify_timeout())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// 503 for "no verdict was obtained". Its own helper because the code string
/// has four call sites and a typo in one would read as a different failure.
fn face_verify_unavailable(message: &str) -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(fail("face_verify_unavailable", message))
}

/// Filename and MIME to forward one stored image under.
///
/// Both come from the magic bytes, never from the stored name: the uploaded
/// name is client text, and `reduce_saved_image` may have re-encoded the file
/// to JPEG without the extension following. The receiving service parses the
/// part's filename, so it gets a plain ASCII one.
fn forward_name_and_mime(field: &str, head: &[u8]) -> (String, &'static str) {
    let (ext, mime) = match detect_image_format(head) {
        "PNG" => ("png", "image/png"),
        "WEBP" => ("webp", "image/webp"),
        "BMP" => ("bmp", "image/bmp"),
        "HEIC" => ("heic", "image/heic"),
        "HEIF" => ("heif", "image/heif"),
        "AVIF" => ("avif", "image/avif"),
        _ => ("jpg", "image/jpeg"),
    };
    (format!("{field}.{ext}"), mime)
}

/// Read a stored upload back off disk as a multipart part.
async fn forward_part(field: &str, path: &str) -> Result<reqwest::multipart::Part, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read {path}: {e}"))?;
    let (filename, mime) = forward_name_and_mime(field, &bytes[..bytes.len().min(16)]);
    reqwest::multipart::Part::bytes(bytes)
        .file_name(filename)
        .mime_str(mime)
        .map_err(|e| e.to_string())
}

/// The human-readable half of the face service's reply.
///
/// It answers in two shapes: `detail` — a string for a rejection, a list of
/// `{msg: …}` for a schema error — and `message`, the prose beside `match`.
/// Worth passing on, because "No face detected in 'card_image'" tells a
/// student to retake the photo and a generic message would not.
fn face_verify_message(body: &Value) -> Option<String> {
    match body.get("detail") {
        Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.trim().to_string()),
        Some(Value::Array(items)) => {
            let joined = items
                .iter()
                .filter_map(|i| i.get("msg").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            if !joined.is_empty() {
                return Some(joined);
            }
        }
        _ => {}
    }
    body.get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Compare the two uploads and say whether the save may go ahead.
///
/// `Ok(())` only on an explicit `match: true`. Everything else is an
/// `Err(response)` the handler returns unchanged:
///
///   400 `face_mismatch`           — compared, and they are different people
///   400 `face_not_comparable`     — could not be compared (no face in one)
///   503 `face_verify_unavailable` — no verdict at all (unset, down, 5xx, 401)
///
/// The split is what the caller acts on: the first two are answered by
/// retaking a photo, the third only by an operator.
async fn verify_card_selfie_match(
    log: &StepLogger,
    card_path: &str,
    selfie_path: &str,
) -> Result<(), HttpResponse> {
    let Some(url) = face_verify_url() else {
        log.step("NFC_FACE_VERIFY_URL is not set — refusing the save (gate fails closed)");
        return Err(face_verify_unavailable(
            "Face verification is not configured; card not saved",
        ));
    };

    let mut form = reqwest::multipart::Form::new();
    for (field, path) in [("card_image", card_path), ("selfie_image", selfie_path)] {
        match forward_part(field, path).await {
            Ok(part) => form = form.part(field, part),
            Err(e) => {
                log.step(format!(
                    "{field} could not be read back for verification: {e}"
                ));
                return Err(face_verify_unavailable(
                    "Face verification could not read the uploaded images",
                ));
            }
        }
    }

    log.step(format!(
        "verifying card_image against student_selfie: POST {url}"
    ));
    let resp = match FACE_VERIFY_CLIENT
        .post(&url)
        .header("X-API-Key", face_verify_api_key())
        .multipart(form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log.step(format!("face verification request FAILED: {e}"));
            return Err(face_verify_unavailable(
                "Face verification service is unavailable; card not saved",
            ));
        }
    };

    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    let body: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            // Logged as the raw text, not dropped: a gateway's HTML error page
            // is the whole explanation when the service itself never answered.
            log.ai_response("/verify", status.as_u16(), &json!({ "non_json_body": raw }));
            log.step("face verification replied with a non-JSON body — refusing the save");
            return Err(face_verify_unavailable(
                "Face verification returned an unreadable response; card not saved",
            ));
        }
    };
    // Logged before the branches below, so a rejected pair is recorded in full
    // too — that reply is the one worth reading afterwards.
    log.ai_response("/verify", status.as_u16(), &body);

    // Our credentials, not the caller's photos: answering 400 here would send
    // a student off to retake a selfie over a misconfigured API key.
    if matches!(status.as_u16(), 401 | 403) {
        log.step("face verification rejected this service's API key — refusing the save");
        return Err(face_verify_unavailable(
            "Face verification credentials were rejected; card not saved",
        ));
    }
    if status.is_server_error() {
        log.step(format!(
            "face verification failed upstream ({}) — refusing the save",
            status.as_u16()
        ));
        return Err(face_verify_unavailable(
            "Face verification service is unavailable; card not saved",
        ));
    }
    // A 4xx here means the request was read but the pair could not be
    // compared — usually no detectable face in one of the photos. The
    // service's own wording is the actionable one.
    if status.is_client_error() {
        let message = face_verify_message(&body)
            .unwrap_or_else(|| "Face verification could not compare the images".to_string());
        log.step(format!(
            "face verification could not compare the images ({}) — returning 400",
            status.as_u16()
        ));
        return Err(HttpResponse::BadRequest().json(fail("face_not_comparable", message)));
    }

    let null = Value::Null;
    log.step(format!(
        "face verification verdict: match={} similarity={} threshold={}",
        body.get("match").unwrap_or(&null),
        body.get("similarity").unwrap_or(&null),
        body.get("threshold").unwrap_or(&null),
    ));

    match body.get("match").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => {
            // Our own message, because `code` is the contract and the
            // service's prose may be reworded; its wording and the scores ride
            // along in `data` for the UI and for anyone reading a screenshot.
            let mut out = fail(
                "face_mismatch",
                "The student selfie does not match the face on the card image; card not saved",
            );
            if let Some(obj) = out.as_object_mut() {
                obj.insert(
                    "data".to_string(),
                    json!({
                        "match":      false,
                        "similarity": body.get("similarity").cloned().unwrap_or(Value::Null),
                        "threshold":  body.get("threshold").cloned().unwrap_or(Value::Null),
                        "detail":     face_verify_message(&body),
                    }),
                );
            }
            Err(HttpResponse::BadRequest().json(out))
        }
        // A 200 with no `match` field is not a verdict. Fails closed with the
        // other no-verdict cases rather than being read as either answer.
        None => {
            log.step("face verification reply carried no `match` field — refusing the save");
            Err(face_verify_unavailable(
                "Face verification returned an unexpected response; card not saved",
            ))
        }
    }
}

// ---------------------------------------------------------------------
// 1 · get_card_info — scan lookup
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GetCardInfoQuery {
    // Optional so extraction never fails; a missing value is reported as a
    // 400 with a message naming the parameter, which beats actix's own
    // "missing field" error text for a reader debugging a scanner.
    pub card_number: Option<String>,
}

/// Pull `card_number` out of a POST body, whichever way the client encoded it.
///
/// A lookup carrying one scalar can arrive four different ways in practice, and
/// a client that guessed differently from us should not get a 400 that reads
/// like the card is unknown. Handled here rather than by an extractor because
/// `web::Json` / `Multipart` each commit to ONE encoding and fail the request
/// outright on the others.
///
/// Returns `None` for a GET (no body), an empty body, or a body with no such
/// field — the caller then falls back to the query string.
fn card_number_from_body(content_type: &str, body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let ct = content_type.to_ascii_lowercase();

    // multipart/form-data — what Postman sends by default for a POST.
    if ct.starts_with("multipart/") {
        return multipart_text_field(body, "card_number");
    }

    // application/json
    if ct.contains("json") {
        return serde_json::from_slice::<Value>(body)
            .ok()?
            .get("card_number")
            .and_then(|v| match v {
                // A numeric UID sent unquoted is still a card number.
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
    }

    // application/x-www-form-urlencoded
    if ct.contains("x-www-form-urlencoded") {
        let text = String::from_utf8_lossy(body);
        return query_to_json(&text)
            .get("card_number")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
    }

    // No usable Content-Type. Rather than give up, try the two encodings a body
    // can be identified from its own bytes — a client that sent no header at
    // all is common enough to be worth rescuing.
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("card_number")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            query_to_json(&String::from_utf8_lossy(body))
                .get("card_number")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
        })
}

/// Read one simple text field out of a `multipart/form-data` body.
///
/// Deliberately not the full `actix_multipart` machinery: that extractor owns
/// the payload stream, and this endpoint needs the raw bytes so the other three
/// encodings above stay reachable. Scoped to what a scalar field looks like —
/// find the part whose `name="…"` matches, skip its headers at the blank line,
/// and take everything up to the next boundary. Anything more exotic (files,
/// nested multipart, base64 transfer-encoding) is not this endpoint's business.
fn multipart_text_field(body: &[u8], field: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let needle = format!("name=\"{field}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];

    // End of this part's headers: the first blank line, either line ending.
    let value_start = rest
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| rest.find("\n\n").map(|i| i + 2))?;
    let value = &rest[value_start..];

    // The next boundary begins with `--`, on its own line.
    let end = value
        .find("\r\n--")
        .or_else(|| value.find("\n--"))
        .unwrap_or(value.len());

    let out = value[..end].trim();
    if out.is_empty() {
        None
    } else {
        Some(out.to_string())
    }
}

// Both methods are accepted. POST is the one to use — every other endpoint in
// this service is POST, including the read-only ones, and a card reader posting
// to all of them should not have to special-case this path. GET is kept because
// the spec (docs/nfc-card-reader-api-spec.md) documents it and it is the honest
// method for a lookup that changes nothing.
//
// The IP allow-list matches on path only, so one `ext_api_allowed_ips` row
// covers both methods.
#[route("/nfc-card/get_card_info", method = "GET", method = "POST")]
pub async fn nfc_get_card_info(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<GetCardInfoQuery>,
    body: web::Bytes,
) -> HttpResponse {
    let log = StepLogger::new("ext-api/nfc-card/get_card_info");
    log.set_base_url(&public_base_url(&req));
    log.set_endpoint(req.method().as_str(), &full_path(&req));
    log.params("query", &query_to_json(req.query_string()));
    let resp = nfc_get_card_info_inner(&log, req, db, query, body).await;
    log_local_response(&log, resp).await
}

async fn nfc_get_card_info_inner(
    log: &StepLogger,
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<GetCardInfoQuery>,
    body: web::Bytes,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    let (token_user_id, token_line) = match require_token_caller(&req) {
        Ok(v) => v,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    log.step(token_line);

    // Query string first, then the body — a caller who sent both meant the one
    // they had to work harder for, and the query string is what GET clients and
    // the spec use.
    let from_query = query
        .card_number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let content_type = req
        .headers()
        .get(actix_web::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let card_number = match from_query {
        Some(c) => c,
        None => match card_number_from_body(content_type, &body) {
            Some(c) => {
                // Record where it came from: a reader whose query string is
                // being stripped in transit looks identical to one that never
                // sent it, and this line is what separates them.
                log.params(
                    "body",
                    &json!({ "card_number": c.trim(), "content_type": content_type }),
                );
                c.trim().to_string()
            }
            None => {
                log.step(format!(
                    "`card_number` missing from query and body (method={}, content_type={}, body_bytes={}) — returning 400",
                    req.method(),
                    if content_type.is_empty() { "<none>" } else { content_type },
                    body.len()
                ));
                return HttpResponse::BadRequest()
                    .json(fail("missing_card_number", "card_number is required"));
            }
        },
    };
    let card_number = card_number.as_str();
    // The scanned value, not the person: this log is filed per card so a
    // reader that keeps failing can be traced through the folder.
    log.set_id(card_number);

    log.step(format!(
        "looking up card via attendance.nfc_card_get_info (lookup by token user id={token_user_id})"
    ));
    let result = sqlx::query_scalar::<_, Value>("SELECT attendance.nfc_card_get_info($1)")
        .bind(card_number)
        .fetch_one(db.get_ref())
        .await;

    match result {
        Ok(body) => {
            if is_ok(&body) {
                log.step("card found — returning 200");
                return HttpResponse::Ok().json(body);
            }
            let code = body
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = status_for_code(&code);
            log.step(format!(
                "lookup rejected (code={code}) — returning {}",
                status.as_u16()
            ));
            HttpResponse::build(status).json(body)
        }
        Err(err) => {
            eprintln!("DB error in nfc_get_card_info: {err}");
            log.step(format!("DB card lookup FAILED: {err}"));
            HttpResponse::InternalServerError().json(fail(
                "internal_error",
                "Internal server error",
            ))
        }
    }
}

// ---------------------------------------------------------------------
// 2 · save_card_info — create / update a card record
// ---------------------------------------------------------------------

/// Scalar fields also accepted as query params.
///
/// The spec puts everything in the multipart body, and that is the documented
/// path. These exist for the same reason `EnrollQuery` does: several HTTP
/// clients drop the query string on a multipart POST, and some send the
/// scalars there anyway. The body wins when both are present.
#[derive(Deserialize)]
pub struct SaveCardInfoQuery {
    pub student_applicant_id: Option<String>,
    pub card_number: Option<String>,
    pub is_verified: Option<String>,
    pub registration_type: Option<String>,
    pub force_reassign: Option<String>,
}

/// One image field of the request, after it has been written to disk.
struct StoredImage {
    /// Filesystem path — what goes in the row.
    path: String,
    /// Field name, for log lines.
    field: &'static str,
}

/// Parse the `1` / `2` enums. Returned as `Option<i16>` for the SQL bind;
/// `Err` marks a value that was supplied but is not a number, which must be a
/// 400 rather than silently becoming "omitted".
fn parse_flag(raw: Option<&str>) -> Result<Option<i16>, ()> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s.parse::<i16>().map(Some).map_err(|_| ()),
    }
}

/// `force_reassign` accepts the spellings a form post actually produces —
/// `true`, `1`, `yes`, `on` — because a checkbox serialises as `on` and a JSON
/// client as `true`, and a caller who asked for a reassignment and silently
/// got a 409 has no way to tell which spelling was wrong.
fn parse_bool(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "true" | "1" | "yes" | "on"
    )
}

#[post("/nfc-card/save_card_info")]
pub async fn nfc_save_card_info(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<SaveCardInfoQuery>,
    payload: Multipart,
) -> HttpResponse {
    let log = StepLogger::new("ext-api/nfc-card/save_card_info");
    log.set_base_url(&public_base_url(&req));
    log.set_endpoint(req.method().as_str(), &full_path(&req));
    log.params("query", &query_to_json(req.query_string()));
    let resp = nfc_save_card_info_inner(&log, req, db, query, payload).await;
    log_local_response(&log, resp).await
}

async fn nfc_save_card_info_inner(
    log: &StepLogger,
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<SaveCardInfoQuery>,
    mut payload: Multipart,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    let (token_user_id, token_line) = match require_token_caller(&req) {
        Ok(v) => v,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    log.step(token_line);

    // Seeded from the query string, overwritten by the body when it carries
    // the field.
    let mut student_applicant_id = query.student_applicant_id.clone();
    let mut card_number = query.card_number.clone();
    let mut is_verified_raw = query.is_verified.clone();
    let mut registration_type_raw = query.registration_type.clone();
    let mut force_reassign_raw = query.force_reassign.clone();
    let mut card_image: Option<StoredImage> = None;
    let mut student_selfie: Option<StoredImage> = None;

    log.step("parsing multipart body");
    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(f) => f,
            Err(e) => {
                log.step(format!("multipart field error: {e}"));
                return HttpResponse::BadRequest()
                    .json(fail("bad_multipart", format!("Multipart error: {e}")));
            }
        };

        let cd = field.content_disposition();
        let field_name = cd.and_then(|c| c.get_name()).unwrap_or("").to_string();
        let filename = cd.and_then(|c| c.get_filename()).unwrap_or("").to_string();

        match field_name.as_str() {
            "student_applicant_id"
            | "card_number"
            | "is_verified"
            | "registration_type"
            | "force_reassign" => {
                let mut data = Vec::new();
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(bytes) => data.extend_from_slice(&bytes),
                        Err(e) => {
                            log.step(format!("field '{field_name}' read error: {e}"));
                            return HttpResponse::BadRequest()
                                .json(fail("bad_multipart", format!("Field read error: {e}")));
                        }
                    }
                }
                let text = String::from_utf8_lossy(&data).to_string();
                // The value, not just the name — a wrong `registration_type`
                // or a card number with an unexpected separator is invisible
                // otherwise. Secret-ish names are masked by the logger.
                let mut param = serde_json::Map::new();
                param.insert(field_name.clone(), Value::String(text.clone()));
                log.params("form", &Value::Object(param));

                let value = Some(text).filter(|s| !s.trim().is_empty());
                match field_name.as_str() {
                    "student_applicant_id" => {
                        student_applicant_id = value.or(student_applicant_id)
                    }
                    "card_number" => card_number = value.or(card_number),
                    "is_verified" => is_verified_raw = value.or(is_verified_raw),
                    "registration_type" => {
                        registration_type_raw = value.or(registration_type_raw)
                    }
                    "force_reassign" => force_reassign_raw = value.or(force_reassign_raw),
                    _ => {}
                }
            }

            "card_image" | "student_selfie" if !filename.is_empty() => {
                // Static because it is logged and matched on below; the
                // borrowed `field_name` does not outlive the loop iteration.
                let (slot_name, dir): (&'static str, String) = if field_name == "card_image" {
                    ("card_image", card_image_dir())
                } else {
                    ("student_selfie", selfie_dir())
                };

                if !Path::new(&dir).exists() {
                    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                        log.step(format!("upload dir create FAILED ({dir}): {e}"));
                        return HttpResponse::InternalServerError().json(fail(
                            "internal_error",
                            format!("Directory create failed: {e}"),
                        ));
                    }
                    log.step(format!("upload dir created: {dir}"));
                }

                let part_ct = field
                    .content_type()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "<none>".into());
                let stored_name = format!("{}_{}", Uuid::new_v4(), safe_file_name(&filename));
                let filepath = format!("{dir}{stored_name}");
                log.step(format!(
                    "receiving {slot_name} '{filename}' (part_ct={part_ct}) -> {stored_name}"
                ));

                let mut f = match File::create(&filepath).await {
                    Ok(file) => file,
                    Err(e) => {
                        log.step(format!("{slot_name} file create FAILED: {e}"));
                        return HttpResponse::InternalServerError()
                            .json(fail("internal_error", format!("File create error: {e}")));
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
                            if written > max_upload_bytes() {
                                log.step(format!(
                                    "{slot_name} exceeds upload ceiling ({written} bytes) — rejecting (partial file kept)"
                                ));
                                return HttpResponse::PayloadTooLarge().json(fail(
                                    "image_too_large",
                                    format!(
                                        "Each image must be at most {}",
                                        max_upload_mb_display()
                                    ),
                                ));
                            }
                            if let Err(e) = f.write_all(&bytes).await {
                                log.step(format!("{slot_name} write FAILED: {e}"));
                                return HttpResponse::InternalServerError().json(fail(
                                    "internal_error",
                                    format!("File write error: {e}"),
                                ));
                            }
                        }
                        Err(e) => {
                            log.step(format!("{slot_name} chunk read error: {e}"));
                            return HttpResponse::BadRequest()
                                .json(fail("bad_multipart", format!("File chunk error: {e}")));
                        }
                    }
                }
                if let Err(e) = f.flush().await {
                    log.step(format!("{slot_name} flush FAILED: {e}"));
                    return HttpResponse::InternalServerError()
                        .json(fail("internal_error", format!("File flush error: {e}")));
                }
                drop(f);

                // Trust the magic bytes, not the extension or the part's
                // Content-Type: both are client-supplied, and a renamed PDF
                // must not end up stored as somebody's card photo.
                let detected = detect_image_format(&head);
                log.step(format!(
                    "{slot_name} stored: {written} bytes, detected={detected}"
                ));
                if !is_supported_image(&head) {
                    log.step(format!(
                        "{slot_name} is not an image (detected={detected}) — rejecting (file kept for audit)"
                    ));
                    return HttpResponse::BadRequest().json(fail(
                        "invalid_image",
                        "Invalid image file",
                    ));
                }

                // Oversized-but-accepted photos are compressed down in place,
                // so what is stored is what the limit describes. Returns the
                // possibly-renamed path.
                let final_path = reduce_saved_image(&filepath).await;
                log.file(&final_path, browsable_path(&final_path).as_deref());

                let slot = if slot_name == "card_image" {
                    &mut card_image
                } else {
                    &mut student_selfie
                };
                // Last one wins if a client sends the field twice; the earlier
                // file stays on disk rather than being deleted, matching how
                // every other upload in this service is treated.
                *slot = Some(StoredImage {
                    path: final_path,
                    field: slot_name,
                });
            }

            // A file part with no filename is an empty file input — the shape
            // a browser sends for "no file chosen". Drained, not rejected.
            other => {
                let mut dropped = 0usize;
                while let Some(chunk) = field.next().await {
                    match chunk {
                        Ok(b) => dropped += b.len(),
                        Err(_) => break,
                    }
                }
                if !other.is_empty() {
                    log.step(format!(
                        "ignoring unexpected field '{other}' ({dropped} bytes drained)"
                    ));
                }
            }
        }
    }

    // ── Validate what the handler owns ───────────────────────────────
    // The SQL function re-checks all of this; these branches exist so a bad
    // request is answered before an image is committed to a row, and so the
    // message names the offending field.
    let applicant = student_applicant_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let card = card_number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(a) = applicant {
        log.set_id(a);
    }

    let is_verified = match parse_flag(is_verified_raw.as_deref()) {
        Ok(v) => v,
        Err(()) => {
            log.step("`is_verified` is not a number — returning 400");
            return HttpResponse::BadRequest().json(fail(
                "invalid_value",
                "is_verified/registration_type must be 1 or 2",
            ));
        }
    };
    let registration_type = match parse_flag(registration_type_raw.as_deref()) {
        Ok(v) => v,
        Err(()) => {
            log.step("`registration_type` is not a number — returning 400");
            return HttpResponse::BadRequest().json(fail(
                "invalid_value",
                "is_verified/registration_type must be 1 or 2",
            ));
        }
    };

    if applicant.is_none() || card.is_none() || registration_type.is_none() {
        log.step("required field missing — returning 400");
        return HttpResponse::BadRequest().json(fail(
            "missing_fields",
            "student_applicant_id, card_number, and registration_type are required",
        ));
    }

    // ── The face-match gate ──────────────────────────────────────────
    // Both photos are required on EVERY save, including one that only flips
    // `is_verified`: the pair is what the gate below compares, and a save that
    // could omit them would be a save that skips the check. The row keeps its
    // stored images when a save omits one (COALESCE, in SQL), but that path is
    // no longer reachable through this endpoint.
    let (Some(card_img), Some(selfie_img)) = (card_image.as_ref(), student_selfie.as_ref()) else {
        log.step("card_image and/or student_selfie missing — returning 400");
        return HttpResponse::BadRequest().json(fail(
            "missing_images",
            "card_image and student_selfie are both required",
        ));
    };

    // Before the write, never after: a pair the service rejects must leave no
    // row behind. The files stay on disk either way, like every other rejected
    // upload here, and are referenced by nothing.
    if let Err(resp) = verify_card_selfie_match(log, &card_img.path, &selfie_img.path).await {
        return resp;
    }

    let force_reassign = parse_bool(force_reassign_raw.as_deref());
    let stored_paths: Vec<&str> = [card_image.as_ref(), student_selfie.as_ref()]
        .iter()
        .flatten()
        .map(|img| img.field)
        .collect();
    log.step(format!(
        "saving via attendance.nfc_card_save_info (images={:?}, force_reassign={force_reassign}, \
         write by token user id={token_user_id})",
        stored_paths
    ));

    let result = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.nfc_card_save_info($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(applicant)
    .bind(card)
    .bind(is_verified)
    .bind(registration_type)
    .bind(card_image.as_ref().map(|i| i.path.as_str()))
    .bind(student_selfie.as_ref().map(|i| i.path.as_str()))
    .bind(force_reassign)
    .bind(token_user_id)
    .bind(&client_ip)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(mut body) => {
            if !is_ok(&body) {
                let code = body
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let status = status_for_code(&code);
                log.step(format!(
                    "save rejected (code={code}) — returning {}",
                    status.as_u16()
                ));
                return HttpResponse::build(status).json(body);
            }

            // Rows hold filesystem paths; the response owes the caller URLs.
            let base = public_base_url(&req);
            if let Some(data) = body.get_mut("data").and_then(Value::as_object_mut) {
                for key in ["card_image", "student_selfie"] {
                    let stored = data.get(key).and_then(Value::as_str).map(str::to_string);
                    let url = public_image_url(&base, stored.as_deref());
                    // A path outside the served tree has no URL; report null
                    // rather than a link that 404s. `stored` being None is the
                    // ordinary "no image on file" case and lands here too.
                    data.insert(
                        key.to_string(),
                        url.map(Value::String).unwrap_or(Value::Null),
                    );
                }
            }

            log.step("card info saved — returning 200");
            HttpResponse::Ok().json(body)
        }
        Err(err) => {
            eprintln!("DB error in nfc_save_card_info: {err}");
            log.step(format!("DB card save FAILED: {err}"));
            HttpResponse::InternalServerError().json(fail(
                "internal_error",
                "Internal server error",
            ))
        }
    }
}

// ---------------------------------------------------------------------
// Tests
//
// The filename sanitiser and the two form-value parsers are the parts of this
// module that turn attacker- or client-controlled text into a path or a
// decision, so they are the parts worth pinning down. Normalisation and the
// business rules are tested against the database in
// `tests/nfc_card_sql.rs`, because that is where they are implemented.
// ---------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_file_name_keeps_ordinary_names() {
        assert_eq!(safe_file_name("card.jpg"), "card.jpg");
        assert_eq!(safe_file_name("my-card_01.PNG"), "my-card_01.PNG");
    }

    #[test]
    fn safe_file_name_strips_any_directory_part() {
        // The whole point: nothing that reaches the path can contain a
        // separator, so no value can walk out of the upload folder.
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("/etc/passwd"), "passwd");
        assert_eq!(safe_file_name(r"..\..\windows\system32\a.jpg"), "a.jpg");
        assert!(!safe_file_name("../../etc/passwd").contains('/'));
    }

    #[test]
    fn safe_file_name_neutralises_leading_dots() {
        // A name that is only dots would otherwise yield "." or "..", which
        // name a directory rather than a file.
        assert_eq!(safe_file_name(".."), "upload");
        assert_eq!(safe_file_name("."), "upload");
        assert_eq!(safe_file_name(".hidden.jpg"), "hidden.jpg");
    }

    #[test]
    fn safe_file_name_replaces_shell_and_url_hostile_characters() {
        assert_eq!(safe_file_name("a b;c$d.jpg"), "a_b_c_d.jpg");
        assert_eq!(safe_file_name("card#1?x.jpg"), "card_1_x.jpg");
    }

    #[test]
    fn safe_file_name_falls_back_when_nothing_survives() {
        assert_eq!(safe_file_name(""), "upload");
        assert_eq!(safe_file_name("   "), "upload");
    }

    #[test]
    fn safe_file_name_is_bounded() {
        // A 300-character name plus a UUID prefix would exceed the 255-byte
        // per-component limit on ext4 and fail the write.
        let long = "a".repeat(300);
        assert_eq!(safe_file_name(&long).len(), 80);
    }

    #[test]
    fn parse_flag_reads_the_enum_values() {
        assert_eq!(parse_flag(Some("1")), Ok(Some(1)));
        assert_eq!(parse_flag(Some(" 2 ")), Ok(Some(2)));
    }

    #[test]
    fn parse_flag_treats_absent_and_empty_as_omitted() {
        // An empty form field is "not sent", so the DB default applies rather
        // than the request being rejected.
        assert_eq!(parse_flag(None), Ok(None));
        assert_eq!(parse_flag(Some("")), Ok(None));
        assert_eq!(parse_flag(Some("   ")), Ok(None));
    }

    #[test]
    fn parse_flag_rejects_non_numeric() {
        // Must not silently become "omitted": a client sending "yes" meant
        // something, and the DB default would quietly contradict it.
        assert_eq!(parse_flag(Some("yes")), Err(()));
        assert_eq!(parse_flag(Some("1.0")), Err(()));
    }

    #[test]
    fn parse_flag_passes_out_of_range_values_through() {
        // Range is the SQL function's call, so that one message decides it for
        // every caller; this only rejects what is not a number at all.
        assert_eq!(parse_flag(Some("3")), Ok(Some(3)));
        assert_eq!(parse_flag(Some("0")), Ok(Some(0)));
    }

    #[test]
    fn forward_name_and_mime_reads_the_magic_bytes() {
        // What the part is labelled with must follow the bytes, not the
        // stored extension: `reduce_saved_image` re-encodes to JPEG and a
        // client can name a PNG anything at all.
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            forward_name_and_mime("card_image", &png),
            ("card_image.png".to_string(), "image/png")
        );
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(
            forward_name_and_mime("selfie_image", &jpeg),
            ("selfie_image.jpg".to_string(), "image/jpeg")
        );
    }

    #[test]
    fn forward_name_and_mime_falls_back_to_jpeg() {
        // Unrecognised bytes can't reach here — `is_supported_image` rejected
        // them at upload — so the fallback only has to be a valid label.
        assert_eq!(
            forward_name_and_mime("card_image", &[0, 1, 2, 3]),
            ("card_image.jpg".to_string(), "image/jpeg")
        );
    }

    #[test]
    fn face_verify_message_reads_a_string_detail() {
        // The shape a rejection arrives in, and the one worth showing the
        // student verbatim.
        let body = json!({ "detail": "No face detected in 'card_image'." });
        assert_eq!(
            face_verify_message(&body).as_deref(),
            Some("No face detected in 'card_image'.")
        );
    }

    #[test]
    fn face_verify_message_joins_a_validation_detail_list() {
        let body = json!({
            "detail": [
                { "type": "missing", "loc": ["body", "selfie_image"], "msg": "Field required" },
                { "type": "missing", "loc": ["body", "card_image"], "msg": "Field required" }
            ]
        });
        assert_eq!(
            face_verify_message(&body).as_deref(),
            Some("Field required; Field required")
        );
    }

    #[test]
    fn face_verify_message_falls_back_to_the_verdict_prose() {
        let body = json!({ "match": false, "message": "NO MATCH — different people" });
        assert_eq!(
            face_verify_message(&body).as_deref(),
            Some("NO MATCH — different people")
        );
    }

    #[test]
    fn face_verify_message_is_none_when_there_is_nothing_to_say() {
        assert_eq!(face_verify_message(&json!({ "match": true })), None);
        assert_eq!(face_verify_message(&json!({ "detail": "   " })), None);
        assert_eq!(face_verify_message(&json!({ "detail": [] })), None);
    }

    #[test]
    fn parse_bool_accepts_the_spellings_forms_actually_send() {
        for s in ["true", "TRUE", "1", "yes", "on", " true "] {
            assert!(parse_bool(Some(s)), "{s:?} should be true");
        }
    }

    #[test]
    fn parse_bool_defaults_to_false() {
        // Reassignment takes a card off another student, so anything that is
        // not an explicit yes must not trigger it.
        for s in ["false", "0", "no", "off", "", "maybe"] {
            assert!(!parse_bool(Some(s)), "{s:?} should be false");
        }
        assert!(!parse_bool(None));
    }

    #[test]
    fn fail_builds_the_error_envelope() {
        let out = fail("missing_card_number", "card_number is required");
        assert_eq!(out["status"], "error");
        assert_eq!(out["message"], "card_number is required");
        assert_eq!(out["code"], "missing_card_number");
        // The boolean the rest of the service uses is deliberately gone here;
        // a client must read `status`.
        assert!(out.get("success").is_none());
        assert!(out.get("error").is_none());
    }

    #[test]
    fn is_ok_reads_the_status_string() {
        assert!(is_ok(&json!({ "status": "success", "message": "Data Found" })));
        assert!(!is_ok(&json!({ "status": "error", "code": "card_not_found" })));
    }

    #[test]
    fn is_ok_is_false_for_anything_unexpected() {
        // Fails closed: a body whose status is missing, misspelled, or of the
        // wrong type must never be reported to the caller as a success.
        assert!(!is_ok(&json!({})));
        assert!(!is_ok(&json!({ "status": "Success" })));   // wrong case
        assert!(!is_ok(&json!({ "status": true })));        // wrong type
        assert!(!is_ok(&json!({ "success": true })));       // the old envelope
        assert!(!is_ok(&Value::Null));
    }

    #[test]
    fn status_for_code_maps_the_known_codes() {
        assert_eq!(status_for_code("card_not_found").as_u16(), 404);
        assert_eq!(status_for_code("card_conflict").as_u16(), 409);
        assert_eq!(status_for_code("missing_fields").as_u16(), 400);
        assert_eq!(status_for_code("invalid_card_number").as_u16(), 400);
        // An unknown code is a caller error, not a 500.
        assert_eq!(status_for_code("something_new").as_u16(), 400);
        assert_eq!(status_for_code("").as_u16(), 400);
    }

    #[test]
    fn public_image_url_joins_origin_and_served_path() {
        assert_eq!(
            public_image_url(
                "https://api.atten.du.ac.bd",
                Some("/app/uploads/nfc_card/cards/abc_card.jpg")
            ),
            Some("https://api.atten.du.ac.bd/uploads/nfc_card/cards/abc_card.jpg".to_string())
        );
        // A trailing slash on the origin must not double up.
        assert_eq!(
            public_image_url(
                "https://api.atten.du.ac.bd/",
                Some("/app/uploads/nfc_card/selfies/abc.jpg")
            ),
            Some("https://api.atten.du.ac.bd/uploads/nfc_card/selfies/abc.jpg".to_string())
        );
    }

    // ---- card_number_from_body ------------------------------------------
    //
    // Four encodings a POST can carry the same scalar in. Each is a real
    // client's default, so each has to resolve to the same card.

    const CARD: &str = "04A1B2C3D4E5";

    #[test]
    fn body_reads_json() {
        assert_eq!(
            card_number_from_body("application/json", br#"{"card_number":"04A1B2C3D4E5"}"#),
            Some(CARD.to_string())
        );
        // charset parameter must not defeat the match
        assert_eq!(
            card_number_from_body(
                "application/json; charset=utf-8",
                br#"{"card_number":"04A1B2C3D4E5"}"#
            ),
            Some(CARD.to_string())
        );
    }

    #[test]
    fn body_reads_an_unquoted_json_number() {
        // A decimal-UID reader that serialises the id as a JSON number. Losing
        // this would 400 a perfectly good scan.
        assert_eq!(
            card_number_from_body("application/json", br#"{"card_number":1234567890}"#),
            Some("1234567890".to_string())
        );
    }

    #[test]
    fn body_reads_form_urlencoded() {
        assert_eq!(
            card_number_from_body(
                "application/x-www-form-urlencoded",
                b"card_number=04A1B2C3D4E5"
            ),
            Some(CARD.to_string())
        );
        // Other fields alongside it, and percent-encoding.
        assert_eq!(
            card_number_from_body(
                "application/x-www-form-urlencoded",
                b"foo=1&card_number=04%3AA1%3AB2&bar=2"
            ),
            Some("04:A1:B2".to_string())
        );
    }

    #[test]
    fn body_reads_multipart() {
        // Postman's default POST body.
        let body = b"------WebKitFormBoundaryABC\r\n\
Content-Disposition: form-data; name=\"card_number\"\r\n\
\r\n\
04A1B2C3D4E5\r\n\
------WebKitFormBoundaryABC--\r\n";
        assert_eq!(
            card_number_from_body(
                "multipart/form-data; boundary=----WebKitFormBoundaryABC",
                body
            ),
            Some(CARD.to_string())
        );
    }

    #[test]
    fn body_reads_multipart_with_other_fields_around_it() {
        // The field must be found by name, not by position.
        let body = b"--X\r\n\
Content-Disposition: form-data; name=\"other\"\r\n\
\r\n\
ignore-me\r\n\
--X\r\n\
Content-Disposition: form-data; name=\"card_number\"\r\n\
\r\n\
AABBCCDD\r\n\
--X--\r\n";
        assert_eq!(
            card_number_from_body("multipart/form-data; boundary=X", body),
            Some("AABBCCDD".to_string())
        );
    }

    #[test]
    fn body_rescues_a_client_that_sent_no_content_type() {
        // Common enough with embedded HTTP stacks to be worth handling.
        assert_eq!(
            card_number_from_body("", br#"{"card_number":"04A1B2C3D4E5"}"#),
            Some(CARD.to_string())
        );
        assert_eq!(
            card_number_from_body("", b"card_number=04A1B2C3D4E5"),
            Some(CARD.to_string())
        );
    }

    #[test]
    fn body_is_none_when_there_is_nothing_to_read() {
        // A GET has no body; the caller then uses the query string.
        assert_eq!(card_number_from_body("", b""), None);
        assert_eq!(card_number_from_body("application/json", b""), None);
        // Present but not carrying this field.
        assert_eq!(
            card_number_from_body("application/json", br#"{"something_else":"x"}"#),
            None
        );
        assert_eq!(
            card_number_from_body("application/x-www-form-urlencoded", b"other=1"),
            None
        );
        // Malformed JSON must not panic.
        assert_eq!(card_number_from_body("application/json", b"{not json"), None);
    }

    #[test]
    fn multipart_field_ignores_an_empty_value() {
        // An empty field is "not sent", so the query string still gets a turn
        // rather than the request failing outright.
        let body = b"--X\r\nContent-Disposition: form-data; name=\"card_number\"\r\n\r\n\r\n--X--\r\n";
        assert_eq!(multipart_text_field(body, "card_number"), None);
    }

    #[test]
    fn multipart_field_handles_bare_newline_endings() {
        // Some embedded clients emit LF rather than CRLF.
        let body = b"--X\nContent-Disposition: form-data; name=\"card_number\"\n\n04A1B2C3D4E5\n--X--\n";
        assert_eq!(
            multipart_text_field(body, "card_number"),
            Some(CARD.to_string())
        );
    }

    #[test]
    fn public_image_url_is_none_without_a_servable_path() {
        assert_eq!(public_image_url("https://x.test", None), None);
        // Outside the served tree — no URL exists, and inventing one that
        // 404s would be worse than reporting null.
        assert_eq!(public_image_url("https://x.test", Some("/tmp/a.jpg")), None);
    }
}
