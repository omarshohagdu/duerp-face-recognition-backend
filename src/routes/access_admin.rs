//! Role administration — the API behind the Access Roles screen.
//!
//!   POST /ext-api/access/roles          — list roles, their permissions, member counts
//!   POST /ext-api/access/resources      — the permission vocabulary, for the tick-boxes
//!   POST /ext-api/access/role-save      — create/update a role and REPLACE its permissions
//!   POST /ext-api/access/role-delete    — delete an unused, non-system role
//!   POST /ext-api/access/users          — list accounts, their role, overrides and effective set
//!   POST /ext-api/access/user-role      — put a person in a role, or take them out
//!   POST /ext-api/access/user-override  — one person's exception: grant | deny | clear
//!   POST /ext-api/access/role-active    — retire or restore a role
//!   POST /ext-api/access/user-status    — the kill switch: active | inactive
//!   POST /ext-api/access/user-delete    — delete an account and its overrides
//!
//! Design: `docs/access_control.md` §2.1 option (c). The roles this service
//! enforces are administered here, because the ERP's screens write `ictcell`
//! and no longer reach this service's copy.
//!
//! THESE ENDPOINTS ENFORCE FROM DAY ONE — the one deliberate exception to the
//! audit-mode rollout in §6/§8. The rollout is cautious because every other
//! endpoint has existing callers who would break; these have none, and they are
//! the API that hands out permissions. Serving them in audit mode would mean
//! any token holder could make themselves an admin, which is a strictly worse
//! failure than a new screen 403ing on its first day.
//!
//! So the check here does NOT consult `EXT_ACCESS_CONTROL` or the rule row's
//! `enforce` flag: it reads the verdict and refuses unless it says yes.
//!
//! WHAT LOCKS THE DOOR BEHIND YOU is handled in SQL, not here
//! (`sql/007_access_admin.sql`): `admin.roles.manage` cannot be stripped from
//! `admin`, the last active admin cannot be demoted, a role with members cannot
//! be deleted. Those hold for a DBA at a psql prompt too, which is the point.

use actix_web::{post, web, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::routes::wow_attendance::{
    full_path, log_local_response, public_base_url, require_token_caller,
};
use crate::utils::step_logger::{query_to_json, StepLogger};

fn fail(code: &str, message: impl Into<String>) -> Value {
    json!({
        "status":  "error",
        "code":    code,
        "message": message.into(),
        "data":    {},
    })
}

/// May this person use this admin endpoint?
///
/// Reuses the ordinary verdict function, keyed on the endpoint path, so the
/// mapping from "this screen" to "this permission" lives in
/// `ext_api_endpoint_permissions` where an administrator can see and change it
/// — rather than being a string compiled into the handler.
///
/// It reads `allowed` and IGNORES `enforce`: see the module note. A failed
/// check is a `500`, never a pass — the migration behind this API and this code
/// ship together, so "the check could not run" here means something is broken,
/// not that a caller should be let through.
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
        Ok(v) => {
            if v.get("allowed").and_then(Value::as_bool).unwrap_or(false) {
                Ok(())
            } else {
                let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
                log.step(format!(
                    "permission refused (reason={reason}) for token user id={person_id} — returning 403"
                ));
                Err(HttpResponse::Forbidden().json(fail(
                    "forbidden",
                    "You do not have permission to administer access roles",
                )))
            }
        }
        Err(e) => {
            eprintln!("access-admin permission check failed for {endpoint}: {e}");
            log.step(format!("permission check FAILED: {e}"));
            Err(HttpResponse::InternalServerError()
                .json(fail("internal_error", "Internal server error")))
        }
    }
}

/// Everything these handlers do: check the token, check the permission, call
/// one SQL function, answer with what it returned.
///
/// The envelope is built in SQL (`status` / `message` / `data`), so a psql
/// caller sees exactly what the screen does, and an error `code` is the stable
/// part to branch on.
async fn run(
    route: &'static str,
    endpoint: &'static str,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    log: &StepLogger,
    sql: &'static str,
    bind: impl FnOnce(sqlx::query::QueryScalar<'_, sqlx::Postgres, Value, sqlx::postgres::PgArguments>, i64, String)
        -> sqlx::query::QueryScalar<'_, sqlx::Postgres, Value, sqlx::postgres::PgArguments>,
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

    if let Err(resp) = require_permission(log, db, person_id, endpoint).await {
        return resp;
    }

    let query = bind(sqlx::query_scalar::<_, Value>(sql), person_id, client_ip);
    match query.fetch_one(db.get_ref()).await {
        Ok(body) => {
            let ok = body.get("status").and_then(Value::as_str) == Some("success");
            let code = body.get("code").and_then(Value::as_str).unwrap_or("");
            log.step(format!("{route}: {}", if ok { "ok" } else { code }));
            if ok {
                HttpResponse::Ok().json(body)
            } else {
                // Every refusal from these functions is the caller asking for
                // something the rules do not allow — a bad key, an unknown
                // permission, a lockout guard. That is a 400, not a 500.
                HttpResponse::BadRequest().json(body)
            }
        }
        Err(err) => {
            eprintln!("DB error in {route}: {err}");
            log.step(format!("DB call FAILED: {err}"));
            HttpResponse::InternalServerError().json(fail("internal_error", "Internal server error"))
        }
    }
}

/// Start a step log for one of these calls. They are rare and consequential —
/// somebody changing who may do what — so every one gets a file, filed under
/// the actor.
fn logger(route: &'static str, req: &actix_web::HttpRequest) -> StepLogger {
    let log = StepLogger::new(route);
    log.set_base_url(&public_base_url(req));
    log.set_endpoint(req.method().as_str(), &full_path(req));
    log.params("query", &query_to_json(req.query_string()));
    log
}

// ---------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------

#[post("/access/roles")]
pub async fn roles_list(req: actix_web::HttpRequest, db: web::Data<PgPool>) -> HttpResponse {
    let log = logger("ext-api/access/roles", &req);
    let resp = run(
        "roles_list",
        "/ext-api/access/roles",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_roles_list()",
        |q, _, _| q,
    )
    .await;
    log_local_response(&log, resp).await
}

#[post("/access/resources")]
pub async fn resources_list(req: actix_web::HttpRequest, db: web::Data<PgPool>) -> HttpResponse {
    let log = logger("ext-api/access/resources", &req);
    let resp = run(
        "resources_list",
        "/ext-api/access/resources",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_resources_list()",
        |q, _, _| q,
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize)]
pub struct UsersQuery {
    pub search: Option<String>,
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

#[post("/access/users")]
pub async fn users_list(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<UsersQuery>,
) -> HttpResponse {
    let log = logger("ext-api/access/users", &req);
    let (search, limit, offset) = (query.search.clone(), query.limit, query.offset);
    let resp = run(
        "users_list",
        "/ext-api/access/users",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_users_list($1, $2, $3)",
        move |q, _, _| q.bind(search).bind(limit).bind(offset),
    )
    .await;
    log_local_response(&log, resp).await
}

// ---------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------

#[derive(Deserialize, serde::Serialize)]
pub struct RoleSaveRequest {
    pub key: String,
    pub name: String,
    /// The WHOLE permission set. A key left out is a revocation — that is what
    /// unticking a box means — so a client that sends a partial list silently
    /// strips the rest.
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[post("/access/role-save")]
pub async fn role_save(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<RoleSaveRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/role-save", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let (key, name, perms) = (body.key.clone(), body.name.clone(), body.permissions.clone());
    let resp = run(
        "role_save",
        "/ext-api/access/role-save",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_role_save($1, $2, $3, $4, $5)",
        move |q, actor, ip| q.bind(key).bind(name).bind(perms).bind(actor).bind(ip),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, serde::Serialize)]
pub struct RoleDeleteRequest {
    pub key: String,
}

#[post("/access/role-delete")]
pub async fn role_delete(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<RoleDeleteRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/role-delete", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let key = body.key.clone();
    let resp = run(
        "role_delete",
        "/ext-api/access/role-delete",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_role_delete($1, $2, $3)",
        move |q, actor, ip| q.bind(key).bind(actor).bind(ip),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, serde::Serialize)]
pub struct UserRoleRequest {
    pub person_id: i64,
    /// `null` or omitted clears the role — "no role", which is denied
    /// everything once enforcement is on.
    pub role: Option<String>,
}

#[post("/access/user-role")]
pub async fn user_role(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<UserRoleRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/user-role", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let (person_id, role) = (body.person_id, body.role.clone());
    let resp = run(
        "user_role",
        "/ext-api/access/user-role",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_user_set_role($1, $2, $3, $4)",
        move |q, actor, ip| q.bind(person_id).bind(role).bind(actor).bind(ip),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, serde::Serialize)]
pub struct UserOverrideRequest {
    pub person_id: i64,
    pub permission: String,
    /// `grant` | `deny` | `clear`.
    pub effect: String,
}

#[derive(Deserialize, serde::Serialize)]
pub struct RoleActiveRequest {
    pub key: String,
    /// `false` retires the role: it stops being assignable. Everybody already
    /// holding it keeps exactly what they had — see `sql/008`, which states
    /// why that is narrow on purpose.
    pub is_active: bool,
}

#[post("/access/role-active")]
pub async fn role_active(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<RoleActiveRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/role-active", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let (key, is_active) = (body.key.clone(), body.is_active);
    let resp = run(
        "role_active",
        "/ext-api/access/role-active",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_role_set_active($1, $2, $3, $4)",
        move |q, actor, ip| q.bind(key).bind(is_active).bind(actor).bind(ip),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, serde::Serialize)]
pub struct UserStatusRequest {
    pub person_id: i64,
    /// `active` | `inactive`. Anything else is a 400 from the SQL function
    /// rather than a silent lockout: the gate treats every value that is not
    /// `active` as inactive, so a typo would deny the person just as
    /// effectively as the word that was meant.
    pub status: String,
}

#[post("/access/user-status")]
pub async fn user_status(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<UserStatusRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/user-status", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let (person_id, status) = (body.person_id, body.status.clone());
    let resp = run(
        "user_status",
        "/ext-api/access/user-status",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_user_set_status($1, $2, $3, $4)",
        move |q, actor, ip| q.bind(person_id).bind(status).bind(actor).bind(ip),
    )
    .await;
    log_local_response(&log, resp).await
}

#[derive(Deserialize, serde::Serialize)]
pub struct UserDeleteRequest {
    pub person_id: i64,
}

/// Delete an account. IRREVERSIBLE, and more so than it looks: nothing in this
/// service creates `app_users` rows — they arrive by import — so the person
/// does not get a fresh account by signing in again. They simply have none,
/// which the gate answers as `no_account`.
///
/// The refusals that matter (`self_delete`, `last_admin`) are in SQL, so a DBA
/// deleting a row by hand hits them too.
#[post("/access/user-delete")]
pub async fn user_delete(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<UserDeleteRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/user-delete", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let person_id = body.person_id;
    let resp = run(
        "user_delete",
        "/ext-api/access/user-delete",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_user_delete($1, $2, $3)",
        move |q, actor, ip| q.bind(person_id).bind(actor).bind(ip),
    )
    .await;
    log_local_response(&log, resp).await
}

#[post("/access/user-override")]
pub async fn user_override(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    body: web::Json<UserOverrideRequest>,
) -> HttpResponse {
    let log = logger("ext-api/access/user-override", &req);
    log.params("json", &serde_json::to_value(&*body).unwrap_or(Value::Null));
    let (person_id, permission, effect) = (
        body.person_id,
        body.permission.clone(),
        body.effect.clone(),
    );
    let resp = run(
        "user_override",
        "/ext-api/access/user-override",
        &req,
        &db,
        &log,
        "SELECT attendance.admin_user_override($1, $2, $3, $4, $5)",
        move |q, actor, ip| {
            q.bind(person_id)
                .bind(permission)
                .bind(effect)
                .bind(actor)
                .bind(ip)
        },
    )
    .await;
    log_local_response(&log, resp).await
}
