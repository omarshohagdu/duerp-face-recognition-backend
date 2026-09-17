//! duerp-attendance — standalone WOW face-attendance service.
//!
//! Split out of `duerp-api`, which kept the rest of the ERP. The URL layout is
//! deliberately IDENTICAL to what duerp-api served (`/login`, `/uploads/**`,
//! `/ext-api/wow-attendance/**`), so existing clients only change host:port —
//! no path rewrites. Point the reverse proxy at this process for those paths
//! and everything else stays with duerp-api.
//!
//! The database is shared: both services talk to the same Postgres `ictcell`
//! schema, so nothing had to be migrated. The one exception is this service's
//! own ext-api gate and NFC tables, which live in `attendance` — see
//! `sql/005_ext_api_attendance_schema.sql` and `docs/ARCHITECTURE.md`.

mod middleware;
mod models;
mod routes;
mod utils;

use actix_files::Files;
use actix_web::{web, App, HttpResponse, HttpServer};
use dotenvy::dotenv;
use utils::db;

/// Load `.env`, and SAY WHAT HAPPENED.
///
/// `dotenvy::dotenv()` searches upward from the CURRENT WORKING DIRECTORY. It
/// therefore finds nothing whenever the service is started from anywhere but
/// the crate root — which under systemd is the default unless the unit sets
/// `WorkingDirectory`, and is always the case under docker.
///
/// This used to be `dotenv().ok()`, which threw that failure away. The first
/// symptom was `get_db_pool` panicking with `DATABASE_URL not set: NotPresent`
/// on a restart loop — a message that names neither the file that was missing
/// nor the directory that was searched, and so sends you looking at the
/// database instead of at the working directory. One line here is what that
/// cost.
///
/// `ENV_FILE` takes an absolute path and skips the search entirely. That is
/// the reliable way to run this from an arbitrary working directory; it is
/// also the fastest fix when this has already gone wrong in production.
///
/// Not finding a file is NOT fatal: `EnvironmentFile=` in a systemd unit and
/// `--env-file` in docker both populate the real process environment, and in
/// those deployments there is correctly no `.env` to find. Only a genuinely
/// missing variable is fatal, and that is reported where it is read.
fn load_env() {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());

    match std::env::var("ENV_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(path) => match dotenvy::from_path(&path) {
            Ok(()) => println!("config: loaded {path} (ENV_FILE)"),
            Err(e) => eprintln!(
                "config: ENV_FILE={path} could not be read ({e}) — \
                 continuing with the process environment only"
            ),
        },
        None => match dotenv() {
            Ok(path) => println!("config: loaded {}", path.display()),
            Err(_) => eprintln!(
                "config: no .env found searching up from {cwd} — continuing with the \
                 process environment only. That is correct under systemd \
                 (EnvironmentFile=) or docker (--env-file); if neither is in play, \
                 set ENV_FILE=/absolute/path/to/.env or give the unit a \
                 WorkingDirectory."
            ),
        },
    }
}

/// Liveness probe. Standalone services sit behind a proxy / systemd unit that
/// needs a cheap non-authenticated endpoint to poll; the ERP monolith never had
/// one because it was checked through its UI.
async fn health() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status":  "ok",
        "service": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    load_env();
    env_logger::init();

    let db_pool = db::get_db_pool().await;

    // Own port so this can run alongside duerp-api (8080) on one host.
    let bind_addr = std::env::var("ATTENDANCE_BIND")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let port: u16 = std::env::var("ATTENDANCE_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8083);

    println!("duerp-attendance starting at http://{bind_addr}:{port}");

    HttpServer::new(move || {
        // Directory the `/uploads` URL serves from. Absolute on the server so it
        // does not depend on the process working directory (the default
        // `./uploads` only works when launched from the crate root). This is the
        // PARENT of WOW_UPLOAD_DIR — the `/uploads` prefix supplies the rest —
        // so the two must resolve to the same `uploads` folder.
        //
        // That folder is THIS crate's own `uploads/`, not duerp-api's: face
        // captures belong to attendance and nothing in duerp-api reads them.
        // So `/uploads/wow_attendance/*` resolves here and everything else under
        // `/uploads` (lectures, course materials, …) resolves on duerp-api —
        // the proxy splits them by prefix; see docs/DEPLOYMENT.md.
        let uploads_serve_dir = std::env::var("WOW_UPLOADS_SERVE_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "./uploads".to_string());

        App::new()
            .app_data(web::Data::new(db_pool.clone()))
            .wrap(actix_cors::Cors::permissive())
            .route("/health", web::get().to(health))
            // Step logs live at `<uploads>/log` (attendance calls) and
            // `<uploads>/login` (sign-ins) so they ride the same mounted volume
            // as the images. That puts them inside the folder the `/uploads`
            // static route serves, so these MUST be registered BEFORE that
            // route to shadow it: the logs carry tokens, client IPs, usernames
            // and employee ids and must never be reachable over HTTP.
            //
            // Both forms are covered for each: the bare path (which the static
            // server would otherwise 301-redirect to the listing, revealing the
            // folder) and everything beneath it.
            .service(
                web::scope("/uploads/log").default_service(
                    web::route().to(|| async { HttpResponse::NotFound().finish() }),
                ),
            )
            .service(
                web::scope("/uploads/login").default_service(
                    web::route().to(|| async { HttpResponse::NotFound().finish() }),
                ),
            )
            .service(
                Files::new("/uploads", uploads_serve_dir)
                    .show_files_listing()
                    .disable_content_disposition(),
            )
            .service(routes::auth::login)
            .service(
                web::scope("/ext-api")
                    .wrap(middleware::api_logger::ApiLogger)
                    .wrap(middleware::ext_auth_middleware::ExtAuthMiddleware)
                    .service(routes::wow_attendance::wow_enroll)         // POST /ext-api/wow-attendance/enroll?id=&id_type=
                    .service(routes::wow_attendance::wow_enrolled_list)  // POST /ext-api/wow-attendance/enrolled?id_type=
                    .service(routes::wow_attendance::wow_check_enrolled) // POST /ext-api/wow-attendance/check?person_id=
                    .service(routes::wow_attendance::wow_records_by_date)   // POST /ext-api/wow-attendance/reports/by-date?from_date=&to_date=
                    .service(routes::wow_attendance::wow_records_by_person) // POST /ext-api/wow-attendance/reports/by-person?person_id=&from_date=&to_date=
                    .service(routes::wow_attendance::wow_ssl_image_verify)  // POST /ext-api/wow-attendance/ssl_image_verfiy (images: multiple file)
                    .service(routes::wow_attendance::wow_verify)         // POST /ext-api/wow-attendance/verify?id=&id_type=
                    .service(routes::wow_attendance::wow_mapping_save)   // POST /ext-api/wow-attendance/mapping-save (json: body_code, building_id|building_name, lat, long, radius)
                    // Readers for the two step-log folders. They sit here
                    // rather than on their own scope so they inherit the
                    // app-credential + IP allow-list gate every other ext-api
                    // call gets; on top of that each takes a bearer token.
                    // The `/uploads/log` and `/uploads/login` 404 blocks above
                    // stay — these serve JSON, never the files.
                    .service(routes::logs::wow_login_logs)      // POST /ext-api/wow-attendance/logs/login?file=&person_id=&from_date=&to_date=&page=&limit=
                    .service(routes::logs::wow_attendance_logs) // POST /ext-api/wow-attendance/logs/attendance?file=&person_id=&from_date=&to_date=&page=&limit=
                    // NFC card <-> student mapping. Same gate as everything
                    // else in this scope (app credentials + IP allow-list) plus
                    // a bearer token — see src/routes/nfc_card.rs.
                    // Each path needs its own `ext_api_allowed_ips` row, added
                    // by sql/004_nfc_card.sql, or every call is a 403. All three
                    // rows hold the `'*'` wildcard: reachable from any IP, with
                    // the app credentials and bearer token as the only gate.
                    .service(routes::nfc_card::nfc_get_card_info)  // GET|POST /ext-api/nfc-card/get_card_info?card_number= (or in the body)
                    .service(routes::nfc_card::nfc_save_card_info) // POST /ext-api/nfc-card/save_card_info (multipart: student_applicant_id, card_number, is_verified, registration_type, force_reassign, card_image, student_selfie)
                    .service(routes::nfc_card::nfc_checking_card_reg_status) // GET|POST /ext-api/nfc-card/checking_card_reg_status?registration_no= (or in the body)
                    // What the caller may do, and what their client should
                    // render. Open to any authenticated caller — its rule row
                    // carries no permission, because a client cannot draw a
                    // screen without it. The payload is rendering hints only;
                    // ExtAuthMiddleware is what refuses. See docs/access_control.md.
                    .service(routes::access::me_access) // GET|POST /ext-api/me/access?platform=desktop|mobile|kiosk
                    // Role administration — the Access Roles screen. These
                    // ENFORCE their permission unconditionally, unlike the
                    // audit-mode gate: they are new (nobody to break) and they
                    // hand out permissions, so serving them in audit mode would
                    // let any token holder make themselves an admin. See
                    // src/routes/access_admin.rs and docs/access_control.md.
                    .service(routes::access_admin::roles_list)     // POST /ext-api/access/roles
                    .service(routes::access_admin::resources_list) // POST /ext-api/access/resources
                    .service(routes::access_admin::role_save)      // POST /ext-api/access/role-save     (json: key, name, permissions[])
                    .service(routes::access_admin::role_delete)    // POST /ext-api/access/role-delete   (json: key)
                    .service(routes::access_admin::users_list)     // POST /ext-api/access/users?search=&limit=&offset=
                    .service(routes::access_admin::user_role)      // POST /ext-api/access/user-role     (json: person_id, role|null)
                    .service(routes::access_admin::user_override)  // POST /ext-api/access/user-override (json: person_id, permission, effect)
            )
    })
    .bind((bind_addr, port))?
    .run()
    .await
}