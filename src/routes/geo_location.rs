//! Employee office geo-locations with admin approval.
//!
//! Spec: `docs/employee_attendance_face_api.md`. SQL: `sql/010_employee_geo_location.sql`.
//!
//! Admin — office geo points (`attendance.geo.manage`)
//!   POST /ext-api/geo-location/offices?office_id=&status=     list, with approved/pending counts
//!   POST /ext-api/geo-location/offices/create                 json: office_id, latitude, longitude, radius_meters?, status?, label?
//!   POST /ext-api/geo-location/offices/update                 json: id, latitude?, longitude?, radius_meters?, label?
//!   POST /ext-api/geo-location/offices/status                 json: id, status (ACTIVE|INACTIVE)
//!
//! Admin — approvals (`attendance.geo.review`)
//!   POST /ext-api/geo-location/requests?status=&office_id=&employee_id=&search=&page=&limit=
//!   POST /ext-api/geo-location/requests/approve               json: id, note?
//!   POST /ext-api/geo-location/requests/reject                json: id, reason
//!   POST /ext-api/geo-location/requests/revoke                json: id, reason
//!   POST /ext-api/geo-location/requests/bulk-review           json: action, ids[], note?
//!   POST /ext-api/geo-location/requests/assign                json: employee_id, geo_location_office_id, note?
//!   POST /ext-api/geo-location/requests/history               json: id
//!
//! Employee — self-service (any authenticated employee, own rows only)
//!   POST /ext-api/geo-location/available                      ACTIVE points to choose from
//!   POST /ext-api/geo-location/my                             my selections
//!   POST /ext-api/geo-location/select                         json: geo_location_office_ids[]
//!   POST /ext-api/geo-location/remove                         json: id
//!
//! WHY THE PATHS DIFFER FROM THE SPEC'S REST SHAPE
//! `ExtAuthMiddleware` matches the allow-list and the permission map on the
//! EXACT path, so `/admin/employee-geo-locations/{id}/approve` would need a row
//! per id. Every other endpoint in this service is a fixed POST path with the
//! ids in the body or query, and these follow suit.
//!
//! WHO THE EMPLOYEE IS
//! The employee endpoints take the employee id from the bearer token's `sub`
//! and never from the request — there is no `employee_id` parameter to tamper
//! with, so one employee cannot select, list or remove offices for another.
//!
//! ADMIN ENDPOINTS ENFORCE FROM DAY ONE, like `/ext-api/access/*`: they are new
//! (nobody to break) and an unenforced approve endpoint would let any employee
//! approve their own office.

use actix_web::http::StatusCode;
use actix_web::{post, web, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgArguments;
use sqlx::query::QueryScalar;
use sqlx::{PgPool, Postgres};

use crate::routes::wow_attendance::{
    full_path, log_local_response, public_base_url, require_token_caller,
};
use crate::utils::step_logger::{query_to_json, StepLogger};

type Q<'a> = QueryScalar<'a, Postgres, Value, PgArguments>;

fn fail(status: StatusCode, code: &str, message: &str) -> HttpResponse {
    HttpResponse::build(status).json(json!({
        "status":  "failed",
        "code":    code,
        "message": message,
        "data":    {},
    }))
}

/// HTTP status for a failure `code` from the SQL layer — the table in the spec.
fn status_for(code: &str) -> StatusCode {
    match code {
        "NOT_FOUND"
        | "NOT_ENROLLED"
        | "EMPLOYEE_NOT_FOUND"
        | "OFFICE_NOT_FOUND"
        | "GEO_LOCATION_OFFICE_NOT_FOUND" => StatusCode::NOT_FOUND,
        "ALREADY_SELECTED"
        | "INVALID_STATUS_TRANSITION"
        | "OFFICE_REVOKED"
        | "GEO_LOCATION_OFFICE_INACTIVE" => StatusCode::CONFLICT,
        "LOCATION_LIMIT_EXCEEDED" | "REASON_REQUIRED" => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// Who may call: an admin holding the endpoint's permission, or the employee
/// the token names.
#[derive(Clone, Copy)]
enum Caller {
    Admin,
    Employee,
}

/// Admin permission check, keyed on the endpoint path so the mapping lives in
/// `ext_api_endpoint_permissions`. Reads `allowed` and IGNORES `enforce` (see
/// module note). A check that cannot run is a 500, never a pass.
async fn require_permission(
    log: &StepLogger,
    db: &web::Data<PgPool>,
    person_id: i64,
    endpoint: &str,
) -> Result<(), HttpResponse> {
    let verdict = sqlx::query_scalar::<_, Value>("SELECT attendance.ext_api_can_call($1, $2)")
        .bind(person_id)
        .bind(endpoint)
        .fetch_one(db.get_ref())
        .await;

    match verdict {
        Ok(v) if v.get("allowed").and_then(Value::as_bool).unwrap_or(false) => Ok(()),
        Ok(v) => {
            let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
            log.step(format!(
                "permission refused (reason={reason}) for token user id={person_id} — returning 403"
            ));
            Err(fail(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "You do not have permission to manage office geo-locations",
            ))
        }
        Err(e) => {
            eprintln!("geo-location permission check failed for {endpoint}: {e}");
            log.step(format!("permission check FAILED: {e}"));
            Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error"))
        }
    }
}

/// Every handler: token → (admin permission) → one SQL function → its envelope.
///
/// `bind` gets the token's person id: the actor for admin calls, the employee
/// for self-service ones.
async fn run(
    route: &'static str,
    endpoint: &'static str,
    caller: Caller,
    success: StatusCode,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    log: &StepLogger,
    sql: &'static str,
    bind: impl FnOnce(Q<'_>, i64) -> Q<'_>,
) -> HttpResponse {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    log.step(format!("request received (client_ip={client_ip})"));

    let (person_id, token_line) = match require_token_caller(req) {
        Ok(v) => v,
        Err(resp) => {
            log.step("token validation FAILED — rejecting request");
            return resp;
        }
    };
    log.step(token_line);
    log.set_id(&person_id.to_string());

    if let Caller::Admin = caller {
        if let Err(resp) = require_permission(log, db, person_id, endpoint).await {
            return resp;
        }
    }

    let query = bind(sqlx::query_scalar::<_, Value>(sql), person_id);
    match query.fetch_one(db.get_ref()).await {
        Ok(body) => {
            let ok = body.get("status").and_then(Value::as_str) == Some("success");
            let code = body.get("code").and_then(Value::as_str).unwrap_or("").to_string();
            log.step(format!("{route}: {}", if ok { "ok" } else { code.as_str() }));
            if ok {
                HttpResponse::build(success).json(body)
            } else {
                HttpResponse::build(status_for(&code)).json(body)
            }
        }
        Err(err) => {
            eprintln!("DB error in {route}: {err}");
            log.step(format!("DB call FAILED: {err}"));
            fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error")
        }
    }
}

fn logger(route: &'static str, req: &actix_web::HttpRequest) -> StepLogger {
    let log = StepLogger::new(route);
    log.set_base_url(&public_base_url(req));
    log.set_endpoint(req.method().as_str(), &full_path(req));
    log.params("query", &query_to_json(req.query_string()));
    log
}

fn log_json<T: Serialize>(log: &StepLogger, body: &T) {
    log.params("json", &serde_json::to_value(body).unwrap_or(Value::Null));
}

// ---------------------------------------------------------------------
// Admin: office geo points
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OfficesQuery {
    pub office_id: Option<String>,
    pub status: Option<String>,
}

#[post("/geo-location/offices")]
pub async fn offices_list(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<OfficesQuery>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/offices", &req);
    let (office_id, status) = (query.office_id.clone(), query.status.clone());
    let resp = run(
        "offices_list",
        "/ext-api/geo-location/offices",
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.geo_office_list($1, $2)",
        move |q, _| q.bind(office_id).bind(status),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct OfficeCreateRequest {
    pub office_id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius_meters: Option<i32>,
    pub status: Option<String>,
    pub label: Option<String>,
}

#[post("/geo-location/offices/create")]
pub async fn office_create(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<OfficeCreateRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/offices/create", &req);
    log_json(&log, &*body);
    let b = body.into_inner();
    let resp = run(
        "office_create",
        "/ext-api/geo-location/offices/create",
        Caller::Admin,
        StatusCode::CREATED,
        &req,
        &db,
        &log,
        "SELECT attendance.geo_office_create($1, $2, $3, $4, $5, $6, $7)",
        move |q, actor| {
            q.bind(b.office_id)
                .bind(b.latitude)
                .bind(b.longitude)
                .bind(b.radius_meters)
                .bind(b.status)
                .bind(b.label)
                .bind(actor)
        },
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct OfficeUpdateRequest {
    pub id: i64,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub radius_meters: Option<i32>,
    /// `""` clears the label; omitted leaves it unchanged.
    pub label: Option<String>,
}

#[post("/geo-location/offices/update")]
pub async fn office_update(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<OfficeUpdateRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/offices/update", &req);
    log_json(&log, &*body);
    let b = body.into_inner();
    let resp = run(
        "office_update",
        "/ext-api/geo-location/offices/update",
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.geo_office_update($1, $2, $3, $4, $5, $6)",
        move |q, actor| {
            q.bind(b.id)
                .bind(b.latitude)
                .bind(b.longitude)
                .bind(b.radius_meters)
                .bind(b.label)
                .bind(actor)
        },
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct OfficeStatusRequest {
    pub id: i64,
    pub status: String,
}

#[post("/geo-location/offices/status")]
pub async fn office_status(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<OfficeStatusRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/offices/status", &req);
    log_json(&log, &*body);
    let b = body.into_inner();
    let resp = run(
        "office_status",
        "/ext-api/geo-location/offices/status",
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.geo_office_set_status($1, $2, $3)",
        move |q, actor| q.bind(b.id).bind(b.status).bind(actor),
    )
    .await;
    log_local_response(&log, resp).await
}

// ---------------------------------------------------------------------
// Admin: approvals
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RequestsQuery {
    pub status: Option<String>,
    pub office_id: Option<String>,
    pub employee_id: Option<String>,
    pub search: Option<String>,
    pub page: Option<i32>,
    pub limit: Option<i32>,
}

#[post("/geo-location/requests")]
pub async fn requests_list(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<RequestsQuery>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/requests", &req);
    let q0 = query.into_inner();
    let resp = run(
        "requests_list",
        "/ext-api/geo-location/requests",
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_requests($1, $2, $3, $4, $5, $6)",
        move |q, _| {
            q.bind(q0.status)
                .bind(q0.office_id)
                .bind(q0.employee_id)
                .bind(q0.search)
                .bind(q0.page)
                .bind(q0.limit)
        },
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct ReviewRequest {
    pub id: i64,
    pub note: Option<String>,
    pub reason: Option<String>,
}

async fn review(
    route: &'static str,
    endpoint: &'static str,
    action: &'static str,
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: ReviewRequest,
) -> HttpResponse {
    let log = logger(route, &req);
    log_json(&log, &body);
    // Approve takes a `note`, reject/revoke a `reason`; either is accepted.
    let note = match action {
        "APPROVE" => body.note.or(body.reason),
        _ => body.reason.or(body.note),
    };
    let id = body.id;
    let resp = run(
        route,
        endpoint,
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_review($1, $2, $3, $4)",
        move |q, actor| q.bind(id).bind(action).bind(note).bind(actor),
    )
    .await;
    log_local_response(&log, resp).await
}

#[post("/geo-location/requests/approve")]
pub async fn request_approve(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<ReviewRequest>,
) -> HttpResponse {
    review(
        "ext-api/geo-location/requests/approve",
        "/ext-api/geo-location/requests/approve",
        "APPROVE",
        req,
        db,
        body.into_inner(),
    )
    .await
}

#[post("/geo-location/requests/reject")]
pub async fn request_reject(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<ReviewRequest>,
) -> HttpResponse {
    review(
        "ext-api/geo-location/requests/reject",
        "/ext-api/geo-location/requests/reject",
        "REJECT",
        req,
        db,
        body.into_inner(),
    )
    .await
}

#[post("/geo-location/requests/revoke")]
pub async fn request_revoke(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<ReviewRequest>,
) -> HttpResponse {
    review(
        "ext-api/geo-location/requests/revoke",
        "/ext-api/geo-location/requests/revoke",
        "REVOKE",
        req,
        db,
        body.into_inner(),
    )
    .await
}

#[derive(Deserialize, Serialize)]
pub struct BulkReviewRequest {
    pub action: String,
    pub ids: Vec<i64>,
    pub note: Option<String>,
    pub reason: Option<String>,
}

#[post("/geo-location/requests/bulk-review")]
pub async fn requests_bulk_review(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<BulkReviewRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/requests/bulk-review", &req);
    log_json(&log, &*body);
    let b = body.into_inner();
    let note = b.reason.or(b.note);
    let resp = run(
        "requests_bulk_review",
        "/ext-api/geo-location/requests/bulk-review",
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_bulk_review($1, $2, $3, $4)",
        move |q, actor| q.bind(b.action).bind(b.ids).bind(note).bind(actor),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct AssignRequest {
    pub employee_id: String,
    pub geo_location_office_id: i64,
    pub note: Option<String>,
}

#[post("/geo-location/requests/assign")]
pub async fn request_assign(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<AssignRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/requests/assign", &req);
    log_json(&log, &*body);
    let b = body.into_inner();
    let resp = run(
        "request_assign",
        "/ext-api/geo-location/requests/assign",
        Caller::Admin,
        StatusCode::CREATED,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_assign($1, $2, $3, $4)",
        move |q, actor| {
            q.bind(b.employee_id)
                .bind(b.geo_location_office_id)
                .bind(b.note)
                .bind(actor)
        },
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct IdRequest {
    pub id: i64,
}

#[post("/geo-location/requests/history")]
pub async fn request_history(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<IdRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/requests/history", &req);
    log_json(&log, &*body);
    let id = body.id;
    let resp = run(
        "request_history",
        "/ext-api/geo-location/requests/history",
        Caller::Admin,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_history($1)",
        move |q, _| q.bind(id),
    )
    .await;
    log_local_response(&log, resp).await
}

// ---------------------------------------------------------------------
// Employee: self-service. The employee id is ALWAYS the token's `sub`.
// ---------------------------------------------------------------------

#[post("/geo-location/available")]
pub async fn available(req: actix_web::HttpRequest, db: web::Data<PgPool>) -> HttpResponse {
    let log = logger("ext-api/geo-location/available", &req);
    let resp = run(
        "available",
        "/ext-api/geo-location/available",
        Caller::Employee,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.geo_office_available()",
        |q, _| q,
    )
    .await;
    log_local_response(&log, resp).await
}

#[post("/geo-location/my")]
pub async fn my_locations(req: actix_web::HttpRequest, db: web::Data<PgPool>) -> HttpResponse {
    let log = logger("ext-api/geo-location/my", &req);
    let resp = run(
        "my_locations",
        "/ext-api/geo-location/my",
        Caller::Employee,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_my($1)",
        |q, me| q.bind(me.to_string()),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, Serialize)]
pub struct SelectRequest {
    pub geo_location_office_ids: Vec<i64>,
}

#[post("/geo-location/select")]
pub async fn select(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<SelectRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/select", &req);
    log_json(&log, &*body);
    let ids = body.into_inner().geo_location_office_ids;
    let resp = run(
        "select",
        "/ext-api/geo-location/select",
        Caller::Employee,
        StatusCode::CREATED,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_select($1, $2)",
        move |q, me| q.bind(me.to_string()).bind(ids),
    )
    .await;
    log_local_response(&log, resp).await
}

#[post("/geo-location/remove")]
pub async fn remove(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<IdRequest>,
) -> HttpResponse {
    let log = logger("ext-api/geo-location/remove", &req);
    log_json(&log, &*body);
    let id = body.id;
    let resp = run(
        "remove",
        "/ext-api/geo-location/remove",
        Caller::Employee,
        StatusCode::OK,
        &req,
        &db,
        &log,
        "SELECT attendance.employee_geo_remove($1, $2)",
        move |q, me| q.bind(me.to_string()).bind(id),
    )
    .await;
    log_local_response(&log, resp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_status_codes() {
        assert_eq!(status_for("NOT_ENROLLED"), StatusCode::NOT_FOUND);
        assert_eq!(status_for("GEO_LOCATION_OFFICE_NOT_FOUND"), StatusCode::NOT_FOUND);
        assert_eq!(status_for("ALREADY_SELECTED"), StatusCode::CONFLICT);
        assert_eq!(status_for("INVALID_STATUS_TRANSITION"), StatusCode::CONFLICT);
        assert_eq!(status_for("LOCATION_LIMIT_EXCEEDED"), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status_for("REASON_REQUIRED"), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status_for("INVALID_INPUT"), StatusCode::BAD_REQUEST);
        assert_eq!(status_for("anything else"), StatusCode::BAD_REQUEST);
    }
}
