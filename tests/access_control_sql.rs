//! DB-backed tests for the access-control SQL — `sql/006_access_control.sql`.
//!
//! Design: `docs/access_control.md`. These cover the two functions that decide
//! things:
//!
//!   * `attendance.ext_api_can_call(person_id, endpoint)` — **the gate.** If a
//!     test here is wrong, requests are allowed that should not be.
//!   * `attendance.access_profile(person_id, platform)` — what the desktop and
//!     mobile clients render. Wrong here is a UI that lies: a button the API
//!     will refuse, or a screen the user needed and never saw.
//!
//! The pair must agree, and `profile_and_can_call_agree` is the test that says
//! so. They are computed by two different queries over the same tables, which
//! is exactly the kind of duplication that drifts.
//!
//! The RBAC tables live in `attendance` — this service's own copy of the ERP's
//! model (`sql/006_access_control.sql` §1-2), so these tests never write to
//! `ictcell`.
//!
//! Every test runs inside a transaction that is dropped without committing, so
//! the database is left exactly as it was found. Every fixture key carries a
//! per-test random suffix, so a run against a database holding the copied
//! production rows cannot collide with them.
//!
//! Requires `DATABASE_URL`, and the migration must already be applied:
//!
//!     psql "$DATABASE_URL" -f sql/006_access_control.sql
//!     cargo test --test access_control_sql
//!
//! Without either, every test skips rather than fails, so `cargo test` still
//! works on a machine with no database.

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()
}

/// Is `sql/006_access_control.sql` applied to this database?
///
/// Both functions AND the six RBAC tables the migration copies out of
/// `ictcell`: the functions compile against tables that do not exist yet and
/// fail at call time, so checking only for them would turn a half-applied
/// migration into a wall of opaque "relation does not exist" failures instead
/// of one skip naming the file.
async fn migration_applied(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT (SELECT count(*) = 2 FROM pg_proc p
                   JOIN pg_namespace n ON n.oid = p.pronamespace
                  WHERE n.nspname = 'attendance'
                    AND p.proname IN ('ext_api_can_call', 'access_profile'))
            AND (SELECT count(*) = 7 FROM information_schema.tables
                  WHERE table_schema = 'attendance'
                    AND table_name IN ('roles', 'resources', 'role_permissions',
                                       'app_users', 'user_permission_overrides',
                                       'menu_items', 'ext_api_endpoint_permissions'))",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

macro_rules! db_or_skip {
    () => {{
        match pool().await {
            Some(p) => {
                if !migration_applied(&p).await {
                    eprintln!(
                        "skipping: access-control objects not found — \
                         apply sql/006_access_control.sql first"
                    );
                    return;
                }
                p
            }
            None => {
                eprintln!("skipping: DATABASE_URL unset or unreachable");
                return;
            }
        }
    }};
}

fn unique() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_lowercase()
}

/// One test's worth of ERP rows: a person, a role, a permission and a path.
///
/// Built per test rather than shared, because most of these tests are about
/// what happens when ONE of those is missing or overridden, and a shared
/// fixture would have to be un-set to express that.
struct Fx {
    person_id: i64,
    /// `app_users.id` — what the override table references, NOT the person id.
    user_id: i32,
    role_id: i32,
    resource_id: i32,
    resource_key: String,
    endpoint: String,
}

async fn fixture(tx: &mut Transaction<'_, Postgres>) -> Fx {
    let u = unique();

    let role_id: i32 = sqlx::query_scalar(
        "INSERT INTO attendance.roles (key, name, is_system)
         VALUES ($1, $2, false) RETURNING id",
    )
    .bind(format!("test_role_{u}"))
    .bind(format!("Test Role {u}"))
    .fetch_one(&mut **tx)
    .await
    .expect("role fixture");

    let resource_key = format!("test.res.{u}");
    let resource_id: i32 = sqlx::query_scalar(
        "INSERT INTO attendance.resources (key, name, category)
         VALUES ($1, $2, 'Test') RETURNING id",
    )
    .bind(&resource_key)
    .bind(format!("Test Resource {u}"))
    .fetch_one(&mut **tx)
    .await
    .expect("resource fixture");

    // Well outside the range real person ids occupy (10-digit employee ids and
    // DU user ids), so a fixture can never be mistaken for a real account even
    // if one somehow escaped the rollback.
    let person_id: i64 = 9_000_000_000_000
        + (u64::from_str_radix(&u[..8], 16).unwrap_or(0) % 1_000_000_000) as i64;

    let user_id: i32 = sqlx::query_scalar(
        "INSERT INTO attendance.app_users (person_id, username, du_base_role, role_id, status)
         VALUES ($1, $2, 'TestRole', $3, 'active') RETURNING id",
    )
    .bind(person_id)
    .bind(format!("test-{u}@du.ac.bd"))
    .bind(role_id)
    .fetch_one(&mut **tx)
    .await
    .expect("app_user fixture");

    let endpoint = format!("/ext-api/test/{u}");
    sqlx::query(
        "INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce)
         VALUES ($1, $2, true)",
    )
    .bind(&endpoint)
    .bind(&resource_key)
    .execute(&mut **tx)
    .await
    .expect("endpoint rule fixture");

    Fx { person_id, user_id, role_id, resource_id, resource_key, endpoint }
}

/// Give the fixture's role the fixture's permission.
async fn grant_to_role(tx: &mut Transaction<'_, Postgres>, fx: &Fx) {
    sqlx::query(
        "INSERT INTO attendance.role_permissions (role_id, resource_id)
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(fx.role_id)
    .bind(fx.resource_id)
    .execute(&mut **tx)
    .await
    .expect("role grant");
}

/// A per-person override: `'grant'` or `'deny'`.
async fn override_for(
    tx: &mut Transaction<'_, Postgres>,
    fx: &Fx,
    resource_id: i32,
    effect: &str,
) {
    sqlx::query(
        "INSERT INTO attendance.user_permission_overrides (user_id, resource_id, effect)
         VALUES ($1, $2, $3)
         ON CONFLICT (user_id, resource_id) DO UPDATE SET effect = EXCLUDED.effect",
    )
    .bind(fx.user_id)
    .bind(resource_id)
    .bind(effect)
    .execute(&mut **tx)
    .await
    .expect("override");
}

async fn can_call(tx: &mut Transaction<'_, Postgres>, person_id: i64, endpoint: &str) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.ext_api_can_call($1, $2)")
        .bind(person_id)
        .bind(endpoint)
        .fetch_one(&mut **tx)
        .await
        .expect("can_call")
}

async fn profile(tx: &mut Transaction<'_, Postgres>, person_id: i64, platform: &str) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.access_profile($1, $2)")
        .bind(person_id)
        .bind(platform)
        .fetch_one(&mut **tx)
        .await
        .expect("access_profile")
}

fn allowed(v: &Value) -> bool {
    v.get("allowed").and_then(Value::as_bool).unwrap_or(false)
}

fn reason(v: &Value) -> String {
    v.get("reason").and_then(Value::as_str).unwrap_or("").to_string()
}

fn permissions(p: &Value) -> Vec<String> {
    p["data"]["permissions"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Labels of the top-level menu items, in the order they came back.
fn menu_labels(p: &Value) -> Vec<String> {
    p["data"]["menu"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["label"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn child_labels(p: &Value, parent: &str) -> Vec<String> {
    p["data"]["menu"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["label"].as_str() == Some(parent)))
        .and_then(|m| m["children"].as_array())
        .map(|c| {
            c.iter()
                .filter_map(|x| x["label"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// =====================================================================
// ext_api_can_call — the gate
// =====================================================================

#[tokio::test]
async fn an_unconfigured_endpoint_is_denied() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // THE fail-closed test. A path nobody has written a rule for must refuse,
    // not wave through — otherwise every endpoint added after this system
    // ships is unprotected and nobody notices until it matters.
    let v = can_call(&mut tx, 1, "/ext-api/never/configured").await;
    assert!(!allowed(&v), "unconfigured path must be denied: {v}");
    assert_eq!(reason(&v), "no_rule");
    // And it refuses for real, rather than reporting itself as audit-only:
    // "nobody configured this" is not a reason to be lenient.
    assert_eq!(v["enforce"], true);
}

#[tokio::test]
async fn a_deactivated_rule_is_treated_as_no_rule() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;

    assert!(allowed(&can_call(&mut tx, fx.person_id, &fx.endpoint).await));

    // Switching a rule off must not quietly open the endpoint — the same trap
    // `is_active` sets on the IP allow-list.
    sqlx::query("UPDATE attendance.ext_api_endpoint_permissions SET is_active = false WHERE endpoint = $1")
        .bind(&fx.endpoint)
        .execute(&mut *tx)
        .await
        .expect("deactivate");

    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(!allowed(&v), "a deactivated rule must not allow: {v}");
    assert_eq!(reason(&v), "no_rule");
}

#[tokio::test]
async fn a_null_resource_key_is_open_to_any_authenticated_caller() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    // The one deliberate exception to fail-closed: somebody wrote down "any
    // authenticated caller" on purpose. `/ext-api/me/access` is the real case.
    sqlx::query("UPDATE attendance.ext_api_endpoint_permissions SET resource_key = NULL WHERE endpoint = $1")
        .bind(&fx.endpoint)
        .execute(&mut *tx)
        .await
        .expect("open the rule");

    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(allowed(&v), "{v}");
    assert_eq!(reason(&v), "open_to_authenticated");
}

#[tokio::test]
async fn the_role_decides_when_there_is_no_override() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    let before = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(!allowed(&before), "a role without the permission must not pass: {before}");
    assert_eq!(reason(&before), "not_in_role");

    grant_to_role(&mut tx, &fx).await;

    let after = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(allowed(&after), "{after}");
    assert_eq!(reason(&after), "granted_by_role");
}

#[tokio::test]
async fn a_grant_override_lets_one_person_through_without_the_role() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    // "Allow this one person" — the requested case, and the reason
    // user_permission_overrides exists.
    override_for(&mut tx, &fx, fx.resource_id, "grant").await;

    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(allowed(&v), "{v}");
    assert_eq!(reason(&v), "granted_by_override");
}

#[tokio::test]
async fn a_deny_override_beats_the_role() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;
    assert!(allowed(&can_call(&mut tx, fx.person_id, &fx.endpoint).await));

    // "Everyone in this role except her" only works if deny outranks the role
    // grant. If this ever flips, a revoked person keeps their access.
    override_for(&mut tx, &fx, fx.resource_id, "deny").await;

    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(!allowed(&v), "deny override must win: {v}");
    assert_eq!(reason(&v), "denied_by_override");
}

#[tokio::test]
async fn an_inactive_account_is_refused_whatever_its_role_says() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;

    // The kill switch. Tokens live ~730 hours with no revocation list, so this
    // is the only thing that stops somebody NOW.
    sqlx::query("UPDATE attendance.app_users SET status = 'inactive' WHERE id = $1")
        .bind(fx.user_id)
        .execute(&mut *tx)
        .await
        .expect("suspend");

    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(!allowed(&v), "a suspended account must be refused: {v}");
    assert_eq!(reason(&v), "account_inactive");
}

#[tokio::test]
async fn a_person_with_no_erp_account_is_denied() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;

    // What every device currently looks like: a valid token, no app_users row.
    // Denied, with the reason that tells an operator it is an account problem
    // rather than a permission one (docs §10).
    let v = can_call(&mut tx, 9_111_111_111_111, &fx.endpoint).await;
    assert!(!allowed(&v), "{v}");
    assert_eq!(reason(&v), "no_account");
}

#[tokio::test]
async fn a_typod_resource_key_denies_rather_than_allows() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;

    sqlx::query("UPDATE attendance.ext_api_endpoint_permissions SET resource_key = 'no.such.key' WHERE endpoint = $1")
        .bind(&fx.endpoint)
        .execute(&mut *tx)
        .await
        .expect("typo the key");

    // A key that matches nothing must not collapse into "no permission
    // required". The distinct reason is what makes it findable.
    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(!allowed(&v), "{v}");
    assert_eq!(reason(&v), "unknown_resource");
}

#[tokio::test]
async fn audit_mode_reports_the_verdict_without_softening_it() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    sqlx::query("UPDATE attendance.ext_api_endpoint_permissions SET enforce = false WHERE endpoint = $1")
        .bind(&fx.endpoint)
        .execute(&mut *tx)
        .await
        .expect("audit mode");

    // The function still says "no". Letting the request through is the
    // MIDDLEWARE's decision, made from `enforce` — keeping that split is what
    // makes the audit-mode logs worth reading during the rollout (docs §8).
    let v = can_call(&mut tx, fx.person_id, &fx.endpoint).await;
    assert!(!allowed(&v), "audit mode must not change the verdict: {v}");
    assert_eq!(v["enforce"], false);
    assert_eq!(reason(&v), "not_in_role");
}

#[tokio::test]
async fn every_verdict_carries_allowed_enforce_and_reason() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    // The middleware reads all three on every path through the function; a
    // missing `enforce` would read as `false` and silently stop refusing.
    let cases = vec![
        ("no rule", can_call(&mut tx, fx.person_id, "/ext-api/nope").await),
        ("not in role", can_call(&mut tx, fx.person_id, &fx.endpoint).await),
        ("no account", can_call(&mut tx, 9_222_222_222_222, &fx.endpoint).await),
    ];

    for (label, v) in cases {
        assert!(v["allowed"].is_boolean(), "{label}: allowed must be a bool — {v}");
        assert!(v["enforce"].is_boolean(), "{label}: enforce must be a bool — {v}");
        assert!(!reason(&v).is_empty(), "{label}: reason must be set — {v}");
    }
}

#[tokio::test]
async fn the_force_reassign_permission_point_is_seeded_and_wired() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // `force_reassign` is a FIELD on save_card_info, not a path, so the
    // endpoint map cannot express it — `src/routes/nfc_card.rs` asks about this
    // exact string instead (its `REASSIGN_POINT`). The two are compared by eye,
    // which is precisely why the string is pinned here: a typo on either side
    // reads as `no_rule`, and `no_rule` fails closed, so every reassignment
    // would be refused the moment the rule is enforced.
    const POINT: &str = "/ext-api/nfc-card/save_card_info#force_reassign";

    let rule: Option<(String, bool)> = sqlx::query_as(
        "SELECT resource_key, enforce FROM attendance.ext_api_endpoint_permissions
          WHERE endpoint = $1 AND is_active",
    )
    .bind(POINT)
    .fetch_optional(&mut *tx)
    .await
    .expect("rule lookup");

    let (resource_key, enforce) =
        rule.unwrap_or_else(|| panic!("no rule row for {POINT} — apply sql/006_access_control.sql"));
    assert_eq!(resource_key, "nfc.card.reassign");
    assert!(!enforce, "the seeded row must start in audit mode");

    // And it decides like any other point: the resource exists, so a person
    // without it is `not_in_role` rather than `unknown_resource`.
    let fx = fixture(&mut tx).await;
    let v = can_call(&mut tx, fx.person_id, POINT).await;
    assert!(!allowed(&v));
    assert_eq!(reason(&v), "not_in_role", "{v}");

    // Granting the resource to their role lets the reassignment through, which
    // is the whole point of splitting it from `nfc.card.register`.
    let reassign: i32 = sqlx::query_scalar("SELECT id FROM attendance.resources WHERE key = 'nfc.card.reassign'")
        .fetch_one(&mut *tx)
        .await
        .expect("nfc.card.reassign must be seeded");
    sqlx::query("INSERT INTO attendance.role_permissions (role_id, resource_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(fx.role_id)
        .bind(reassign)
        .execute(&mut *tx)
        .await
        .expect("grant reassign");

    let v = can_call(&mut tx, fx.person_id, POINT).await;
    assert!(allowed(&v), "{v}");
    assert_eq!(reason(&v), "granted_by_role");
}

#[tokio::test]
async fn registering_a_card_does_not_imply_reassigning_one() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    // A card desk holds `nfc.card.register`. That must NOT carry
    // `nfc.card.reassign` with it: one is "issue a card", the other is "take
    // this student's card away", and one desk clerk in a hundred needs the
    // second.
    let register: i32 = sqlx::query_scalar("SELECT id FROM attendance.resources WHERE key = 'nfc.card.register'")
        .fetch_one(&mut *tx)
        .await
        .expect("nfc.card.register must be seeded");
    sqlx::query("INSERT INTO attendance.role_permissions (role_id, resource_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(fx.role_id)
        .bind(register)
        .execute(&mut *tx)
        .await
        .expect("grant register");

    let save = can_call(&mut tx, fx.person_id, "/ext-api/nfc-card/save_card_info").await;
    assert!(allowed(&save), "the desk may register: {save}");

    let reassign = can_call(&mut tx, fx.person_id, "/ext-api/nfc-card/save_card_info#force_reassign").await;
    assert!(!allowed(&reassign), "but it may NOT reassign: {reassign}");
}

// =====================================================================
// access_profile — what the clients render
// =====================================================================

/// A container menu row with two children, each gated by its own resource.
/// Returns (parent_label, child_a_label, child_b_label, resource_b_id).
async fn menu_fixture(tx: &mut Transaction<'_, Postgres>, fx: &Fx) -> (String, String, String, i32) {
    let u = unique();
    let parent_label = format!("Parent {u}");
    let child_a = format!("Child A {u}");
    let child_b = format!("Child B {u}");

    // A second permission, so "one child denied" and "both denied" are
    // different tests.
    let resource_b_key = format!("test.res.b.{u}");
    let resource_b: i32 = sqlx::query_scalar(
        "INSERT INTO attendance.resources (key, name, category) VALUES ($1, $2, 'Test') RETURNING id",
    )
    .bind(&resource_b_key)
    .bind(format!("Test Resource B {u}"))
    .fetch_one(&mut **tx)
    .await
    .expect("resource b");

    sqlx::query("INSERT INTO attendance.role_permissions (role_id, resource_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(fx.role_id)
        .bind(resource_b)
        .execute(&mut **tx)
        .await
        .expect("grant b");

    let parent_id: i32 = sqlx::query_scalar(
        "INSERT INTO attendance.menu_items (label, route, resource_id, sort_order, platforms)
         VALUES ($1, NULL, NULL, 500, '{desktop,mobile}') RETURNING id",
    )
    .bind(&parent_label)
    .fetch_one(&mut **tx)
    .await
    .expect("parent menu");

    for (label, resource, sort) in [(&child_a, fx.resource_id, 1), (&child_b, resource_b, 2)] {
        sqlx::query(
            "INSERT INTO attendance.menu_items (parent_id, label, route, resource_id, sort_order, platforms)
             VALUES ($1, $2, $3, $4, $5, '{desktop,mobile}')",
        )
        .bind(parent_id)
        .bind(label)
        .bind(format!("/{}", label.replace(' ', "-").to_lowercase()))
        .bind(resource)
        .bind(sort)
        .execute(&mut **tx)
        .await
        .expect("child menu");
    }

    (parent_label, child_a, child_b, resource_b)
}

#[tokio::test]
async fn the_profile_lists_what_the_role_holds() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    assert!(
        !permissions(&profile(&mut tx, fx.person_id, "desktop").await).contains(&fx.resource_key),
        "a permission the role does not hold must not be listed"
    );

    grant_to_role(&mut tx, &fx).await;

    let p = profile(&mut tx, fx.person_id, "desktop").await;
    assert_eq!(p["status"], "success");
    assert!(permissions(&p).contains(&fx.resource_key), "{p}");
    assert_eq!(p["data"]["person_id"], fx.person_id);
}

#[tokio::test]
async fn a_deny_override_removes_the_permission_and_the_menu_item() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;
    let (parent, child_a, child_b, _) = menu_fixture(&mut tx, &fx).await;

    let before = profile(&mut tx, fx.person_id, "desktop").await;
    assert_eq!(child_labels(&before, &parent), vec![child_a.clone(), child_b.clone()]);

    override_for(&mut tx, &fx, fx.resource_id, "deny").await;

    // Both halves, or the UI lies in one of two directions: a listed permission
    // with no menu item is merely confusing; a menu item whose permission was
    // revoked is a screen that 403s on click.
    let after = profile(&mut tx, fx.person_id, "desktop").await;
    assert!(!permissions(&after).contains(&fx.resource_key), "{after}");
    assert_eq!(child_labels(&after, &parent), vec![child_b]);
}

#[tokio::test]
async fn a_grant_override_adds_the_permission_without_a_role() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    override_for(&mut tx, &fx, fx.resource_id, "grant").await;

    let p = profile(&mut tx, fx.person_id, "desktop").await;
    assert!(permissions(&p).contains(&fx.resource_key), "{p}");
}

#[tokio::test]
async fn the_menu_is_filtered_by_platform() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;
    let (parent, _, _, _) = menu_fixture(&mut tx, &fx).await;

    // The fixture menu is marked for both, so it appears on both.
    assert!(menu_labels(&profile(&mut tx, fx.person_id, "desktop").await).contains(&parent));
    assert!(menu_labels(&profile(&mut tx, fx.person_id, "mobile").await).contains(&parent));

    // Take it off mobile: the same person, the same permissions, a different
    // menu. This is the whole point of the platforms column.
    sqlx::query("UPDATE attendance.menu_items SET platforms = '{desktop}' WHERE label = $1 OR parent_id = (SELECT id FROM attendance.menu_items WHERE label = $1)")
        .bind(&parent)
        .execute(&mut *tx)
        .await
        .expect("desktop only");

    assert!(menu_labels(&profile(&mut tx, fx.person_id, "desktop").await).contains(&parent));
    assert!(
        !menu_labels(&profile(&mut tx, fx.person_id, "mobile").await).contains(&parent),
        "a desktop-only item must not reach the mobile menu"
    );
}

#[tokio::test]
async fn a_container_whose_children_are_all_denied_disappears() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;
    let (parent, _, _, resource_b) = menu_fixture(&mut tx, &fx).await;

    assert!(menu_labels(&profile(&mut tx, fx.person_id, "desktop").await).contains(&parent));

    override_for(&mut tx, &fx, fx.resource_id, "deny").await;
    override_for(&mut tx, &fx, resource_b, "deny").await;

    // An empty dropdown is worse than no dropdown: it advertises something and
    // then refuses to open.
    let p = profile(&mut tx, fx.person_id, "desktop").await;
    assert!(!menu_labels(&p).contains(&parent), "empty container must be pruned: {p}");
}

#[tokio::test]
async fn an_inactive_account_gets_an_empty_profile_not_an_error() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;

    sqlx::query("UPDATE attendance.app_users SET status = 'inactive' WHERE id = $1")
        .bind(fx.user_id)
        .execute(&mut *tx)
        .await
        .expect("suspend");

    // A suspended person is not an error case for the CLIENT — it renders a
    // bare shell. The endpoints refuse them regardless, which is where that
    // decision belongs.
    let p = profile(&mut tx, fx.person_id, "desktop").await;
    assert_eq!(p["status"], "success");
    assert!(permissions(&p).is_empty(), "{p}");
    assert_eq!(p["data"]["menu"], serde_json::json!([]));
    assert!(p["data"]["role"].is_null());
}

#[tokio::test]
async fn a_person_with_no_account_gets_an_empty_profile() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    let p = profile(&mut tx, 9_333_333_333_333, "desktop").await;
    assert_eq!(p["status"], "success");
    assert!(permissions(&p).is_empty(), "{p}");
    assert_eq!(p["data"]["menu"], serde_json::json!([]));
}

#[tokio::test]
async fn the_version_changes_when_access_changes_and_not_otherwise() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;
    grant_to_role(&mut tx, &fx).await;
    let _ = menu_fixture(&mut tx, &fx).await;

    let v1 = profile(&mut tx, fx.person_id, "desktop").await["data"]["version"].clone();
    let v2 = profile(&mut tx, fx.person_id, "desktop").await["data"]["version"].clone();
    assert_eq!(v1, v2, "an unchanged profile must hash the same, or clients re-render forever");

    override_for(&mut tx, &fx, fx.resource_id, "deny").await;

    let v3 = profile(&mut tx, fx.person_id, "desktop").await["data"]["version"].clone();
    assert_ne!(v1, v3, "revoking a permission must change the version");
}

// =====================================================================
// The two must agree
// =====================================================================

#[tokio::test]
async fn profile_and_can_call_agree() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let fx = fixture(&mut tx).await;

    // Same question, two queries, four situations. They are computed
    // separately — the gate walks one resource, the profile aggregates them
    // all — and this is the test that catches them drifting apart. A UI that
    // shows what the API refuses is a bug report every time; the reverse is a
    // support ticket about a missing screen.
    let situations: Vec<(&str, bool)> = vec![
        ("nothing", false),
        ("role grant", true),
        ("deny override", false),
        ("grant override", true),
    ];

    for (label, expected) in situations {
        match label {
            "role grant" => grant_to_role(&mut tx, &fx).await,
            "deny override" => override_for(&mut tx, &fx, fx.resource_id, "deny").await,
            "grant override" => override_for(&mut tx, &fx, fx.resource_id, "grant").await,
            _ => {}
        }

        let gate = allowed(&can_call(&mut tx, fx.person_id, &fx.endpoint).await);
        let ui = permissions(&profile(&mut tx, fx.person_id, "desktop").await)
            .contains(&fx.resource_key);

        assert_eq!(gate, expected, "{label}: gate said {gate}");
        assert_eq!(
            ui, gate,
            "{label}: the profile says {ui} and the gate says {gate} — they must never disagree"
        );
    }
}
