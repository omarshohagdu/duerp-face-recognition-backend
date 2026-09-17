//! DB-backed tests for the role-administration functions —
//! `sql/007_access_admin.sql`, behind `POST /ext-api/access/*`.
//!
//! This is the API that hands out permissions, so what it REFUSES matters more
//! than what it does. Two of those refusals exist to stop an administrator
//! locking every administrator out, and they are the reason the guards live in
//! SQL rather than in the handler: they hold for a DBA at a psql prompt too.
//!
//! Every test runs inside a transaction that is dropped without committing, and
//! every fixture key carries a per-test random suffix, so a run against a
//! database holding the real roles cannot collide with them.
//!
//!     psql "$DATABASE_URL" -f sql/007_access_admin.sql
//!     cargo test --test access_admin_sql

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new().max_connections(2).connect(&url).await.ok()
}

async fn migration_applied(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT count(*) = 6 FROM pg_proc p
           JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'attendance'
            AND p.proname IN ('admin_roles_list', 'admin_resources_list', 'admin_role_save',
                              'admin_role_delete', 'admin_users_list', 'admin_user_set_role')",
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
                    eprintln!("skipping: apply sql/007_access_admin.sql first");
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
    uuid::Uuid::new_v4().simple().to_string()[..8].to_lowercase()
}

/// The actor recorded on every audit row. A real person id is not needed — the
/// functions record whatever they are given.
const ACTOR: i64 = 45320;

async fn role_save(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    name: &str,
    permissions: &[&str],
) -> Value {
    let perms: Vec<String> = permissions.iter().map(|s| s.to_string()).collect();
    sqlx::query_scalar::<_, Value>("SELECT attendance.admin_role_save($1,$2,$3,$4,$5)")
        .bind(key)
        .bind(name)
        .bind(perms)
        .bind(ACTOR)
        .bind("127.0.0.1")
        .fetch_one(&mut **tx)
        .await
        .expect("role_save")
}

async fn role_delete(tx: &mut Transaction<'_, Postgres>, key: &str) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.admin_role_delete($1,$2,$3)")
        .bind(key)
        .bind(ACTOR)
        .bind("127.0.0.1")
        .fetch_one(&mut **tx)
        .await
        .expect("role_delete")
}

async fn set_role(tx: &mut Transaction<'_, Postgres>, person: i64, role: Option<&str>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.admin_user_set_role($1,$2,$3,$4)")
        .bind(person)
        .bind(role)
        .bind(ACTOR)
        .bind("127.0.0.1")
        .fetch_one(&mut **tx)
        .await
        .expect("set_role")
}

async fn override_set(
    tx: &mut Transaction<'_, Postgres>,
    person: i64,
    permission: &str,
    effect: &str,
) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.admin_user_override($1,$2,$3,$4,$5)")
        .bind(person)
        .bind(permission)
        .bind(effect)
        .bind(ACTOR)
        .bind("127.0.0.1")
        .fetch_one(&mut **tx)
        .await
        .expect("override_set")
}

fn ok(v: &Value) -> bool {
    v.get("status").and_then(Value::as_str) == Some("success")
}

fn code(v: &Value) -> String {
    v.get("code").and_then(Value::as_str).unwrap_or("").to_string()
}

/// A throwaway account, so no test touches a real person's role.
async fn person(tx: &mut Transaction<'_, Postgres>) -> i64 {
    let u = unique();
    let person_id: i64 = 9_500_000_000_000 + (u64::from_str_radix(&u, 16).unwrap_or(0) % 1_000_000) as i64;
    sqlx::query("INSERT INTO attendance.app_users (person_id, username, status) VALUES ($1,$2,'active')")
        .bind(person_id)
        .bind(format!("admintest-{u}@du.ac.bd"))
        .execute(&mut **tx)
        .await
        .expect("person fixture");
    person_id
}

async fn permissions_of(tx: &mut Transaction<'_, Postgres>, key: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT res.key FROM attendance.role_permissions rp
           JOIN attendance.roles r ON r.id = rp.role_id
           JOIN attendance.resources res ON res.id = rp.resource_id
          WHERE r.key = $1 ORDER BY res.key",
    )
    .bind(key)
    .fetch_all(&mut **tx)
    .await
    .expect("permissions")
}

// =====================================================================
// Roles
// =====================================================================

#[tokio::test]
async fn saving_a_role_replaces_its_permission_set() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let key = format!("t_{}", unique());

    let created = role_save(&mut tx, &key, "Test", &["nfc.card.read", "dashboard.view"]).await;
    assert!(ok(&created), "{created}");
    assert_eq!(created["data"]["created"], true);
    assert_eq!(permissions_of(&mut tx, &key).await, vec!["dashboard.view", "nfc.card.read"]);

    // Unticking a box is a revocation: the set sent REPLACES what was there.
    // A partial list silently stripping the rest is the behaviour a tick-box
    // screen needs, and the reason the API takes the whole set every time.
    let updated = role_save(&mut tx, &key, "Test", &["dashboard.view"]).await;
    assert!(ok(&updated));
    assert_eq!(updated["data"]["created"], false);
    assert_eq!(permissions_of(&mut tx, &key).await, vec!["dashboard.view"]);
}

#[tokio::test]
async fn a_role_cannot_take_admin_roles_manage_away_from_admin() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // THE lockout guard. `admin.roles.manage` is the permission that reaches
    // this API; removing it from `admin` leaves nobody able to give it back,
    // and a psql prompt is then the only way home.
    let r = role_save(&mut tx, "admin", "Admin", &["dashboard.view"]).await;
    assert!(!ok(&r), "{r}");
    assert_eq!(code(&r), "would_lock_out");
    assert!(permissions_of(&mut tx, "admin").await.contains(&"admin.roles.manage".to_string()));
}

#[tokio::test]
async fn a_role_key_has_to_be_an_identifier() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // The key ends up in queries, in this file's guards and in duerp-api's
    // joins. Anything else is refused rather than quietly sanitised.
    for bad in ["Bad Key!", "1role", "x", "role-with-dash", ""] {
        let r = role_save(&mut tx, bad, "X", &["dashboard.view"]).await;
        assert!(!ok(&r), "{bad:?} should be refused: {r}");
        assert!(
            matches!(code(&r).as_str(), "invalid_key" | "missing_fields"),
            "{bad:?} gave {}",
            code(&r)
        );
    }
}

#[tokio::test]
async fn an_unknown_permission_is_refused_not_dropped() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let key = format!("t_{}", unique());

    // Silently ignoring it would leave the screen showing a permission the
    // role does not hold — and the save reporting success.
    let r = role_save(&mut tx, &key, "Test", &["dashboard.view", "no.such.permission"]).await;
    assert!(!ok(&r), "{r}");
    assert_eq!(code(&r), "unknown_permission");
    assert!(permissions_of(&mut tx, &key).await.is_empty(), "nothing should have been written");
}

#[tokio::test]
async fn deleting_a_role_is_refused_while_it_is_in_use_or_is_a_system_role() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let key = format!("t_{}", unique());
    role_save(&mut tx, &key, "Test", &["dashboard.view"]).await;

    // System roles keep their keys: duerp-api still joins on them.
    assert_eq!(code(&role_delete(&mut tx, "admin").await), "system_role");

    // A member would be left with no role — which reads as "denied everything"
    // the moment enforcement is on, and nobody would connect the two.
    let p = person(&mut tx).await;
    assert!(ok(&set_role(&mut tx, p, Some(&key)).await));
    let r = role_delete(&mut tx, &key).await;
    assert_eq!(code(&r), "role_in_use");
    assert_eq!(r["data"]["members"], 1);

    // Free the member and it goes.
    assert!(ok(&set_role(&mut tx, p, None).await));
    assert!(ok(&role_delete(&mut tx, &key).await));
}

// =====================================================================
// People
// =====================================================================

#[tokio::test]
async fn the_last_active_admin_cannot_be_demoted() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // Clear every real admin inside this transaction, leaving one, then try to
    // clear that one too. Demoting the last administrator leaves nobody who can
    // promote anyone — the same lockout as the role guard, from the other end.
    let admins: Vec<i64> = sqlx::query_scalar(
        "SELECT au.person_id FROM attendance.app_users au
           JOIN attendance.roles r ON r.id = au.role_id
          WHERE r.key = 'admin' AND au.status = 'active' ORDER BY au.person_id",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("admins");

    // A fixture admin, so the test works on a database with none.
    let mine = person(&mut tx).await;
    assert!(ok(&set_role(&mut tx, mine, Some("admin")).await));

    for other in &admins {
        assert!(ok(&set_role(&mut tx, *other, None).await), "demoting {other}");
    }

    let r = set_role(&mut tx, mine, None).await;
    assert!(!ok(&r), "the last admin must not be demotable: {r}");
    assert_eq!(code(&r), "last_admin");
}

#[tokio::test]
async fn an_override_grants_denies_and_clears() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let p = person(&mut tx).await;

    assert!(ok(&override_set(&mut tx, p, "nfc.card.reassign", "grant").await));
    assert_eq!(
        override_set(&mut tx, p, "nfc.card.reassign", "deny").await["data"]["effect"],
        "deny",
        "an override flips in place — the PK is (user, resource)"
    );
    // `clear` removes it and lets the role decide again, which is different
    // from writing the opposite effect.
    assert!(override_set(&mut tx, p, "nfc.card.reassign", "clear").await["data"]["effect"].is_null());

    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM attendance.user_permission_overrides o
           JOIN attendance.app_users au ON au.id = o.user_id WHERE au.person_id = $1",
    )
    .bind(p)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    assert_eq!(left, 0);

    for bad in ["allow", "", "GRANTED"] {
        assert_eq!(code(&override_set(&mut tx, p, "nfc.card.reassign", bad).await), "invalid_effect");
    }
}

#[tokio::test]
async fn a_change_for_somebody_who_has_no_account_is_refused() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    assert_eq!(code(&set_role(&mut tx, 9_999_999_999_999, Some("admin")).await), "no_account");
    assert_eq!(
        code(&override_set(&mut tx, 9_999_999_999_999, "dashboard.view", "grant").await),
        "no_account"
    );
}

// =====================================================================
// The trail
// =====================================================================

#[tokio::test]
async fn every_write_is_audited_with_its_actor_and_both_sides() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let key = format!("t_{}", unique());
    let p = person(&mut tx).await;

    role_save(&mut tx, &key, "Test", &["dashboard.view", "nfc.card.read"]).await;
    role_save(&mut tx, &key, "Test", &["dashboard.view"]).await;
    set_role(&mut tx, p, Some(&key)).await;
    override_set(&mut tx, p, "nfc.card.reassign", "grant").await;

    let rows: Vec<(i64, String, String, Value)> = sqlx::query_as(
        "SELECT actor, action, target, detail FROM attendance.access_admin_audit
          WHERE target = $1 OR target = $2 ORDER BY id",
    )
    .bind(&key)
    .bind(p.to_string())
    .fetch_all(&mut *tx)
    .await
    .expect("audit rows");

    let actions: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    assert_eq!(actions, vec!["role_saved", "role_saved", "user_role_set", "override_set"]);
    assert!(rows.iter().all(|r| r.0 == ACTOR), "every row names the actor");

    // "What did this change?" has to be answerable from the row alone — the
    // step logs record who called what, not what it altered.
    let narrowed = &rows[1].3;
    assert_eq!(narrowed["before"], serde_json::json!(["dashboard.view", "nfc.card.read"]));
    assert_eq!(narrowed["after"], serde_json::json!(["dashboard.view"]));
    assert_eq!(rows[2].3["to"], key.as_str());
    assert_eq!(rows[3].3["permission"], "nfc.card.reassign");
    assert_eq!(rows[3].3["to"], "grant");
}
