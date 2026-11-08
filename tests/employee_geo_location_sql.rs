//! DB-backed tests for sql/010_employee_geo_location.sql.
//!
//! Each test opens a transaction, applies the migration INSIDE it, runs, and
//! drops the transaction — so the tests work whether or not 010 has been
//! applied to the database yet, and leave it exactly as they found it.
//! An existing employee is borrowed through `attendance.employees`.
//!
//! Requires `DATABASE_URL`; without it every test skips.
//!
//!     cargo test --test employee_geo_location_sql

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, Executor, PgPool, Postgres, Transaction};

const MIGRATION: &str = include_str!("../sql/010_employee_geo_location.sql");

// Point A and a position ~15m from it; FAR is ~7.5km away.
const A_LAT: f64 = 23.7;
const A_LONG: f64 = 90.4;
const NEAR_LAT: f64 = 23.7001;
const NEAR_LONG: f64 = 90.4001;
const FAR_LAT: f64 = 23.75;
const FAR_LONG: f64 = 90.45;
const ADMIN: i64 = 1;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new().max_connections(2).connect(&url).await.ok()
}

macro_rules! db_or_skip {
    () => {
        match pool().await {
            Some(p) => p,
            None => {
                eprintln!("skipping: DATABASE_URL unset or unreachable");
                return;
            }
        }
    };
}

struct Fx {
    emp: String,
    office: String,
}

/// Apply 010, borrow an employee whose office exists in `ictcell.body`, and
/// give them an active Employee enrollment.
async fn setup(tx: &mut Transaction<'_, Postgres>) -> Fx {
    // Only when not yet applied: re-running it in parallel transactions makes
    // them contend for ALTER TABLE / CREATE OR REPLACE locks and fail.
    let applied: bool = sqlx::query_scalar(
        "SELECT to_regproc('attendance.employee_geo_location_verify') IS NOT NULL",
    )
    .fetch_one(&mut **tx)
    .await
    .unwrap();
    if !applied {
        tx.execute(MIGRATION).await.expect("apply sql/010");
    }
    let (emp, office) = sqlx::query_as::<_, (String, String)>(
        "SELECT e.emp_id, e.office FROM attendance.employees e
          WHERE e.emp_id IS NOT NULL AND e.office IS NOT NULL
            AND EXISTS (SELECT 1 FROM ictcell.body b WHERE b.body_code = e.office)
          LIMIT 1",
    )
    .fetch_one(&mut **tx)
    .await
    .expect("need at least one employee with a known office");
    sqlx::query("INSERT INTO attendance.wow_attendance_enrollments (person_id, id_type) VALUES ($1, 'Employee')")
        .bind(&emp)
        .execute(&mut **tx)
        .await
        .unwrap();
    Fx { emp, office }
}

async fn call(tx: &mut Transaction<'_, Postgres>, sql: &str) -> Value {
    sqlx::query_scalar::<_, Value>(sql).fetch_one(&mut **tx).await.unwrap()
}

async fn create_point(tx: &mut Transaction<'_, Postgres>, office: &str, lat: f64, long: f64) -> i64 {
    let v = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.geo_office_create($1, $2, $3, 120, 'ACTIVE', NULL, $4)",
    )
    .bind(office)
    .bind(lat)
    .bind(long)
    .bind(ADMIN)
    .fetch_one(&mut **tx)
    .await
    .unwrap();
    assert_eq!(v["status"], "success", "{v}");
    v["data"]["id"].as_i64().unwrap()
}

async fn select(tx: &mut Transaction<'_, Postgres>, emp: &str, ids: &[i64]) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_select($1, $2)")
        .bind(emp)
        .bind(ids)
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

async fn review(tx: &mut Transaction<'_, Postgres>, id: i64, action: &str, note: Option<&str>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_review($1, $2, $3, $4)")
        .bind(id)
        .bind(action)
        .bind(note)
        .bind(ADMIN)
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

async fn verify(tx: &mut Transaction<'_, Postgres>, emp: &str, lat: f64, long: f64, acc: Option<f64>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_location_verify($1, $2, $3, $4)")
        .bind(emp)
        .bind(lat)
        .bind(long)
        .bind(acc)
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

async fn set_mode(tx: &mut Transaction<'_, Postgres>, mode: &str) {
    sqlx::query("UPDATE attendance.system_settings SET value = $1 WHERE key = 'GEO_LOCATION_MODE'")
        .bind(mode)
        .execute(&mut **tx)
        .await
        .unwrap();
}

#[tokio::test]
async fn office_create_validates() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let fx = setup(&mut tx).await;

    let v = call(&mut tx, "SELECT attendance.geo_office_create('no-such-office', 23.7, 90.4, 100, 'ACTIVE', NULL, 1)").await;
    assert_eq!(v["code"], "OFFICE_NOT_FOUND");
    let v = call(&mut tx, &format!("SELECT attendance.geo_office_create('{}', 23.7, 90.4, 10, NULL, NULL, 1)", fx.office)).await;
    assert_eq!(v["code"], "INVALID_INPUT", "radius below 30");
    let v = call(&mut tx, &format!("SELECT attendance.geo_office_create('{}', 91, 90.4, 100, NULL, NULL, 1)", fx.office)).await;
    assert_eq!(v["code"], "INVALID_INPUT", "latitude out of range");

    let id = create_point(&mut tx, &fx.office, A_LAT, A_LONG).await;
    let list = call(&mut tx, "SELECT attendance.geo_office_available()").await;
    assert!(list["data"].as_array().unwrap().iter().any(|p| p["id"] == id));
}

#[tokio::test]
async fn full_approval_flow_and_gate() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let fx = setup(&mut tx).await;
    set_mode(&mut tx, "APPROVED_OFFICE").await;
    let g1 = create_point(&mut tx, &fx.office, A_LAT, A_LONG).await;

    // Strict mode, nothing approved yet.
    assert_eq!(verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, None).await["code"], "NO_APPROVED_LOCATION");

    let sel = select(&mut tx, &fx.emp, &[g1, g1]).await;
    assert_eq!(sel["status"], "success", "{sel}");
    let row = sel["data"][0]["id"].as_i64().unwrap();
    assert_eq!(sel["data"].as_array().unwrap().len(), 1, "duplicate ids collapse");
    assert_eq!(select(&mut tx, &fx.emp, &[g1]).await["code"], "ALREADY_SELECTED");

    // PENDING does not count.
    assert_eq!(verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, None).await["code"], "NO_APPROVED_LOCATION");

    assert_eq!(review(&mut tx, row, "APPROVE", None).await["data"]["status"], "APPROVED");
    let ok = verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, Some(10.0)).await;
    assert_eq!(ok["verified"], true, "{ok}");
    assert_eq!(ok["data"]["employee_geo_location_id"], row);
    assert_eq!(ok["data"]["geo_location_office_id"], g1);

    assert_eq!(verify(&mut tx, &fx.emp, FAR_LAT, FAR_LONG, None).await["code"], "OUT_OF_LOCATION");
    assert_eq!(verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, Some(80.0)).await["code"], "GPS_ACCURACY_TOO_LOW");

    // INACTIVE point blocks even an APPROVED selection.
    call(&mut tx, &format!("SELECT attendance.geo_office_set_status({g1}, 'INACTIVE', 1)")).await;
    assert_eq!(verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, None).await["code"], "NO_APPROVED_LOCATION");

    let fails: i64 = sqlx::query_scalar("SELECT count(*) FROM attendance.geo_attendance_attempt_log WHERE employee_id = $1")
        .bind(&fx.emp)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(fails, 5);
}

#[tokio::test]
async fn transitions_and_reasons() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let fx = setup(&mut tx).await;
    let g1 = create_point(&mut tx, &fx.office, A_LAT, A_LONG).await;
    let g2 = create_point(&mut tx, &fx.office, FAR_LAT, FAR_LONG).await;
    let sel = select(&mut tx, &fx.emp, &[g1, g2]).await;
    let ids: Vec<i64> = sel["data"].as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect();
    let (r1, r2) = (ids[0], ids[1]);

    assert_eq!(review(&mut tx, r1, "REJECT", Some("")).await["code"], "REASON_REQUIRED");
    assert_eq!(review(&mut tx, r1, "REVOKE", Some("nope")).await["code"], "INVALID_STATUS_TRANSITION");
    assert_eq!(review(&mut tx, r1, "APPROVE", None).await["status"], "success");
    assert_eq!(review(&mut tx, r1, "APPROVE", None).await["code"], "INVALID_STATUS_TRANSITION");
    assert_eq!(review(&mut tx, r1, "REVOKE", Some("moved office")).await["data"]["status"], "REVOKED");

    // Revoked: only an admin can give it back.
    let g1_id = sel["data"][0]["geo_location_office_id"].as_i64().unwrap();
    assert_eq!(select(&mut tx, &fx.emp, &[g1_id]).await["code"], "OFFICE_REVOKED");
    let a = sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_assign($1, $2, NULL, 1)")
        .bind(&fx.emp)
        .bind(g1_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(a["data"]["status"], "APPROVED", "{a}");

    // Rejected: the employee may ask again, and it goes back to PENDING.
    let bulk = sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_bulk_review('REJECT', $1, 'wrong office', 1)")
        .bind(vec![r2, 999_999_999_i64])
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(bulk["data"]["succeeded"].as_array().unwrap().len(), 1, "{bulk}");
    assert_eq!(bulk["data"]["failed"][0]["code"], "NOT_FOUND");
    let g2_id = sel["data"][1]["geo_location_office_id"].as_i64().unwrap();
    assert_eq!(select(&mut tx, &fx.emp, &[g2_id]).await["data"][0]["status"], "PENDING");

    let hist = call(&mut tx, &format!("SELECT attendance.employee_geo_history({r1})")).await;
    let actions: Vec<&str> = hist["data"]["history"].as_array().unwrap().iter().map(|h| h["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["REQUEST", "APPROVE", "REVOKE", "ASSIGN"]);
}

#[tokio::test]
async fn employee_scoping_and_limits() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let fx = setup(&mut tx).await;
    let g1 = create_point(&mut tx, &fx.office, A_LAT, A_LONG).await;

    // Another person's row reads as not found.
    let row = select(&mut tx, &fx.emp, &[g1]).await["data"][0]["id"].as_i64().unwrap();
    let v = sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_remove('0000000000', $1)")
        .bind(row)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(v["code"], "NOT_FOUND");
    let v = sqlx::query_scalar::<_, Value>("SELECT attendance.employee_geo_remove($1, $2)")
        .bind(&fx.emp)
        .bind(row)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(v["data"]["status"], "REMOVED");

    // Not an employee / not enrolled.
    assert_eq!(select(&mut tx, "0000000000", &[g1]).await["code"], "EMPLOYEE_NOT_FOUND");
    sqlx::query("UPDATE attendance.wow_attendance_enrollments SET is_active = false WHERE person_id = $1")
        .bind(&fx.emp)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(select(&mut tx, &fx.emp, &[g1]).await["code"], "NOT_ENROLLED");
    sqlx::query("INSERT INTO attendance.wow_attendance_enrollments (person_id, id_type) VALUES ($1, 'Employee')")
        .bind(&fx.emp)
        .execute(&mut *tx)
        .await
        .unwrap();

    // Limit.
    sqlx::query("UPDATE attendance.system_settings SET value = '2' WHERE key = 'GEO_MAX_ACTIVE_LOCATIONS'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let g2 = create_point(&mut tx, &fx.office, FAR_LAT, FAR_LONG).await;
    let g3 = create_point(&mut tx, &fx.office, 23.6, 90.3).await;
    assert_eq!(select(&mut tx, &fx.emp, &[g1, g2, g3]).await["code"], "LOCATION_LIMIT_EXCEEDED");
    assert_eq!(select(&mut tx, &fx.emp, &[g1, g2]).await["status"], "success");
}

#[tokio::test]
async fn fallback_mode_uses_body_mapping_until_approved() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let fx = setup(&mut tx).await;
    set_mode(&mut tx, "APPROVED_OFFICE_OR_BODY_MAPPING").await;

    let before = verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, None).await;
    assert_eq!(before["data"]["source"], "BODY_MAPPING", "{before}");

    let g1 = create_point(&mut tx, &fx.office, A_LAT, A_LONG).await;
    let row = select(&mut tx, &fx.emp, &[g1]).await["data"][0]["id"].as_i64().unwrap();
    review(&mut tx, row, "APPROVE", None).await;
    let after = verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, None).await;
    assert_eq!(after["data"]["source"], "APPROVED_OFFICE", "{after}");
    assert_eq!(after["verified"], true);

    set_mode(&mut tx, "BODY_MAPPING").await;
    assert_eq!(verify(&mut tx, &fx.emp, NEAR_LAT, NEAR_LONG, None).await["data"]["source"], "BODY_MAPPING");
}

#[tokio::test]
async fn record_location_is_stamped() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let fx = setup(&mut tx).await;
    let rec = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.wow_attendance_verify($1, 'Employee', 't', NULL, '/x.jpg', true, 1.0)",
    )
    .bind(&fx.emp)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let id: uuid_str::Id = rec["data"]["attendance_id"].as_str().unwrap().into();
    sqlx::query("SELECT attendance.wow_attendance_record_location($1::uuid, $2, $3, $4)")
        .bind(&id.0)
        .bind(NEAR_LAT)
        .bind(NEAR_LONG)
        .bind(serde_json::json!({"employee_geo_location_id": 7, "geo_location_office_id": 3, "distance_meters": 15.08}))
        .execute(&mut *tx)
        .await
        .unwrap();
    let (egl, glo, dist): (Option<i64>, Option<i64>, Option<f64>) = sqlx::query_as(
        "SELECT employee_geo_location_id, geo_location_office_id, distance_meters::float8
           FROM attendance.wow_attendance_records WHERE id = $1::uuid",
    )
    .bind(&id.0)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!((egl, glo, dist), (Some(7), Some(3), Some(15.08)));
}

/// Keeps the uuid as text so the test needs no extra sqlx type plumbing.
mod uuid_str {
    pub struct Id(pub String);
    impl From<&str> for Id {
        fn from(s: &str) -> Self {
            Id(s.to_string())
        }
    }
}
