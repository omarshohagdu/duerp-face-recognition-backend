//! The caller's own access profile.
//!
//!   GET|POST /ext-api/me/access?platform=desktop|mobile|kiosk
//!
//! One call, made by the desktop SPA and the mobile app right after login, that
//! answers "who am I, what may I do, what do I show". Design:
//! `docs/access_control.md` §14. The payload is built by
//! `attendance.access_profile`, so a psql caller sees exactly what the HTTP
//! client does.
//!
//! WHAT THIS IS NOT
//! **It is not a gate.** The menu and the permission list it returns are
//! rendering hints; `attendance.ext_api_can_call` — applied by
//! `ExtAuthMiddleware` on every request — is what actually refuses. A client
//! that trusts this payload for anything but drawing a screen has no access
//! control at all, and a mobile binary is fully inspectable by whoever installs
//! it. The two read the same tables so they cannot disagree about a person; only
//! one of them enforces.
//!
//! IT ANSWERS ONLY ABOUT THE CALLER. The person id comes from the bearer token
//! and there is deliberately no parameter for it: an endpoint that let you ask
//! about somebody else would be an org-chart leak, and a tempting one.
//!
//! AUTH is the bearer token, on top of the `X-App-Id` / `X-App-Password` + IP
//! allow-list gate `ExtAuthMiddleware` applies to everything under `/ext-api`.
//! Its own rule row carries `resource_key = NULL` — open to any authenticated
//! caller — because a client cannot render anything at all without it.

use actix_web::{route, web, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::routes::nfc_card::scalar_from_body;
use crate::routes::wow_attendance::{
    full_path, log_local_response, public_base_url, require_token_caller,
};
use crate::utils::step_logger::{query_to_json, StepLogger};

/// The clients that have their own menu.
///
/// Checked against a fixed list rather than accepted as free text, because a
/// typo — `?platform=mobil` — would otherwise match no menu row and return an
/// EMPTY menu with a `200`, which renders as "you have no access" and sends
/// somebody hunting through role tables. A rejected value says what is wrong.
///
/// This list and `attendance.menu_items.platforms` are the same vocabulary;
/// adding a client means adding it here and marking its rows in that column.
const PLATFORMS: [&str; 3] = ["desktop", "mobile", "kiosk"];

/// `desktop` when the caller says nothing: it is what the ERP SPA wants, and it
/// is the value every existing menu row carries.
const DEFAULT_PLATFORM: &str = "desktop";

#[derive(Deserialize)]
pub struct AccessQuery {
    pub platform: Option<String>,
    /// The `version` from a previous response. Sent back to ask "has anything
    /// changed?" — see [`not_modified`].
    pub version: Option<String>,
}

fn fail(code: &str, message: impl Into<String>) -> Value {
    json!({
        "status":  "error",
        "code":    code,
        "message": message.into(),
    })
}

/// Normalise and validate the requested platform.
fn parse_platform(raw: Option<&str>) -> Result<String, String> {
    let value = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PLATFORM)
        .to_ascii_lowercase();

    if PLATFORMS.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(value)
    }
}

/// Did the caller already have this exact profile?
///
/// The client may send the previous `version` either as `If-None-Match` (what a
/// browser does on its own once it has seen the `ETag` header) or as
/// `?version=` (what a mobile app does, having stored the value). Both are
/// accepted because both happen, and neither is worth a second round trip.
///
/// Quotes and the weak-validator prefix are stripped: `If-None-Match` is
/// specified as `"abc"` or `W/"abc"`, while a stored value is the bare hash.
fn not_modified(client_version: Option<&str>, current: &str) -> bool {
    match client_version.map(str::trim).filter(|s| !s.is_empty()) {
        None => false,
        Some(v) => {
            let cleaned = v
                .trim_start_matches("W/")
                .trim_matches('"')
                .trim();
            !cleaned.is_empty() && cleaned == current
        }
    }
}

// GET and POST, for the reason every read-only endpoint here takes both: this
// service is POST throughout and a client posting to all of it should not
// special-case one path, while a profile read changes nothing, so GET is
// honest — and it is what lets a browser's own `If-None-Match` handling work.
//
// One `ext_api_allowed_ips` row covers both methods; the allow-list matches on
// path.
#[route("/me/access", method = "GET", method = "POST")]
pub async fn me_access(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<AccessQuery>,
    body: web::Bytes,
) -> HttpResponse {
    let log = StepLogger::new("ext-api/me/access");
    log.set_base_url(&public_base_url(&req));
    log.set_endpoint(req.method().as_str(), &full_path(&req));
    log.params("query", &query_to_json(req.query_string()));
    let resp = me_access_inner(&log, req, db, query, body).await;
    log_local_response(&log, resp).await
}

async fn me_access_inner(
    log: &StepLogger,
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<AccessQuery>,
    body: web::Bytes,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    // The identity is the token's, and only the token's.
    let (person_id, token_line) = match require_token_caller(&req) {
        Ok(v) => v,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    log.step(token_line);
    // Filed per person: "what did this user's app see?" is the question these
    // logs get read for.
    log.set_id(&person_id.to_string());

    let content_type = req
        .headers()
        .get(actix_web::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Query string first, then the body — the same precedence and the same four
    // encodings the NFC lookups accept, so a client that already talks to this
    // service needs no second way of sending one scalar.
    let requested = query
        .platform
        .clone()
        .or_else(|| scalar_from_body(content_type, &body, "platform"));

    let platform = match parse_platform(requested.as_deref()) {
        Ok(p) => p,
        Err(bad) => {
            log.step(format!("unknown platform `{bad}` — returning 400"));
            return HttpResponse::BadRequest().json(fail(
                "invalid_platform",
                format!(
                    "platform must be one of: {}",
                    PLATFORMS.join(", ")
                ),
            ));
        }
    };
    log.step(format!("building access profile for platform={platform}"));

    let result = sqlx::query_scalar::<_, Value>("SELECT attendance.access_profile($1, $2)")
        .bind(person_id)
        .bind(&platform)
        .fetch_one(db.get_ref())
        .await;

    let profile = match result {
        Ok(v) => v,
        Err(err) => {
            eprintln!("DB error in me_access: {err}");
            log.step(format!("DB access_profile FAILED: {err}"));
            // A missing `attendance.access_profile` lands here — the SQL and
            // the binary ship together, so an unapplied migration reads as a
            // 500 on every call rather than as an empty profile, which a client
            // would render as "you have no access".
            return HttpResponse::InternalServerError()
                .json(fail("internal_error", "Internal server error"));
        }
    };

    let version = profile["data"]["version"].as_str().unwrap_or("").to_string();

    // Unchanged since the client's last fetch? Say so and send nothing: this is
    // called on every app foreground, and the answer is usually the same one.
    let client_version = query.version.as_deref().or_else(|| {
        req.headers()
            .get(actix_web::http::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
    });

    if not_modified(client_version, &version) {
        log.step(format!("profile unchanged (version={version}) — returning 304"));
        return HttpResponse::NotModified()
            .insert_header((actix_web::http::header::ETAG, format!("\"{version}\"")))
            .finish();
    }

    // A person with no account, or a suspended one, gets an empty profile and a
    // 200 — not an error. The client renders a bare shell, and every endpoint
    // still refuses them; a 403 here would only make the app look broken to
    // someone who is merely unprivileged.
    let permissions = profile["data"]["permissions"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    let menu = profile["data"]["menu"].as_array().map(Vec::len).unwrap_or(0);
    log.step(format!(
        "profile built: role={} permissions={permissions} menu_items={menu} version={version}",
        profile["data"]["role"]
    ));

    HttpResponse::Ok()
        .insert_header((actix_web::http::header::ETAG, format!("\"{version}\"")))
        .json(profile)
}

// ---------------------------------------------------------------------
// Tests
//
// The two decisions this module makes on its own — which platform was asked
// for, and whether the client already has the answer. The profile itself is
// SQL and is tested against a database in `tests/access_control_sql.rs`.
// ---------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_defaults_to_desktop() {
        // Nothing sent, or an empty value, is the ERP SPA — and the value every
        // existing menu row carries.
        assert_eq!(parse_platform(None), Ok("desktop".to_string()));
        assert_eq!(parse_platform(Some("")), Ok("desktop".to_string()));
        assert_eq!(parse_platform(Some("   ")), Ok("desktop".to_string()));
    }

    #[test]
    fn platform_is_case_and_whitespace_insensitive() {
        assert_eq!(parse_platform(Some("Mobile")), Ok("mobile".to_string()));
        assert_eq!(parse_platform(Some(" KIOSK ")), Ok("kiosk".to_string()));
    }

    #[test]
    fn an_unknown_platform_is_rejected_rather_than_returning_an_empty_menu() {
        // The failure this prevents: `mobil` matches no menu row, so the
        // profile would come back with a 200 and an empty menu, which renders
        // as "you have no access" and sends somebody hunting through roles.
        assert_eq!(parse_platform(Some("mobil")), Err("mobil".to_string()));
        assert_eq!(parse_platform(Some("android")), Err("android".to_string()));
    }

    #[test]
    fn not_modified_matches_a_bare_version_or_an_etag() {
        let current = "8f14e45fceea167a5a36dedd4bea2543";
        // What a mobile app stores and sends back.
        assert!(not_modified(Some(current), current));
        // What a browser sends in If-None-Match, quoted, and as a weak
        // validator.
        assert!(not_modified(Some(&format!("\"{current}\"")), current));
        assert!(not_modified(Some(&format!("W/\"{current}\"")), current));
    }

    #[test]
    fn not_modified_is_false_when_anything_differs_or_is_missing() {
        let current = "8f14e45fceea167a5a36dedd4bea2543";
        assert!(!not_modified(None, current));
        assert!(!not_modified(Some(""), current));
        assert!(!not_modified(Some("   "), current));
        assert!(!not_modified(Some("\"\""), current));
        assert!(!not_modified(Some("something-else"), current));
        // A truncated version must not count as a match.
        assert!(!not_modified(Some(&current[..10]), current));
    }
}
