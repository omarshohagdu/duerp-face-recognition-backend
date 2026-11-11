//! DB-backed tests for Office Location (Others) — `sql/013_geo_location_others.sql`,
//! behind `/ext-api/geo-location/others/*`.
//!
//! The guard is the point: a colleague may request offices only for an
//! employee whose ACTIVE enrollment names them as the enroller, and the audit
//! trail must name the colleague, not the employee.
//!
//! Applies 012 + 013 inside a transaction and rolls back.
//!
//!     cargo test --test geo_others_sql

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, Executor, Postgres, Transaction};

const M012: &str = include_str!("../sql/012_face_attendance_others.sql");
const M013: &str = include_str!("../sql/013_geo_location_others.sql");

struct Fx {
    tx: Transaction<'static, Postgres>,
    emp: String,
    by: String,
    office: i64,
}

async fn setup() -> Option<Fx> {
    let url = std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty());
    let Some(pool) = (match url {
        Some(u) => PgPoolOptions::new().max_connections(2).connect(&u).await.ok(),
        None => None,
    }) else {
        eprintln!("skipping: DATABASE_URL unset or unreachable");
        return None;
    };
    let mut tx = pool.begin().await.expect("begin");
    (&mut *tx).execute(M012).await.expect("apply 012");
    (&mut *tx).execute(M013).await.expect("apply 013");
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT emp_id FROM attendance.employees e
          WHERE emp_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM attendance.wow_attendance_enrollments w WHERE w.person_id = e.emp_id)
            AND NOT EXISTS (SELECT 1 FROM attendance.employee_geo_location g WHERE g.employee_id = e.emp_id)
          ORDER BY emp_id LIMIT 2",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("employees");
    let office: Option<i64> =
        sqlx::query_scalar("SELECT id FROM attendance.geo_location_office WHERE status = 'ACTIVE' ORDER BY id LIMIT 1")
            .fetch_optional(&mut *tx)
            .await
            .expect("office");
    let (Some(office), true) = (office, ids.len() == 2) else {
        eprintln!("skipping: need two fresh employees and an ACTIVE office");
        return None;
    };
    Some(Fx { tx, emp: ids[0].clone(), by: ids[1].clone(), office })
}

async fn call(tx: &mut Transaction<'_, Postgres>, sql: &str, emp: &str, by: &str, extra: Option<i64>) -> Value {
    let q = sqlx::query_scalar::<_, Value>(sql).bind(emp).bind(by);
    let q = match (sql.contains("$3"), extra) {
        (true, Some(x)) if sql.contains("select") => q.bind(vec![x]),
        (true, Some(x)) => q.bind(x),
        _ => q,
    };
    q.fetch_one(&mut **tx).await.expect(sql)
}

const MY: &str = "SELECT attendance.employee_geo_others_my($1, $2)";
const SELECT: &str = "SELECT attendance.employee_geo_others_select($1, $2, $3)";
const REMOVE: &str = "SELECT attendance.employee_geo_others_remove($1, $2, $3)";

fn code(v: &Value) -> &str {
    v.get("code").and_then(Value::as_str).unwrap_or("")
}
fn ok(v: &Value) -> bool {
    v.get("status").and_then(Value::as_str) == Some("success")
}

async fn enroll_for(tx: &mut Transaction<'_, Postgres>, emp: &str, by: &str) {
    let r: Value = sqlx::query_scalar("SELECT attendance.face_enroll_for_other($1, $2, 't', '{}', ARRAY['/tmp/x.jpg'])")
        .bind(emp)
        .bind(by)
        .fetch_one(&mut **tx)
        .await
        .unwrap();
    assert_eq!(code(&r), "OK", "{r}");
}

#[tokio::test]
async fn enroller_requests_lists_and_removes() {
    let Some(mut f) = setup().await else { return };
    enroll_for(&mut f.tx, &f.emp, &f.by).await;

    let r = call(&mut f.tx, SELECT, &f.emp, &f.by, Some(f.office)).await;
    assert!(ok(&r), "{r}");
    let row = r["data"][0]["id"].as_i64().unwrap();
    assert_eq!(r["data"][0]["status"], "PENDING");

    // Audit names the colleague.
    let actor: String = sqlx::query_scalar(
        "SELECT actor_id FROM attendance.employee_geo_location_audit WHERE employee_geo_location_id = $1 AND action = 'REQUEST'",
    )
    .bind(row)
    .fetch_one(&mut *f.tx)
    .await
    .unwrap();
    assert_eq!(actor, f.by);

    let my = call(&mut f.tx, MY, &f.emp, &f.by, None).await;
    assert!(ok(&my) && my["data"].as_array().unwrap().len() == 1, "{my}");

    let rm = call(&mut f.tx, REMOVE, &f.emp, &f.by, Some(row)).await;
    assert!(ok(&rm), "{rm}");
    assert_eq!(rm["data"]["status"], "REMOVED");
}

#[tokio::test]
async fn refuses_an_employee_you_did_not_enroll() {
    let Some(mut f) = setup().await else { return };
    enroll_for(&mut f.tx, &f.emp, &f.by).await;
    let r = call(&mut f.tx, SELECT, &f.emp, "0000000001", Some(f.office)).await;
    assert_eq!(code(&r), "NOT_ENROLLED_BY_YOU");
    assert_eq!(code(&call(&mut f.tx, MY, &f.emp, "0000000001", None).await), "NOT_ENROLLED_BY_YOU");
}

#[tokio::test]
async fn refuses_unenrolled_and_self() {
    let Some(mut f) = setup().await else { return };
    assert_eq!(code(&call(&mut f.tx, SELECT, &f.emp, &f.by, Some(f.office)).await), "NOT_ENROLLED");
    assert_eq!(code(&call(&mut f.tx, MY, &f.by, &f.by, None).await), "USE_SELF_OFFICE");
}

#[tokio::test]
async fn self_enrolled_employee_is_out_of_reach() {
    let Some(mut f) = setup().await else { return };
    enroll_for(&mut f.tx, &f.emp, &f.by).await;
    sqlx::query("SELECT attendance.wow_attendance_enroll($1, 'Employee', 't', '{}', ARRAY['/tmp/s.jpg'])")
        .bind(&f.emp)
        .execute(&mut *f.tx)
        .await
        .unwrap();
    assert_eq!(code(&call(&mut f.tx, MY, &f.emp, &f.by, None).await), "NOT_ENROLLED_BY_YOU");
}
