use std::env;
use std::future::{ready, Ready};
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_service::{Service, Transform};
use actix_web::body::{EitherBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::{web, Error, HttpResponse};
use futures_util::future::LocalBoxFuture;
use sqlx::PgPool;

// ---------------------------------------------------------------------
// Access control — the fourth layer
//
// Layers 1-3 (app credentials, IP allow-list, bearer token) say WHO is
// calling. This one says whether that person may call THIS path, from
// `attendance.ext_api_can_call` — see docs/access_control.md.
//
// TWO SWITCHES, deliberately separate:
//
//   * `EXT_ACCESS_CONTROL` (here) says whether this middleware may refuse
//     anything at all. It is the panic button: one env var and a restart
//     turns the whole layer off.
//   * `ext_api_endpoint_permissions.enforce` (a table column) says WHICH
//     endpoints refuse. It is the rollout dial: one UPDATE, no redeploy.
//
// Both must agree before a request is refused, so the default state —
// `audit`, every rule seeded `enforce = false` — refuses nothing twice over.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum AccessMode {
    /// Do not even ask. No query, no log line, no cost — the service behaves
    /// exactly as it did before this layer existed.
    Off,
    /// Ask, record, and let every request through whatever the answer. This is
    /// the default: a binary carrying this code changes no outcome until
    /// somebody deliberately says otherwise.
    Audit,
    /// Ask, record, and refuse when the endpoint's own rule says `enforce`.
    Enforce,
}

/// Parse `EXT_ACCESS_CONTROL`. Anything unrecognised — including a typo — is
/// `Audit`, never `Enforce`: a misspelt value must not start refusing
/// requests, and must not silently disable the audit trail either.
fn parse_access_mode(raw: &str) -> AccessMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" | "no" => AccessMode::Off,
        "on" | "enforce" | "true" | "1" | "yes" => AccessMode::Enforce,
        _ => AccessMode::Audit,
    }
}

fn access_mode() -> AccessMode {
    parse_access_mode(&env::var("EXT_ACCESS_CONTROL").unwrap_or_default())
}

/// Should a FAILED check (not a refusal — a failure: unapplied migration,
/// database down) refuse the request?
///
/// Default **false**, and that is a deliberate deployment property: a binary
/// deployed ahead of `sql/006_access_control.sql` behaves exactly as today
/// rather than 403ing every ext-api call. Set it true once every endpoint is
/// enforcing and "the check is unavailable" should stop traffic.
fn parse_fail_closed(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn fail_closed() -> bool {
    parse_fail_closed(&env::var("EXT_ACCESS_FAIL_CLOSED").unwrap_or_default())
}

/// What the access layer decided about one permission point.
pub(crate) struct AccessDecision {
    /// Did the caller hold the permission?
    pub allowed: bool,
    /// Should this request be refused? `allowed == false` AND both switches on.
    pub refuse: bool,
}

/// Ask whether a person may do one thing, and record the answer.
///
/// `point` is usually a request path, which is what
/// `ext_api_endpoint_permissions` is keyed on. It can also be a **permission
/// point inside a handler** — `"/ext-api/nfc-card/save_card_info#force_reassign"`
/// — for a decision the path cannot express: a field on the request that is
/// more dangerous than the request itself. Such a row is not a path and never
/// matches one; only the handler that names it ever asks about it.
///
/// Shared by the middleware and those handlers on purpose. The mode switch, the
/// per-row `enforce` flag, the log line and the audit row are one behaviour, and
/// a handler that grew its own copy would drift from the middleware the first
/// time either changed — leaving one of them refusing during a rollout that was
/// supposed to be audit-only.
pub(crate) async fn access_decision(
    pool: &web::Data<PgPool>,
    person_id: Option<i64>,
    token_source: &'static str,
    point: &str,
) -> AccessDecision {
    // `off` costs nothing at all: no query, no log, no row.
    if access_mode() == AccessMode::Off {
        return AccessDecision { allowed: true, refuse: false };
    }

    let verdict = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT attendance.ext_api_can_call($1, $2)",
    )
    .bind(person_id)
    .bind(point)
    .fetch_one(pool.get_ref())
    .await;

    let (allowed, rule_enforces, reason) = match &verdict {
        Ok(v) => (
            v.get("allowed").and_then(serde_json::Value::as_bool).unwrap_or(false),
            v.get("enforce").and_then(serde_json::Value::as_bool).unwrap_or(false),
            v.get("reason").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
        ),
        // The check itself failed — an unapplied migration, or a database that
        // is down. That is NOT a verdict, and it must not read as one: see
        // `fail_closed()`.
        Err(e) => {
            eprintln!("[access] check FAILED for {point}: {e}");
            (false, fail_closed(), "check_failed".to_string())
        }
    };

    let refuse = !allowed && access_mode() == AccessMode::Enforce && rule_enforces;

    if !allowed {
        // One line per would-be refusal, with everything needed to act on it:
        // who, where, why, and whether this was the real thing. `[access]` is
        // the grep handle.
        eprintln!(
            "[access] {} person={} token={} reason={} enforce={} refused={}",
            point,
            person_id.map(|i| i.to_string()).unwrap_or_else(|| "-".into()),
            token_source,
            reason,
            rule_enforces,
            refuse
        );
        // ...and one row, folded by (person, point, reason), so "who would
        // break if I enforced this?" is a query rather than a log trawl.
        record_denial(
            pool.clone(),
            person_id,
            point.to_string(),
            reason,
            token_source,
            rule_enforces,
            refuse,
        );
    }

    AccessDecision { allowed, refuse }
}

/// Fold one denial into `attendance.ext_api_access_audit`.
///
/// Detached on purpose: this is the rollout's evidence, not part of serving
/// the request, and a slow or failing INSERT must not add latency to — or
/// fail — a call the verdict already allowed. A lost row costs one hit in a
/// counter; a blocked request costs a check-in.
///
/// Folded by (person, endpoint, reason) rather than appended, because during
/// the audit phase EVERY request is a denial (no account has a role yet), and
/// a row each would be a write per tap for a report nobody reads line by line.
#[allow(clippy::too_many_arguments)]
fn record_denial(
    pool: web::Data<PgPool>,
    person_id: Option<i64>,
    endpoint: String,
    reason: String,
    token_source: &'static str,
    rule_enforces: bool,
    refused: bool,
) {
    actix_web::rt::spawn(async move {
        let sql = r#"
            INSERT INTO attendance.ext_api_access_audit
                   (person_id, endpoint, reason, token_source, rule_enforces, refused)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (person_id, endpoint, reason) DO UPDATE
               SET hits          = attendance.ext_api_access_audit.hits + 1,
                   last_seen     = now(),
                   token_source  = EXCLUDED.token_source,
                   rule_enforces = EXCLUDED.rule_enforces,
                   refused       = EXCLUDED.refused
        "#;
        if let Err(e) = sqlx::query(sql)
            .bind(person_id)
            .bind(&endpoint)
            .bind(&reason)
            .bind(token_source)
            .bind(rule_enforces)
            .bind(refused)
            .execute(pool.get_ref())
            .await
        {
            // Never propagated: the request it describes has already been
            // served one way or the other.
            eprintln!("[access] audit write failed for {endpoint}: {e}");
        }
    });
}

pub struct ExtAuthMiddleware;

impl<S, B> Transform<S, ServiceRequest> for ExtAuthMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = ExtAuthMiddlewareMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        let app_id = env::var("EXT_APP_ID").expect("EXT_APP_ID must be set in .env");
        let app_password = env::var("EXT_APP_PASSWORD").expect("EXT_APP_PASSWORD must be set in .env");
        ready(Ok(ExtAuthMiddlewareMiddleware {
            service: Rc::new(service),
            app_id,
            app_password,
        }))
    }
}

pub struct ExtAuthMiddlewareMiddleware<S> {
    service: Rc<S>,
    app_id: String,
    app_password: String,
}

impl<S, B> Service<ServiceRequest> for ExtAuthMiddlewareMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let svc = Rc::clone(&self.service);
        let expected_id = self.app_id.clone();
        let expected_password = self.app_password.clone();

        // Get DB pool from app_data
        let db = req.app_data::<web::Data<PgPool>>().cloned();

        // Capture endpoint path and client IP before moving req
        let endpoint = req.path().to_string();
        let client_ip = req
            .connection_info()
            .realip_remote_addr()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            // Credential check
            let app_id = req
                .headers()
                .get("X-App-Id")
                .and_then(|h| h.to_str().ok());

            let app_password = req
                .headers()
                .get("X-App-Password")
                .and_then(|h| h.to_str().ok());

            if app_id != Some(&expected_id) || app_password != Some(&expected_password) {
                let res = HttpResponse::Unauthorized()
                    .json(serde_json::json!({ "error": "Invalid App ID or Password" }))
                    .map_into_right_body();
                return Ok(req.into_response(res));
            }

            // IP check against database.
            //
            // `'*'` in `ip_address` OPENS THE ENDPOINT TO EVERY IP. It is how an
            // endpoint with no fixed set of callers — the NFC card readers, which
            // are on DHCP across campus — is served without listing addresses
            // that change. Kept in the row rather than as a list of paths here so
            // it is an operator decision, revertible with one UPDATE and no
            // redeploy, and visible in the same place the real IPs are.
            //
            // Nothing else changes: the row must still exist, still be
            // `is_active`, and the app credentials and bearer token still apply.
            // Set it ONLY on endpoints whose own auth is enough on its own.
            if let Some(pool) = &db {
                let allowed: bool = sqlx::query_scalar(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM attendance.ext_api_allowed_ips
                        WHERE endpoint = $1
                          AND is_active = true
                          AND ('*' = ANY(ip_address) OR $2 = ANY(ip_address))
                    )
                    "#,
                )
                .bind(&endpoint)
                .bind(&client_ip)
                .fetch_one(pool.get_ref())
                .await
                .unwrap_or(false);

                if !allowed {
                    let res = HttpResponse::Forbidden()
                        .json(serde_json::json!({
                            "error": format!("IP address not allowed for this endpoint. Add your requested IP address: '{}'", client_ip)
                        }))
                        .map_into_right_body();
                    return Ok(req.into_response(res));
                }
            }

            // ── Layer 4 · may THIS PERSON call THIS path? ──────────────
            //
            // The first three layers say a known client, from an allowed
            // address, holding some token. This one is about the person that
            // token names: `attendance.ext_api_can_call` answers from the
            // endpoint's rule row and the caller's role (docs/access_control.md).
            //
            // The identity is read here rather than taken from the handler,
            // which has not run yet, and a missing or unreadable token is not
            // rejected here — it is simply "no identity". The handlers keep
            // their own token checks; this layer only ever ADDS a refusal.
            if let Some(pool) = &db {
                let identity = crate::routes::wow_attendance::token_identity(req.headers());
                let (person_id, token_source) = match identity {
                    None => (None, "none"),
                    Some((id, true)) => (Some(id), "legacy"),
                    Some((id, false)) => (Some(id), "ours"),
                };

                if access_decision(pool, person_id, token_source, &endpoint)
                    .await
                    .refuse
                {
                    let res = HttpResponse::Forbidden()
                        .json(serde_json::json!({
                            "error": "You do not have permission to use this endpoint",
                            "code": "forbidden"
                        }))
                        .map_into_right_body();
                    return Ok(req.into_response(res));
                }
            }

            let res = svc.call(req).await?;
            Ok(res.map_into_left_body())
        })
    }
}

// ---------------------------------------------------------------------
// Tests
//
// The decision this middleware makes is "refuse or not", and it is made from
// three inputs: the mode, the verdict, and the rule's own flag. Those are
// pinned here. The verdicts themselves are SQL and are tested against a
// database in `tests/access_control_sql.rs`.
// ---------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_mode_defaults_to_audit() {
        // Unset, empty, or misspelt: ask and record, never refuse. The two
        // failure modes this avoids are a typo that silently enforces, and a
        // typo that silently stops collecting the rollout evidence.
        assert_eq!(parse_access_mode(""), AccessMode::Audit);
        assert_eq!(parse_access_mode("   "), AccessMode::Audit);
        assert_eq!(parse_access_mode("audit"), AccessMode::Audit);
        assert_eq!(parse_access_mode("enfroce"), AccessMode::Audit);
        assert_eq!(parse_access_mode("ON-ish"), AccessMode::Audit);
    }

    #[test]
    fn access_mode_reads_the_two_deliberate_values() {
        for raw in ["off", "OFF", " false ", "0", "no"] {
            assert_eq!(parse_access_mode(raw), AccessMode::Off, "for {raw:?}");
        }
        for raw in ["on", "enforce", "ENFORCE", " true ", "1", "yes"] {
            assert_eq!(parse_access_mode(raw), AccessMode::Enforce, "for {raw:?}");
        }
    }

    #[test]
    fn fail_closed_is_off_unless_asked_for() {
        // A binary deployed ahead of the SQL must behave exactly as today, so
        // "the check could not run" defaults to letting the request through.
        assert!(!parse_fail_closed(""));
        assert!(!parse_fail_closed("no"));
        assert!(!parse_fail_closed("maybe"));
        assert!(parse_fail_closed("true"));
        assert!(parse_fail_closed("1"));
        assert!(parse_fail_closed(" ON "));
    }

    /// The refusal rule, exactly as the middleware applies it: BOTH the global
    /// mode and the endpoint's own flag must say so.
    fn refuses(mode: AccessMode, allowed: bool, rule_enforces: bool) -> bool {
        !allowed && mode == AccessMode::Enforce && rule_enforces
    }

    #[test]
    fn audit_mode_never_refuses() {
        // The whole point of the rollout stage: the verdict is computed and
        // recorded, and every request is still served.
        assert!(!refuses(AccessMode::Audit, false, true));
        assert!(!refuses(AccessMode::Audit, false, false));
        assert!(!refuses(AccessMode::Off, false, true));
    }

    #[test]
    fn enforcing_needs_the_rule_to_say_so_too() {
        // Both switches, or nothing happens. `enforce` on the row is the
        // rollout dial; the mode is the panic button.
        assert!(refuses(AccessMode::Enforce, false, true));
        assert!(!refuses(AccessMode::Enforce, false, false));
    }

    #[test]
    fn an_allowed_verdict_is_never_refused() {
        for mode in [AccessMode::Off, AccessMode::Audit, AccessMode::Enforce] {
            assert!(!refuses(mode, true, true), "{mode:?}");
        }
    }
}
