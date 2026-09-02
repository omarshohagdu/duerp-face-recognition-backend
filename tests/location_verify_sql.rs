//! DB-backed tests for the location-verification SQL functions.
//!
//! Every test runs inside a transaction that is dropped without committing, so
//! the database is left exactly as it was found. Fixtures are created inside
//! that transaction; no test writes to `ictcell.employees` — an existing
//! employee is borrowed instead, which also keeps the tests honest about the
//! real `employees.office` shape.
//!
//! Requires `DATABASE_URL`. Without it every test skips rather than fails, so
//! `cargo test` still works on a machine with no database.
//!
//!     cargo test --test location_verify_sql
//!
//! The functions under test must already be applied:
//!
//!     psql "$DATABASE_URL" -f sql/002_location_verify.sql

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};

// Coordinates used across the tests. The building sits at BUILDING_*; NEAR is
// ~18m away (inside a 50m radius), FAR is ~1.3km (outside any sane radius).
const BUILDING_LAT: f64 = 23.72815;
const BUILDING_LONG: f64 = 90.39925;
const NEAR_LAT: f64 = 23.728310;
const NEAR_LONG: f64 = 90.399250;
const FAR_LAT: f64 = 23.740000;
const FAR_LONG: f64 = 90.410000;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()
}

/// Skip the test (printing why) when there is no database to talk to.
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

/// An employee that actually exists, with a non-empty office. Returns
/// (emp_id, office). The office code is what mappings are keyed on.
async fn borrow_employee(tx: &mut Transaction<'_, Postgres>) -> (String, String) {
    sqlx::query_as::<_, (String, String)>(
        "SELECT emp_id, office FROM ictcell.employees
          WHERE emp_id IS NOT NULL AND office IS NOT NULL AND btrim(office) <> ''
          LIMIT 1",
    )
    .fetch_one(&mut **tx)
    .await
    .expect("no usable employee row found in ictcell.employees")
}

/// Insert an active building + mapping for `office` inside the transaction.
async fn map_building(
    tx: &mut Transaction<'_, Postgres>,
    office: &str,
    name: &str,
    lat: f64,
    long: f64,
    radius: f64,
    is_active: bool,
) -> i32 {
    let building_id: i32 =
        sqlx::query_scalar("INSERT INTO ictcell.buildings (name, status) VALUES ($1, 'Active') RETURNING id")
            .bind(name)
            .fetch_one(&mut **tx)
            .await
            .expect("insert building");

    sqlx::query(
        "INSERT INTO ictcell.body_building_mapping
             (body_code, building_id, lat, \"long\", radius, is_active)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(office)
    .bind(building_id)
    .bind(lat)
    .bind(long)
    .bind(radius)
    .bind(is_active)
    .execute(&mut **tx)
    .await
    .expect("insert mapping");

    building_id
}

/// Call the verify function exactly as the handler does: f64 binds -> float8.
async fn verify(
    tx: &mut Transaction<'_, Postgres>,
    emp_id: &str,
    lat: f64,
    long: f64,
) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT ictcell.wow_attendance_location_verify($1, $2, $3)")
        .bind(emp_id)
        .bind(lat)
        .bind(long)
        .fetch_one(&mut **tx)
        .await
        .expect("location verify call failed")
}

/// Call the admin save function exactly as the handler does.
#[allow(clippy::too_many_arguments)]
async fn save_mapping(
    tx: &mut Transaction<'_, Postgres>,
    body_code: &str,
    building_id: Option<i32>,
    building_name: Option<&str>,
    lat: f64,
    long: f64,
    radius: Option<f64>,
    is_active: Option<bool>,
) -> Value {
    sqlx::query_scalar::<_, Value>(
        "SELECT ictcell.wow_attendance_body_building_mapping_save($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(body_code)
    .bind(building_id)
    .bind(building_name)
    .bind(lat)
    .bind(long)
    .bind(radius)
    .bind(is_active)
    .fetch_one(&mut **tx)
    .await
    .expect("mapping save call failed")
}

fn verified(v: &Value) -> bool {
    v.get("verified").and_then(Value::as_bool).unwrap_or(false)
}

fn message(v: &Value) -> String {
    v.get("message").and_then(Value::as_str).unwrap_or("").to_string()
}

fn distance(v: &Value) -> f64 {
    v.pointer("/data/distance_m").and_then(Value::as_f64).unwrap_or(-1.0)
}

// ---------------------------------------------------------------------
// wow_attendance_location_verify
// ---------------------------------------------------------------------

#[tokio::test]
async fn inside_radius_is_verified() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Test Bhaban", BUILDING_LAT, BUILDING_LONG, 50.0, true).await;

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;

    assert!(verified(&out), "expected verified, got {out}");
    let d = distance(&out);
    assert!((10.0..30.0).contains(&d), "distance {d}m outside the expected ~18m");
    assert_eq!(out.pointer("/data/body_code").and_then(Value::as_str), Some(office.as_str()));
}

#[tokio::test]
async fn outside_radius_is_rejected() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Test Bhaban", BUILDING_LAT, BUILDING_LONG, 50.0, true).await;

    let out = verify(&mut tx, &emp_id, FAR_LAT, FAR_LONG).await;

    assert!(!verified(&out), "expected rejection, got {out}");
    assert!(distance(&out) > 50.0, "distance should exceed the radius: {out}");
    // The distance is still reported, so support can see how far off it was.
    assert_eq!(out.pointer("/data/radius_m").and_then(Value::as_f64), Some(50.0));
}

#[tokio::test]
async fn exact_building_coordinates_do_not_break_asin() {
    // Floating-point error can push asin()'s argument just above 1 when the
    // device sits on the building; without the least(1, ...) guard this raises
    // a math error instead of returning 0m.
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Test Bhaban", BUILDING_LAT, BUILDING_LONG, 50.0, true).await;

    let out = verify(&mut tx, &emp_id, BUILDING_LAT, BUILDING_LONG).await;

    assert!(verified(&out), "expected verified, got {out}");
    assert_eq!(distance(&out), 0.0, "expected 0m at the exact coordinates: {out}");
}

#[tokio::test]
async fn closest_building_wins() {
    // Two mappings for one office: the near one is inside its radius, the far
    // one is not. The function must pick the nearest, not the first row.
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Far Hall", 23.74, 90.41, 50.0, true).await;
    let near = map_building(&mut tx, &office, "Near Hall", BUILDING_LAT, BUILDING_LONG, 50.0, true).await;

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;

    assert!(verified(&out), "expected verified, got {out}");
    assert_eq!(out.pointer("/data/building_id").and_then(Value::as_i64), Some(near as i64));
}

#[tokio::test]
async fn per_building_radius_is_honoured() {
    // Same distance, tighter radius -> rejected. Confirms the radius comes from
    // the mapping row rather than being hardcoded.
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Tiny Lab", BUILDING_LAT, BUILDING_LONG, 5.0, true).await;

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;

    assert!(!verified(&out), "18m should fall outside a 5m radius: {out}");
}

#[tokio::test]
async fn inactive_mapping_is_ignored() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Disabled Hall", BUILDING_LAT, BUILDING_LONG, 50.0, false).await;

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;

    assert!(!verified(&out), "an inactive mapping must not verify: {out}");
    assert!(message(&out).contains("No building mapping"), "got {out}");
}

#[tokio::test]
async fn null_coordinate_mapping_is_ignored() {
    // This is what the staged 490010 row looks like: present but unsurveyed. It
    // must never fence anyone, even though is_active is true here.
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    let building_id: i32 = sqlx::query_scalar(
        "INSERT INTO ictcell.buildings (name, status) VALUES ('Unsurveyed', 'Active') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ictcell.body_building_mapping (body_code, building_id, lat, \"long\", radius, is_active)
         VALUES ($1, $2, NULL, NULL, 50, true)",
    )
    .bind(&office)
    .bind(building_id)
    .execute(&mut *tx)
    .await
    .unwrap();

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;

    assert!(!verified(&out), "a mapping with no coordinates must not verify: {out}");
}

#[tokio::test]
async fn office_with_no_mapping_is_rejected() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, _office) = borrow_employee(&mut tx).await;
    // No mapping inserted at all.

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;

    assert!(!verified(&out));
    assert!(message(&out).contains("No building mapping"), "got {out}");
}

#[tokio::test]
async fn unknown_employee_is_rejected() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();

    let out = verify(&mut tx, "NO-SUCH-EMPLOYEE-9999", NEAR_LAT, NEAR_LONG).await;

    assert!(!verified(&out));
    assert_eq!(message(&out), "Employee not found", "got {out}");
}

#[tokio::test]
async fn invalid_coordinates_are_rejected() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;
    map_building(&mut tx, &office, "Test Bhaban", BUILDING_LAT, BUILDING_LONG, 50.0, true).await;

    for (lat, long) in [(999.0, 90.0), (23.0, 999.0)] {
        let out = verify(&mut tx, &emp_id, lat, long).await;
        assert!(!verified(&out));
        assert_eq!(message(&out), "Invalid device coordinates", "got {out}");
    }
}

// ---------------------------------------------------------------------
// wow_attendance_body_building_mapping_save
// ---------------------------------------------------------------------

#[tokio::test]
async fn save_creates_building_by_name_then_upserts() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (_emp_id, office) = borrow_employee(&mut tx).await;

    let created = save_mapping(
        &mut tx, &office, None, Some("Brand New Bhaban"),
        BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(true),
    ).await;
    assert_eq!(message(&created), "Mapping created", "got {created}");
    assert_eq!(created.pointer("/data/building_created").and_then(Value::as_bool), Some(true));
    let building_id = created.pointer("/data/building_id").and_then(Value::as_i64).unwrap() as i32;

    // Same (body_code, building_id) again -> update, not a duplicate row.
    let updated = save_mapping(
        &mut tx, &office, Some(building_id), None,
        BUILDING_LAT, BUILDING_LONG, Some(120.0), Some(true),
    ).await;
    assert_eq!(message(&updated), "Mapping updated", "got {updated}");

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ictcell.body_building_mapping WHERE body_code = $1 AND building_id = $2",
    )
    .bind(&office)
    .bind(building_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(count, 1, "upsert must not duplicate the mapping");
}

#[tokio::test]
async fn save_defaults_radius_and_active_when_omitted() {
    // The handler sends None for absent optional fields.
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (_emp_id, office) = borrow_employee(&mut tx).await;

    let out = save_mapping(
        &mut tx, &office, None, Some("Defaults Bhaban"),
        BUILDING_LAT, BUILDING_LONG, None, None,
    ).await;

    assert_eq!(out.pointer("/data/radius").and_then(Value::as_f64), Some(50.0), "got {out}");
    assert_eq!(out.pointer("/data/is_active").and_then(Value::as_bool), Some(true), "got {out}");
}

#[tokio::test]
async fn save_then_verify_round_trip() {
    // The path an admin actually takes: save a mapping, then a check-in at that
    // building passes.
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;

    save_mapping(
        &mut tx, &office, None, Some("Round Trip Bhaban"),
        BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(true),
    ).await;

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;
    assert!(verified(&out), "expected verified after save, got {out}");
}

#[tokio::test]
async fn save_deactivation_disables_verification() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (emp_id, office) = borrow_employee(&mut tx).await;

    let saved = save_mapping(
        &mut tx, &office, None, Some("Toggle Bhaban"),
        BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(true),
    ).await;
    let building_id = saved.pointer("/data/building_id").and_then(Value::as_i64).unwrap() as i32;
    assert!(verified(&verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await));

    save_mapping(
        &mut tx, &office, Some(building_id), None,
        BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(false),
    ).await;

    let out = verify(&mut tx, &emp_id, NEAR_LAT, NEAR_LONG).await;
    assert!(!verified(&out), "deactivated mapping must stop verifying: {out}");
}

#[tokio::test]
async fn save_warns_on_unknown_body_code_and_tiny_radius() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();

    let out = save_mapping(
        &mut tx, "NO-SUCH-OFFICE", None, Some("Orphan Lab"),
        BUILDING_LAT, BUILDING_LONG, Some(5.0), Some(true),
    ).await;

    assert_eq!(out.pointer("/data/employee_count").and_then(Value::as_i64), Some(0));
    let warnings = out.get("warnings").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(warnings.len(), 2, "expected both warnings, got {out}");
    let joined = warnings.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" | ");
    assert!(joined.contains("No employee has office"), "got {joined}");
    assert!(joined.contains("below 20m"), "got {joined}");
}

#[tokio::test]
async fn save_rejects_bad_input() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.unwrap();
    let (_emp_id, office) = borrow_employee(&mut tx).await;

    // Neither building_id nor building_name.
    let out = save_mapping(&mut tx, &office, None, None, BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(true)).await;
    assert!(message(&out).contains("building_id"), "got {out}");

    // Unknown building_id.
    let out = save_mapping(&mut tx, &office, Some(-1), None, BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(true)).await;
    assert!(message(&out).contains("not found"), "got {out}");

    // Out-of-range coordinates.
    let out = save_mapping(&mut tx, &office, None, Some("X"), 999.0, 90.0, Some(50.0), Some(true)).await;
    assert_eq!(message(&out), "Invalid building coordinates", "got {out}");

    // Non-positive radius.
    let out = save_mapping(&mut tx, &office, None, Some("X"), BUILDING_LAT, BUILDING_LONG, Some(0.0), Some(true)).await;
    assert!(message(&out).contains("radius"), "got {out}");

    // Empty body_code.
    let out = save_mapping(&mut tx, "  ", None, Some("X"), BUILDING_LAT, BUILDING_LONG, Some(50.0), Some(true)).await;
    assert!(message(&out).contains("body_code"), "got {out}");
}