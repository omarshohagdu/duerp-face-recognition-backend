use sqlx::{postgres::PgPoolOptions, PgPool};
use std::env;

/// Shared with duerp-api: same Postgres instance, same `ictcell` schema. The
/// split is at the process boundary, not the data boundary — see
/// `docs/ARCHITECTURE.md` for why the tables were not moved.
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