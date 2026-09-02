// ============================================================
// POST /ext-api/wow-attendance/verify
// Procedure: wow_attendance_location_verification
// ============================================================

use actix_web::{web, HttpResponse};
use serde::Deserialize;
use serde_json::Value;
use sqlx::MySqlPool;

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub emp_id:      String,
    pub device_lat:  f64,
    pub device_long: f64,
}

pub async fn wow_attendance_verify(
    pool: web::Data<MySqlPool>,
    body: web::Json<VerifyRequest>,
) -> HttpResponse {

    let result = sqlx::query_scalar::<_, Value>(
        "CALL wow_attendance_location_verification(?, ?, ?)"
    )
    .bind(&body.emp_id)
    .bind(body.device_lat)
    .bind(body.device_long)
    .fetch_one(pool.get_ref())
    .await;

    match result {
        Ok(val) => {
            let verified = val
                .get("verified")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if verified {
                HttpResponse::Ok().json(val)
            } else {
                HttpResponse::Forbidden().json(val)
            }
        }
        Err(e) => {
            eprintln!("[wow_attendance_verify] DB error: {e}");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "verified": false,
                "error":    "Internal server error"
            }))
        }
    }
}

// Route registration in main.rs:
//
// cfg.service(
//     web::resource("/ext-api/wow-attendance/verify")
//         .route(web::post().to(wow_attendance_verify))
// );
