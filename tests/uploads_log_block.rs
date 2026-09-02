//! The step logs live under `<uploads>/log`, inside the folder the `/uploads`
//! static route serves. They carry tokens, client IPs and employee ids, so they
//! must never be reachable over HTTP. This test reproduces the exact service
//! wiring from main.rs — the block scope registered BEFORE the Files service —
//! and asserts the logs are shadowed while ordinary uploads still serve.

use actix_files::Files;
use actix_web::{test, web, App, HttpResponse};
use std::io::Write;

// Mirror of the two services in main.rs, in the same registration order.
macro_rules! build_app {
    ($serve_dir:expr) => {
        App::new()
            .service(
                web::scope("/uploads/log").default_service(
                    web::route().to(|| async { HttpResponse::NotFound().finish() }),
                ),
            )
            .service(
                Files::new("/uploads", $serve_dir)
                    .show_files_listing()
                    .disable_content_disposition(),
            )
    };
}

#[actix_web::test]
async fn logs_are_not_reachable_but_uploads_are() {
    // A temp uploads dir with a normal image and a log file underneath log/.
    let dir = std::env::temp_dir().join(format!("uploads_block_{}", std::process::id()));
    let log_dir = dir.join("log");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::File::create(dir.join("face.jpg"))
        .unwrap()
        .write_all(b"not-really-a-jpeg")
        .unwrap();
    std::fs::File::create(log_dir.join("2020_secret.log"))
        .unwrap()
        .write_all(b"token user id=45320 client_ip=1.2.3.4")
        .unwrap();

    let app = test::init_service(build_app!(dir.clone())).await;

    // A real upload still serves — the block must not break the static route.
    let resp = test::call_service(&app, test::TestRequest::get().uri("/uploads/face.jpg").to_request()).await;
    assert!(resp.status().is_success(), "normal upload should serve, got {}", resp.status());

    // The individual log file is not readable.
    let resp = test::call_service(&app, test::TestRequest::get().uri("/uploads/log/2020_secret.log").to_request()).await;
    assert_eq!(resp.status().as_u16(), 404, "log file must be blocked");

    // The listing under log/ is not browsable.
    let resp = test::call_service(&app, test::TestRequest::get().uri("/uploads/log/").to_request()).await;
    assert_eq!(resp.status().as_u16(), 404, "log listing (trailing slash) must be blocked");

    // The bare folder path must not redirect to a listing either.
    let resp = test::call_service(&app, test::TestRequest::get().uri("/uploads/log").to_request()).await;
    assert_eq!(resp.status().as_u16(), 404, "bare log path must be blocked, not redirected (got {})", resp.status());

    std::fs::remove_dir_all(&dir).ok();
}