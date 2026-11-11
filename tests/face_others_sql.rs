//! DB-backed tests for Others Selfie Attendance — `sql/012_face_attendance_others.sql`,
//! behind `/ext-api/face-attendance/others/*`.
//!
//! What matters is what a colleague CANNOT do: replace a photo the employee
//! registered themselves, take over another colleague's enrollment, or enroll
//! themselves through the others path.
//!
//! Each test applies 012 inside its own transaction and rolls back, so it runs
//! whether or not the migration is live, and never leaves rows behind.
//!
//!     cargo test --test face_others_sql

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, Executor, PgPool, Postgres, Transaction};

const MIGRATION: &str = include_str!("../sql/012_face_attendance_others.sql");

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new().max_connections(2).connect(&url).await.ok()
}

/// A transaction with 012 applied, and two staff ids with no enrollment.
async fn setup() -> Option<(Transaction<'static, Postgres>, String, String)> {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL unset or unreachable");
        return None;
    };
    let mut tx = pool.begin().await.expect("begin");
    (&mut *tx).execute(MIGRATION).await.expect("apply 012");
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT emp_id FROM attendance.employees e
          WHERE emp_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM attendance.wow_attendance_enrollments w WHERE w.person_id = e.emp_id)
          ORDER BY emp_id LIMIT 2",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("employees");
    if ids.len() < 2 {
        eprintln!("skipping: need two unenrolled employees");
        return None;
    }
    Some((tx, ids[0].clone(), ids[1].clone()))
}

async fn enroll_for(tx: &mut Transaction<'_, Postgres>, employee: &str, by: &str) -> Value {
    sqlx::query_scalar("SELECT attendance.face_enroll_for_other($1, $2, 'tok', '{}'::jsonb, ARRAY['/tmp/x.jpg'])")
        .bind(employee)
        .bind(by)
        .fetch_one(&mut **tx)
        .await
        .expect("face_enroll_for_other")
}

async fn self_enroll(tx: &mut Transaction<'_, Postgres>, employee: &str) {
    sqlx::query("SELECT attendance.wow_attendance_enroll($1, 'Employee', 'tok', '{}'::jsonb, ARRAY['/tmp/s.jpg'])")
        .bind(employee)
        .execute(&mut **tx)
        .await
        .expect("self enroll");
}

fn code(v: &Value) -> &str {
    v.get("code").and_then(Value::as_str).unwrap_or("")
}

#[tokio::test]
async fn enrolls_and_stamps_enrolled_by() {
    let Some((mut tx, emp, by)) = setup().await else { return };
    let r = enroll_for(&mut tx, &emp, &by).await;
    assert_eq!(code(&r), "OK", "{r}");
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT enrolled_by FROM attendance.wow_attendance_enrollments WHERE person_id = $1 AND is_active",
    )
    .bind(&emp)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some(by.as_str()));

    // The same enroller may replace the photo.
    let again = enroll_for(&mut tx, &emp, &by).await;
    assert_eq!(code(&again), "OK", "{again}");
    assert_eq!(again["data"]["is_reenrollment"], Value::Bool(true));

    let list: Value = sqlx::query_scalar("SELECT attendance.face_enrolled_by($1)")
        .bind(&by)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1, "one active enrollment, not one per version");
    assert_eq!(list[0]["employee_id"], Value::String(emp));
    assert_eq!(list[0]["marked_today"], Value::Bool(false));
}

#[tokio::test]
async fn cannot_replace_a_self_enrollment() {
    let Some((mut tx, emp, by)) = setup().await else { return };
    self_enroll(&mut tx, &emp).await;
    assert_eq!(code(&enroll_for(&mut tx, &emp, &by).await), "ALREADY_SELF_ENROLLED");
}

#[tokio::test]
async fn cannot_take_over_another_enrollers_employee() {
    let Some((mut tx, emp, by)) = setup().await else { return };
    assert_eq!(code(&enroll_for(&mut tx, &emp, &by).await), "OK");
    let r = enroll_for(&mut tx, &emp, "0000000001").await;
    assert_eq!(code(&r), "ENROLLED_BY_OTHER");
    assert_eq!(r["data"]["enrolled_by"], Value::String(by));
}

#[tokio::test]
async fn employee_can_take_over_by_self_enrolling() {
    let Some((mut tx, emp, by)) = setup().await else { return };
    assert_eq!(code(&enroll_for(&mut tx, &emp, &by).await), "OK");
    self_enroll(&mut tx, &emp).await;
    let list: Value = sqlx::query_scalar("SELECT attendance.face_enrolled_by($1)")
        .bind(&by)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(list, Value::Array(vec![]), "no longer the enroller's");
}

#[tokio::test]
async fn cannot_enroll_yourself_through_others() {
    let Some((mut tx, emp, _)) = setup().await else { return };
    assert_eq!(code(&enroll_for(&mut tx, &emp, &emp).await), "USE_SELF_ENROLL");
}
