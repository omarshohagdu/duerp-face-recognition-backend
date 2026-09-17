//! DB-backed tests for the NFC face-verification setting —
//! `sql/008_system_settings.sql`, behind `GET|PUT /admin-api/settings/nfc-face-verify`.
//!
//! The switch these cover turns OFF the check that stops one student's card
//! being registered against another student's face. So the tests that matter
//! are the ones about what CANNOT happen: no enabling it without somewhere to
//! send the images, no storing a value that is neither ON nor OFF, and no
//! change that leaves no trace of who made it.
//!
//! Transaction per test, rolled back, so the live setting is never altered.
//!
//!     psql "$DATABASE_URL" -f sql/008_system_settings.sql
//!     cargo test --test system_settings_sql

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new().max_connections(2).connect(&url).await.ok()
}

async fn migration_applied(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT count(*) = 2 FROM pg_proc p
           JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'attendance'
            AND p.proname IN ('nfc_face_verify_get', 'nfc_face_verify_save')",
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
                    eprintln!("skipping: apply sql/008_system_settings.sql first");
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

/// A real account, because `updated_by` is a foreign key: the trail has to
/// name somebody who exists.
const ACTOR: i64 = 4691;
const URL: &str = "https://face.du.ac.bd/verify";

async fn get(tx: &mut Transaction<'_, Postgres>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.nfc_face_verify_get()")
        .fetch_one(&mut **tx)
        .await
        .expect("get")
}

async fn save(tx: &mut Transaction<'_, Postgres>, enabled: &str, url: Option<&str>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.nfc_face_verify_save($1,$2,$3,$4)")
        .bind(enabled)
        .bind(url)
        .bind(ACTOR)
        .bind("127.0.0.1")
        .fetch_one(&mut **tx)
        .await
        .expect("save")
}

fn ok(v: &Value) -> bool {
    v.get("status").and_then(Value::as_str) == Some("success")
}

fn code(v: &Value) -> String {
    v.get("code").and_then(Value::as_str).unwrap_or("").to_string()
}

fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v["data"][key].as_str().unwrap_or("")
}

async fn audit_rows(tx: &mut Transaction<'_, Postgres>) -> Vec<(String, String, String, i64)> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<i64>)>(
        "SELECT setting_key, old_value, new_value, updated_by
           FROM attendance.system_settings_audit ORDER BY id",
    )
    .fetch_all(&mut **tx)
    .await
    .expect("audit")
    .into_iter()
    .map(|(k, o, n, by)| (k, o.unwrap_or_default(), n.unwrap_or_default(), by.unwrap_or(0)))
    .collect()
}

// ---------------------------------------------------------------------

#[tokio::test]
async fn get_returns_the_current_settings() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    let before = get(&mut tx).await;
    assert!(ok(&before));
    // Always both keys, always strings — a client never has to decide what a
    // missing setting means.
    assert!(matches!(field(&before, "nfc_face_verify"), "ON" | "OFF"));
    assert!(before["data"]["nfc_face_verify_url"].is_string());

    save(&mut tx, "OFF", Some(URL)).await;
    let after = get(&mut tx).await;
    assert_eq!(field(&after, "nfc_face_verify"), "OFF");
    assert_eq!(field(&after, "nfc_face_verify_url"), URL);
}

#[tokio::test]
async fn the_toggle_goes_both_ways() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // A URL first, because ON without one is refused — see below.
    assert!(ok(&save(&mut tx, "OFF", Some(URL)).await));

    let on = save(&mut tx, "ON", None).await;
    assert!(ok(&on), "{on}");
    assert_eq!(field(&on, "nfc_face_verify"), "ON");
    // Flipping the switch leaves the endpoint alone: an admin turning the gate
    // off for an hour should not have to retype the URL to turn it back on.
    assert_eq!(field(&on, "nfc_face_verify_url"), URL);

    let off = save(&mut tx, "OFF", None).await;
    assert!(ok(&off));
    assert_eq!(field(&off, "nfc_face_verify"), "OFF");
    assert_eq!(field(&off, "nfc_face_verify_url"), URL);
}

#[tokio::test]
async fn only_on_and_off_are_accepted() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    save(&mut tx, "OFF", Some(URL)).await;

    for bad in ["", "on-ish", "true", "1", "enabled", "Off "] {
        let r = save(&mut tx, bad, None).await;
        if bad == "Off " {
            // Trimmed and upper-cased: a form field with a trailing space is a
            // client quirk, not an operator saying something different.
            assert!(ok(&r), "{bad:?} should be accepted after trimming: {r}");
            continue;
        }
        assert!(!ok(&r), "{bad:?} should be refused: {r}");
        assert_eq!(code(&r), "invalid_value", "for {bad:?}");
    }

    // ...and nothing was stored meanwhile.
    assert!(matches!(field(&get(&mut tx).await, "nfc_face_verify"), "ON" | "OFF"));
}

#[tokio::test]
async fn the_url_must_be_well_formed() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    for bad in [
        "https:/typo.du.ac.bd/verify", // one slash
        "face.du.ac.bd/verify",        // no scheme
        "https://",                    // no host
        "https://host with spaces/verify",
        "ftp://face.du.ac.bd/verify",
    ] {
        let r = save(&mut tx, "OFF", Some(bad)).await;
        assert!(!ok(&r), "{bad:?} should be refused: {r}");
        assert_eq!(code(&r), "invalid_url", "for {bad:?}");
    }

    for good in [
        "https://face.du.ac.bd/verify",
        "https://face.du.ac.bd:8443/verify",
        "https://face-01.du.ac.bd/v1/verify?strict=1",
    ] {
        let r = save(&mut tx, "OFF", Some(good)).await;
        assert!(ok(&r), "{good:?} should be accepted: {r}");
    }
}

#[tokio::test]
async fn http_is_allowed_to_a_private_address_and_nowhere_else() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // The face service runs on the campus network with no certificate, so
    // https-only would mean the switch could never be turned on for the
    // service it exists to control.
    for internal in [
        "http://10.224.224.101:8089/verify", // the real one
        "http://192.168.1.7/verify",
        "http://172.16.0.4:8089/verify",
        "http://172.31.255.254/verify",
        "http://127.0.0.1:8089/verify",
        "http://localhost:8089/verify",
    ] {
        let r = save(&mut tx, "OFF", Some(internal)).await;
        assert!(ok(&r), "{internal:?} should be accepted: {r}");
    }

    // Everything else still has to be https. This is the case that matters:
    // a typo'd `http://face.du.ac.bd/verify` would put photographs of
    // students' faces on the open internet in clear.
    for public in [
        "http://face.du.ac.bd/verify",
        "http://8.8.8.8/verify",
        "http://172.15.0.4/verify",  // just outside 172.16/12
        "http://172.32.0.4/verify",  // just above it
        "http://10.300.1.1/verify",  // not an address at all
        "http://192.169.1.7/verify", // one off 192.168/16
    ] {
        let r = save(&mut tx, "OFF", Some(public)).await;
        assert!(!ok(&r), "{public:?} should be refused: {r}");
        assert_eq!(code(&r), "invalid_url", "for {public:?}");
        // The refusal names the host, so an operator can see what it objected
        // to rather than re-reading their own URL.
        assert!(r["data"]["host"].is_string(), "{public:?}: {r}");
    }

    // A NAME that resolves privately does not count: this rule cannot resolve
    // anything, and a name whose DNS somebody else controls is exactly how
    // "internal" stops being internal.
    let r = save(&mut tx, "OFF", Some("http://face-internal/verify")).await;
    assert_eq!(code(&r), "invalid_url");
}

#[tokio::test]
async fn turning_it_on_without_a_url_is_refused() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // Clear the URL, then try to enable. The state this prevents is the gate
    // ON with nothing to call, which fails EVERY card save with a 503 at the
    // desk — and reads as "the system is broken", not "somebody enabled a
    // feature that was never configured".
    assert!(ok(&save(&mut tx, "OFF", Some("")).await));

    let r = save(&mut tx, "ON", None).await;
    assert!(!ok(&r), "{r}");
    assert_eq!(code(&r), "url_required");
    assert_eq!(
        r["message"],
        "Cannot enable face verification without NFC_FACE_VERIFY_URL"
    );

    // Clearing the URL in the SAME request that enables it is the same state,
    // and is refused the same way.
    let both = save(&mut tx, "ON", Some("")).await;
    assert_eq!(code(&both), "url_required");

    // Nothing was written by either attempt.
    assert_eq!(field(&get(&mut tx).await, "nfc_face_verify"), "OFF");

    // ...and supplying the URL in the enabling request itself works.
    let together = save(&mut tx, "ON", Some(URL)).await;
    assert!(ok(&together), "{together}");
    assert_eq!(field(&together, "nfc_face_verify"), "ON");
}

#[tokio::test]
async fn every_change_is_audited_with_both_sides_and_the_admin() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    save(&mut tx, "OFF", Some(URL)).await;
    save(&mut tx, "ON", None).await;
    save(&mut tx, "OFF", Some("https://other.du.ac.bd/verify")).await;

    let rows = audit_rows(&mut tx).await;
    // One row PER KEY CHANGED — "when did face verification get turned off,
    // and by whom" should not need unpicking a combined record.
    let toggles: Vec<_> = rows.iter().filter(|r| r.0 == "NFC_FACE_VERIFY").collect();
    let urls: Vec<_> = rows.iter().filter(|r| r.0 == "NFC_FACE_VERIFY_URL").collect();

    assert_eq!(toggles.len(), 2, "OFF->ON and ON->OFF: {rows:?}");
    assert_eq!(toggles[0].2, "ON");
    assert_eq!(toggles[1].1, "ON", "the old value is recorded, not just the new");
    assert_eq!(toggles[1].2, "OFF");
    assert_eq!(urls.len(), 2, "the URL was set and later changed: {rows:?}");
    assert_eq!(urls[1].2, "https://other.du.ac.bd/verify");
    assert!(rows.iter().all(|r| r.3 == ACTOR), "every row names the admin");
}

#[tokio::test]
async fn a_change_that_changes_nothing_writes_no_audit_row() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    save(&mut tx, "OFF", Some(URL)).await;

    let before = audit_rows(&mut tx).await.len();
    // Same value, same URL. An audit trail that records changes which did not
    // happen trains people to ignore it.
    assert!(ok(&save(&mut tx, "OFF", Some(URL)).await));
    assert!(ok(&save(&mut tx, "OFF", None).await));
    assert_eq!(audit_rows(&mut tx).await.len(), before);
}

#[tokio::test]
async fn a_write_records_who_made_it_on_the_setting_itself() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    save(&mut tx, "OFF", Some(URL)).await;

    let (by, managed): (Option<i64>, bool) = sqlx::query_as(
        "SELECT updated_by, updated_by IS NOT NULL FROM attendance.system_settings
          WHERE key = 'NFC_FACE_VERIFY'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("row");

    assert_eq!(by, Some(ACTOR));
    // `updated_by IS NOT NULL` is also the flag the reader uses to decide the
    // table is in charge rather than the environment (utils::settings). If it
    // were not set on write, the toggle would go on being ignored.
    assert!(managed);
}
