//! duerp-attendance — standalone WOW face-attendance service.
//!
//! Split out of `duerp-api`, which kept the rest of the ERP. The URL layout is
//! deliberately IDENTICAL to what duerp-api served (`/login`, `/uploads/**`,
//! `/ext-api/wow-attendance/**`), so existing clients only change host:port —
//! no path rewrites. Point the reverse proxy at this process for those paths
//! and everything else stays with duerp-api.
//!
//! The database is shared: both services talk to the same Postgres `ictcell`
//! schema, so nothing had to be migrated. See `docs/ARCHITECTURE.md`.

mod middleware;
mod models;
mod routes;
mod utils;

use actix_files::Files;
use actix_web::{web, App, HttpResponse, HttpServer};
use dotenvy::dotenv;
use utils::db;

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
    dotenv().ok();
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
            // Step logs live at `<uploads>/log` so they ride the same mounted
            // volume as the images. That puts them inside the folder the
            // `/uploads` static route serves, so this MUST be registered BEFORE
            // that route to shadow it: the logs carry tokens, client IPs and
            // employee ids and must never be reachable over HTTP.
            .service(
                // Both forms: the bare path (which the static server would
                // otherwise 301-redirect to the listing, revealing the folder)
                // and everything beneath it.
                web::scope("/uploads/log").default_service(
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
                    .service(routes::wow_attendance::wow_mapping_save)   // POST /ext-api/wow-attendance/mapping-save (admin; json: body_code, building_id|building_name, lat, long, radius)
            )
    })
    .bind((bind_addr, port))?
    .run()
    .await
}