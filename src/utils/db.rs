use sqlx::{postgres::PgPoolOptions, PgPool};
use std::env;

/// Shared with duerp-api: same Postgres instance, and mostly the same `ictcell`
/// schema. The split is at the process boundary, not the data boundary — see
/// `docs/ARCHITECTURE.md` for why the tables were not moved.
///
/// The exception is this service's own ext-api gate, which now lives in the
/// `attendance` schema: `attendance.ext_api_allowed_ips` (read by
/// `ext_auth_middleware`) and `attendance.ext_api_call_logs` (written by
/// `api_logger`), alongside the NFC card tables. duerp-api keeps using the
/// `ictcell` pair for its own endpoints and the two no longer track each other
/// — see `sql/005_ext_api_attendance_schema.sql`.
///
/// The MySQL helper duerp-api carries is not ported: no attendance code path
/// ever used it, so the `mysql` sqlx feature is off in this crate.
pub async fn get_db_pool() -> PgPool {
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL not set");
    let max_connections: u32 = env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(10);

    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(&database_url)
        .await
        .expect("Failed to create PostgreSQL DB pool")
}