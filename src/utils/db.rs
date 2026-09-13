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
    // Deliberately not `.expect()`. This is the single most common startup
    // failure, and a panic answers it with "run with RUST_BACKTRACE=1" — a
    // backtrace through `main` that cannot possibly say where the value was
    // meant to come from. Under `Restart=`, that unhelpful message is then
    // printed on a loop. The text below is what actually resolves it.
    let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| {
        eprintln!(
            "FATAL: DATABASE_URL is not set.\n\
             \n\
             It is read from the process environment, which is populated either by a\n\
             `.env` found from the working directory, or by systemd `EnvironmentFile=`\n\
             / docker `--env-file`. The `config:` line printed just above says which\n\
             of those was found — if it reports no .env and the unit has no\n\
             EnvironmentFile, that is the fault.\n\
             \n\
             `.env` is gitignored, so a git-based deploy does NOT carry it onto a new\n\
             host. Copy `.env.example` to `.env` there and fill it in, or point the\n\
             service at one with ENV_FILE=/absolute/path/to/.env."
        );
        std::process::exit(1);
    });
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