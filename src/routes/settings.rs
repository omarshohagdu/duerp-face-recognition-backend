//! Admin settings API — the NFC face-verification toggle.
//!
//!   GET /admin-api/settings/nfc-face-verify
//!   PUT /admin-api/settings/nfc-face-verify
//!
//! What the admin panel binds a switch and a text box to. The values live in
//! `attendance.system_settings`; the rules, the validation and the audit rows
//! are in `sql/008_system_settings.sql`, so a psql caller gets the same
//! answers.
//!
//! WHY `/admin-api` AND NOT `/ext-api`
//! The path is what the admin panel asked for. It is mounted in `main.rs`
//! behind the SAME `ExtAuthMiddleware` as `/ext-api` — app credentials, IP
//! allow-list, and then this module's own permission check — because a scope
//! outside that middleware would be a new, unguarded surface on a service
//! whose whole gate lives there.
//!
//! THE ENVELOPE IS THE ONE THE SPEC ASKED FOR — `{"success": true, "data":
//! {...}}` — which is the wow-attendance shape, not the NFC module's
//! `status`/`message`. The SQL underneath answers in the house shape and this
//! module translates, so the contract the panel sees is exactly the documented
//! one and the database still reads like the rest of the schema.
//!
//! **This endpoint enforces its permission immediately**, like the rest of
//! `/ext-api/access/*` and unlike the audit-mode rollout: it is new, so nobody
//! breaks, and a switch that disables face verification is not a switch to
//! leave unguarded.

use actix_web::{get, put, web, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::routes::wow_attendance::{
    full_path, log_local_response, public_base_url, require_token_caller,
};
use crate::utils::settings;
use crate::utils::step_logger::{query_to_json, StepLogger};

/// These handlers are mounted TWICE — under `/ext-api` and under `/admin-api`
/// (see `main.rs`) — so the permission check reads the path off the request
/// rather than a constant. Both paths carry their own `ext_api_allowed_ips`
/// and `ext_api_endpoint_permissions` rows, pointing at the same permission,
/// because the middleware matches on the exact path.
///
/// Why two: `/admin-api/settings/nfc-face-verify` is the documented contract,
/// but the production gateway has no proxy rule for `/admin-api` and adding
/// one is another team's change. `/ext-api/` is already proxied, so the same
/// endpoint under that prefix works today. When the gateway rule lands, the
/// documented path starts working with no further change.

/// The permission. Its own rather than `admin.roles.manage`: turning face
/// verification off is an operations decision, and the person who administers
/// roles is not necessarily the person who should make it.
const PERMISSION_NOTE: &str = "admin.settings.manage";

fn fail(status: actix_web::http::StatusCode, code: &str, message: &str) -> HttpResponse {
    HttpResponse::build(status).json(json!({
        "success": false,
        "code":    code,
        "message": message,
    }))
}

/// Map the SQL layer's `code` onto the status the spec documents.
///
/// `url_required` is a 422 and not a 400 on purpose: the request was
/// well-formed and every field valid — it is the resulting STATE that is not
/// allowed, which is exactly what 422 is for. A client can tell "you sent
/// nonsense" from "that would leave the gate enabled with nothing to call".
fn status_for(code: &str) -> actix_web::http::StatusCode {
    use actix_web::http::StatusCode;
    match code {
        "url_required" => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// Token, then permission. Returns the caller's person id.
///
/// Reads the verdict and ignores `enforce` / `EXT_ACCESS_CONTROL`: see the
/// module note. A failed check is a 500, never a pass.
async fn require_admin(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
) -> Result<i64, HttpResponse> {
    let (person_id, token_line) = match require_token_caller(req) {
        Ok(v) => v,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return Err(resp);
        }
    };
    log.step(token_line);
    log.set_id(&person_id.to_string());

    let verdict = sqlx::query_scalar::<_, Value>("SELECT attendance.ext_api_can_call($1, $2)")
        .bind(person_id)
        // The path actually called, so this agrees with the rule row the
        // middleware just evaluated for the same request.
        .bind(req.path())
        .fetch_one(db.get_ref())
        .await;

    match verdict {
        Ok(v) if v.get("allowed").and_then(Value::as_bool).unwrap_or(false) => Ok(person_id),
        Ok(v) => {
            let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
            log.step(format!(
                "permission refused (reason={reason}, needs {PERMISSION_NOTE}) — returning 403"
            ));
            Err(fail(
                actix_web::http::StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to change system settings",
            ))
        }
        Err(e) => {
            eprintln!("settings permission check failed: {e}");
            log.step(format!("permission check FAILED: {e}"));
            Err(fail(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            ))
        }
    }
}

/// Turn a rejected JSON body into this API's envelope.
///
/// Actix answers a bad payload with PLAIN TEXT — `Content type error`, or
/// `Json deserialize error: …`. A client that expects `{"success": …}` gets a
/// string, reads `success` as `undefined`, and shows an empty error: the
/// failure that started this was invisible for exactly that reason.
///
/// Registered on the `/admin-api` scope in `main.rs`, so every JSON extractor
/// under it answers in one shape.
pub fn json_error_handler(
    err: actix_web::error::JsonPayloadError,
    _req: &actix_web::HttpRequest,
) -> actix_web::Error {
    use actix_web::error::JsonPayloadError;

    // Name the two failures a caller can actually act on, and keep serde's own
    // message for the rest — "missing field `nfc_face_verify`" is the useful
    // part, and inventing our own wording would lose it.
    let (code, message) = match &err {
        JsonPayloadError::ContentType => (
            "invalid_content_type",
            "Send the body as JSON with `Content-Type: application/json`".to_string(),
        ),
        JsonPayloadError::Deserialize(e) => ("invalid_json", format!("Invalid JSON body: {e}")),
        other => ("invalid_json", format!("Could not read the JSON body: {other}")),
    };

    actix_web::error::InternalError::from_response(
        err,
        HttpResponse::BadRequest().json(json!({
            "success": false,
            "code":    code,
            "message": message,
        })),
    )
    .into()
}

fn logger(req: &actix_web::HttpRequest) -> StepLogger {
    let log = StepLogger::new("admin-api/settings/nfc-face-verify");
    log.set_base_url(&public_base_url(req));
    log.set_endpoint(req.method().as_str(), &full_path(req));
    log.params("query", &query_to_json(req.query_string()));
    log
}

// ---------------------------------------------------------------------
// GET — what is configured now
// ---------------------------------------------------------------------

#[get("/settings/nfc-face-verify")]
pub async fn get_nfc_face_verify(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
) -> HttpResponse {
    let log = logger(&req);
    let resp = get_inner(&log, &req, &db).await;
    log_local_response(&log, resp).await
}

async fn get_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
) -> HttpResponse {
    if let Err(resp) = require_admin(log, req, db).await {
        return resp;
    }

    match sqlx::query_scalar::<_, Value>("SELECT attendance.nfc_face_verify_get()")
        .fetch_one(db.get_ref())
        .await
    {
        Ok(body) => {
            // What the SETTINGS say. The gate may still be reading the
            // environment — see `effective` below — and an operator looking at
            // this screen needs to know which.
            let effective = settings::face_verify(db.get_ref()).await;
            let mut data = body.get("data").cloned().unwrap_or_else(|| json!({}));
            if let Some(obj) = data.as_object_mut() {
                obj.insert(
                    "effective".to_string(),
                    json!({
                        "enabled": effective.enabled,
                        "url":     effective.url,
                        // false = nobody has used this API yet, so the
                        // environment is still in charge and the two columns
                        // above may not match the stored rows.
                        "managed_here": effective.managed,
                    }),
                );
            }
            log.step("settings read");
            HttpResponse::Ok().json(json!({ "success": true, "data": data }))
        }
        Err(err) => {
            eprintln!("DB error reading nfc-face-verify settings: {err}");
            log.step(format!("DB read FAILED: {err}"));
            fail(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            )
        }
    }
}

// ---------------------------------------------------------------------
// PUT — change it
// ---------------------------------------------------------------------

#[derive(Deserialize, serde::Serialize)]
pub struct UpdateRequest {
    pub nfc_face_verify: String,
    /// Absent leaves the stored URL alone — the "just flip the switch" case.
    /// `""` clears it, which then makes `ON` impossible until one is set
    /// again, and the 422 says so.
    #[serde(default)]
    pub nfc_face_verify_url: Option<String>,
}

#[put("/settings/nfc-face-verify")]
pub async fn put_nfc_face_verify(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<UpdateRequest>,
) -> HttpResponse {
    let log = logger(&req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let resp = put_inner(&log, &req, &db, body).await;
    log_local_response(&log, resp).await
}

async fn put_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    body: web::Json<UpdateRequest>,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();

    let person_id = match require_admin(log, req, db).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let result = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.nfc_face_verify_save($1, $2, $3, $4)",
    )
    .bind(&body.nfc_face_verify)
    .bind(body.nfc_face_verify_url.as_deref())
    .bind(person_id)
    .bind(&client_ip)
    .fetch_one(db.get_ref())
    .await;

    match result {
        Ok(out) => {
            if out.get("status").and_then(Value::as_str) != Some("success") {
                let code = out.get("code").and_then(Value::as_str).unwrap_or("");
                let message = out
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Invalid request");
                log.step(format!("rejected ({code})"));
                return fail(status_for(code), code, message);
            }

            // The cache is dropped BEFORE answering, so the admin's next
            // request — and every card save after it — sees the new value.
            // Doing it after the response would leave a window where the
            // screen says ON and the gate is still off.
            settings::invalidate();

            let data = out.get("data").cloned().unwrap_or_else(|| json!({}));
            log.step(format!(
                "settings updated by person {person_id}: nfc_face_verify={} — cache invalidated",
                data.get("nfc_face_verify").and_then(Value::as_str).unwrap_or("?")
            ));
            HttpResponse::Ok().json(json!({
                "success": true,
                "message": "Settings updated",
                "data":    data,
            }))
        }
        Err(err) => {
            eprintln!("DB error writing nfc-face-verify settings: {err}");
            log.step(format!("DB write FAILED: {err}"));
            fail(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_required_is_a_422_and_everything_else_a_400() {
        // The distinction the spec asks for, and it is a real one: a 400 means
        // "you sent something invalid", a 422 means "every field is fine but
        // the result would not be" — the gate enabled with nothing to call.
        assert_eq!(status_for("url_required").as_u16(), 422);
        assert_eq!(status_for("invalid_value").as_u16(), 400);
        assert_eq!(status_for("invalid_url").as_u16(), 400);
        // An unrecognised code is a caller error, not a 500: every code this
        // function sees comes from the validation layer.
        assert_eq!(status_for("something_new").as_u16(), 400);
    }

    #[test]
    fn a_url_is_optional_but_a_toggle_is_not() {
        // Absent URL = leave it alone; present-but-empty = clear it. The two
        // have to stay distinguishable, which is why the field is Option and
        // not a defaulted String.
        let only_toggle: UpdateRequest =
            serde_json::from_str(r#"{"nfc_face_verify":"OFF"}"#).expect("parses");
        assert_eq!(only_toggle.nfc_face_verify, "OFF");
        assert!(only_toggle.nfc_face_verify_url.is_none());

        let clearing: UpdateRequest =
            serde_json::from_str(r#"{"nfc_face_verify":"OFF","nfc_face_verify_url":""}"#)
                .expect("parses");
        assert_eq!(clearing.nfc_face_verify_url.as_deref(), Some(""));

        // The toggle itself is mandatory — a PUT with no state to set is a
        // client bug, and actix answers it with a 400 before the handler runs.
        assert!(serde_json::from_str::<UpdateRequest>(r#"{"nfc_face_verify_url":"https://x"}"#).is_err());
    }
}
