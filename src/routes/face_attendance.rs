//! Selfie enrollment and selfie attendance, verified by the face-verify service.
//!
//!   POST /ext-api/face-attendance/enroll   multipart: employee_id, employee_selfie_image, device_info?
//!   POST /ext-api/face-attendance/mark     multipart: employee_id, selfie_image, device_info (GPS)
//!
//! Others Selfie Attendance (a colleague acting for an employee with no phone):
//!
//!   POST /ext-api/face-attendance/others/enroll  multipart: employee_id, employee_selfie_image, enrolled_by, device_info?
//!   POST /ext-api/face-attendance/others/mark    multipart: employee_id, selfie_image, enrolled_by, device_info (GPS)
//!   POST /ext-api/face-attendance/others/list    enrolled_by? (query or JSON)
//!
//! A second path next to `/wow-attendance/enroll` + `/verify`. Instead of the AI
//! platform learning a face and identifying people 1:N, this one stores ONE
//! selfie per employee and, at check-in, asks the face-verify service at
//! `NFC_FACE_VERIFY_URL` whether the new selfie is the same person (1:1). The
//! service is the one the NFC card desk already uses: multipart `card_image` +
//! `selfie_image`, answering `match`, `similarity`, `threshold`.
//!
//! ENROLL checks the image with that same service before saving it: the selfie
//! is sent as both images, so a photo with no clear, single face is refused by
//! the service (`NO_FACE_DETECTED`) instead of becoming an enrollment nothing
//! can ever match.
//!
//! MARK requires, in order: the token holder's own employee id, an active
//! enrollment with an image on disk, GPS, a face match against that image,
//! and the device inside one of the employee's APPROVED + ACTIVE offices
//! (always the approved-office rule here, whatever `GEO_LOCATION_MODE` says).
//!
//! STORAGE is shared with the AI path (`wow_attendance_enrollments`, `_images`,
//! `_records`), so office selection, reports and the enrolled list see both;
//! `wow_attendance_records.verify_method = 'FACE_VERIFY'` tells them apart.
//!
//! Every response is `{success, code, message, data}`; branch on `code`.

use actix_multipart::Multipart;
use actix_web::http::StatusCode;
use actix_web::{post, web, HttpResponse};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::routes::nfc_card::{face_verify_api_key, face_verify_message, forward_part, FACE_VERIFY_CLIENT};
use crate::routes::wow_attendance::{
    browsable_path, device_accuracy, device_coords, du_identify, enrolled_dir, full_path,
    is_supported_image, live_dir, log_local_response, max_upload_bytes, max_upload_mb_display,
    public_base_url, record_token_mismatch, reduce_saved_image, require_bearer_token,
    require_token_caller,
};
use crate::utils::step_logger::{query_to_json, StepLogger};

/// Image field names accepted on both endpoints. The documented ones are
/// `employee_selfie_image` (enroll) and `selfie_image` (mark); the rest are
/// spellings clients have been seen to send. Compared lower-cased.
const IMAGE_FIELDS: [&str; 6] = [
    "employee_selfie_image",
    "employee_selife_image",
    "selfie_image",
    "image",
    "photo",
    "file",
];

fn reply(status: StatusCode, code: &str, message: impl Into<String>, data: Value) -> HttpResponse {
    HttpResponse::build(status).json(json!({
        "success": status.is_success(),
        "code":    code,
        "message": message.into(),
        "data":    data,
    }))
}

fn fail(status: StatusCode, code: &str, message: impl Into<String>) -> HttpResponse {
    reply(status, code, message, json!({}))
}

fn logger(route: &'static str, req: &actix_web::HttpRequest) -> StepLogger {
    let log = StepLogger::new(route);
    log.set_base_url(&public_base_url(req));
    log.set_endpoint(req.method().as_str(), &full_path(req));
    log.params("query", &query_to_json(req.query_string()));
    log
}

// ---------------------------------------------------------------------
// Multipart: `employee_id`, `device_info`, one image.
// ---------------------------------------------------------------------

struct Upload {
    employee_id: Option<String>,
    /// Others path only: the colleague acting for `employee_id`.
    enrolled_by: Option<String>,
    device_info: Option<String>,
    image: Option<String>,
}

async fn read_upload(
    log: &StepLogger,
    mut payload: Multipart,
    dir: &str,
    query_employee_id: Option<String>,
) -> Result<Upload, HttpResponse> {
    if !Path::new(dir).exists() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            log.step(format!("upload dir create FAILED: {e}"));
            return Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Could not store the image"));
        }
    }

    let mut up = Upload { employee_id: query_employee_id, enrolled_by: None, device_info: None, image: None };

    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| {
            log.step(format!("multipart error: {e}"));
            fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", format!("Multipart error: {e}"))
        })?;
        let cd = field.content_disposition();
        let name = cd.and_then(|c| c.get_name()).unwrap_or("").to_ascii_lowercase();
        let filename = cd.and_then(|c| c.get_filename()).unwrap_or("").to_string();

        if IMAGE_FIELDS.contains(&name.as_str()) && !filename.is_empty() {
            if up.image.is_some() {
                // Single image by contract. Drain and ignore the extras rather
                // than guessing which one the caller meant.
                while field.next().await.is_some() {}
                log.step(format!("extra image part '{name}' ignored — one image only"));
                continue;
            }
            let path = format!("{dir}{}.upload", Uuid::new_v4());
            let mut f = File::create(&path).await.map_err(|e| {
                log.step(format!("image file create FAILED: {e}"));
                fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Could not store the image")
            })?;
            let mut head: Vec<u8> = Vec::new();
            let mut written = 0usize;
            while let Some(chunk) = field.next().await {
                let bytes = chunk.map_err(|e| {
                    fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", format!("Image read error: {e}"))
                })?;
                if head.len() < 16 {
                    head.extend_from_slice(&bytes[..bytes.len().min(16 - head.len())]);
                }
                written += bytes.len();
                if written > max_upload_bytes() {
                    log.step(format!("image exceeds upload ceiling ({written} bytes)"));
                    return Err(fail(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "IMAGE_TOO_LARGE",
                        format!("The image must be at most {}", max_upload_mb_display()),
                    ));
                }
                f.write_all(&bytes).await.map_err(|e| {
                    log.step(format!("image write FAILED: {e}"));
                    fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Could not store the image")
                })?;
            }
            if !is_supported_image(&head) {
                log.step(format!("'{filename}' rejected — not an image"));
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    "INVALID_IMAGE",
                    "Only image files are allowed (JPEG, PNG, WEBP, BMP, HEIC/HEIF)",
                ));
            }
            // Name it by content type so the file browser and the face service
            // both see a sensible extension.
            let ext = match crate::routes::wow_attendance::detect_image_format(&head) {
                "PNG" => "png",
                "WEBP" => "webp",
                "BMP" => "bmp",
                "HEIC" => "heic",
                "HEIF" => "heif",
                _ => "jpg",
            };
            let final_path = path.replace(".upload", &format!(".{ext}"));
            if tokio::fs::rename(&path, &final_path).await.is_err() {
                up.image = Some(path);
            } else {
                up.image = Some(final_path);
            }
            log.step(format!("image '{filename}' saved ({written} bytes)"));
            continue;
        }

        // Text fields.
        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(b) => {
                    if data.len() + b.len() > 64 * 1024 {
                        return Err(fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", format!("Field '{name}' is too long")));
                    }
                    data.extend_from_slice(&b)
                }
                Err(e) => {
                    return Err(fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", format!("Field read error: {e}")))
                }
            }
        }
        let text = String::from_utf8_lossy(&data).trim().to_string();
        match name.as_str() {
            "employee_id" | "id" => {
                if !text.is_empty() {
                    up.employee_id = Some(text);
                }
            }
            "enrolled_by" | "marked_by" => {
                if !text.is_empty() {
                    up.enrolled_by = Some(text);
                }
            }
            "device_info" => {
                let mut p = serde_json::Map::new();
                p.insert("device_info".into(), Value::String(text.clone()));
                log.params("form", &Value::Object(p));
                up.device_info = Some(text);
            }
            _ => {}
        }
    }

    if let Some(path) = &up.image {
        log.file(path, browsable_path(path).as_deref());
    }
    Ok(up)
}

// ---------------------------------------------------------------------
// The face-verify service
// ---------------------------------------------------------------------

struct Verdict {
    matched: bool,
    similarity: Option<f64>,
    threshold: Option<f64>,
    detail: Option<String>,
}

enum VerifyError {
    /// The service read the request but could not compare — usually no face.
    NotComparable(String),
    /// No verdict at all: unset URL, unreachable, 5xx, rejected API key.
    Unavailable(String),
}

/// Ask the face-verify service whether `selfie` is the same person as `reference`.
async fn compare_faces(
    log: &StepLogger,
    db: &PgPool,
    reference: &str,
    selfie: &str,
) -> Result<Verdict, VerifyError> {
    // Same URL resolution as the NFC gate: the admin setting first, then
    // NFC_FACE_VERIFY_URL. The NFC ON/OFF switch is NOT consulted — attendance
    // without a face check is not a mode this path has.
    let Some(url) = crate::utils::settings::face_verify(db).await.url else {
        log.step("no face-verify URL configured (NFC_FACE_VERIFY_URL) — refusing");
        return Err(VerifyError::Unavailable("Face verification is not configured".into()));
    };

    let mut form = reqwest::multipart::Form::new();
    for (field, path) in [("card_image", reference), ("selfie_image", selfie)] {
        match forward_part(field, path).await {
            Ok(part) => form = form.part(field, part),
            Err(e) => {
                log.step(format!("{field} could not be read for verification: {e}"));
                return Err(VerifyError::Unavailable("Face verification could not read the images".into()));
            }
        }
    }

    log.step(format!("face-verify: POST {url}"));
    let resp = match FACE_VERIFY_CLIENT
        .post(&url)
        .header("X-API-Key", face_verify_api_key())
        .multipart(form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log.step(format!("face-verify request FAILED: {e}"));
            return Err(VerifyError::Unavailable("Face verification service is unavailable".into()));
        }
    };

    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    let body: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            log.ai_response("/verify", status.as_u16(), &json!({ "non_json_body": raw }));
            return Err(VerifyError::Unavailable("Face verification returned an unreadable response".into()));
        }
    };
    log.ai_response("/verify", status.as_u16(), &body);

    if matches!(status.as_u16(), 401 | 403) {
        return Err(VerifyError::Unavailable("Face verification credentials were rejected".into()));
    }
    if status.is_server_error() {
        return Err(VerifyError::Unavailable("Face verification service is unavailable".into()));
    }
    if status.is_client_error() {
        return Err(VerifyError::NotComparable(
            face_verify_message(&body).unwrap_or_else(|| "The photo could not be checked".into()),
        ));
    }

    let num = |k: &str| body.get(k).and_then(Value::as_f64);
    match body.get("match").and_then(Value::as_bool) {
        Some(matched) => {
            log.step(format!(
                "face-verify verdict: match={matched} similarity={:?} threshold={:?}",
                num("similarity"),
                num("threshold")
            ));
            Ok(Verdict {
                matched,
                similarity: num("similarity"),
                threshold: num("threshold"),
                detail: face_verify_message(&body),
            })
        }
        None => Err(VerifyError::Unavailable("Face verification returned an unexpected response".into())),
    }
}

fn verify_error_response(e: VerifyError) -> HttpResponse {
    match e {
        VerifyError::NotComparable(msg) => reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NO_FACE_DETECTED",
            "We couldn't find a clear face in the photo. Face the camera in good light and try again.",
            json!({ "detail": msg }),
        ),
        VerifyError::Unavailable(msg) => fail(StatusCode::SERVICE_UNAVAILABLE, "FACE_VERIFY_UNAVAILABLE", msg),
    }
}

// ---------------------------------------------------------------------
// Shared front half: token, employee id, ownership, employee record.
// ---------------------------------------------------------------------

struct Caller {
    token: String,
    employee_id: String,
}

/// `field` names the part that must hold the token holder's own id:
/// `employee_id` on the self path, `enrolled_by` on the others path.
async fn identify_caller(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    employee_id: Option<String>,
    field: &str,
    action: &str,
) -> Result<Caller, HttpResponse> {
    let token = require_bearer_token(req)?;
    let (token_user_id, line) = require_token_caller(req)?;
    log.step(line);

    let Some(employee_id) = employee_id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return Err(fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", format!("`{field}` is required")));
    };
    log.set_id(&employee_id);

    // Self only: the id must be the token holder's.
    let who = du_identify(&employee_id, token_user_id, log).await;
    if !who.owned {
        let message = format!("Person mismatch. {field}={employee_id} but Logged in User id={token_user_id}");
        log.step(&message);
        record_token_mismatch(db.get_ref(), action, &employee_id, token_user_id, None, None, log).await;
        return Err(reply(
            StatusCode::UNAUTHORIZED,
            "NOT_YOUR_ID",
            if field == "employee_id" {
                "You can only enroll or mark attendance for yourself.".to_string()
            } else {
                format!("`{field}` must be your own employee ID.")
            },
            json!({ field: employee_id, "logged_in_user_id": token_user_id }),
        ));
    }

    if !employee_exists(db.get_ref(), &employee_id).await {
        log.step(format!("{field} {employee_id} not found in attendance.employees"));
        return Err(fail(StatusCode::NOT_FOUND, "EMPLOYEE_NOT_FOUND", "No staff record was found for this employee ID."));
    }

    Ok(Caller { token, employee_id })
}

async fn employee_exists(db: &PgPool, employee_id: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM attendance.employees WHERE emp_id = $1)")
        .bind(employee_id)
        .fetch_one(db)
        .await
        .unwrap_or(false)
}

/// Others path: the employee being enrolled / marked. Must be given, must not
/// be the caller (that is the self path), and must be staff.
async fn resolve_subject(
    log: &StepLogger,
    db: &PgPool,
    employee_id: Option<String>,
    caller: &Caller,
    self_code: &str,
) -> Result<String, HttpResponse> {
    let Some(subject) = employee_id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return Err(fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", "`employee_id` is required"));
    };
    if subject == caller.employee_id {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            self_code,
            "That is your own employee ID. Use Selfie attendance for yourself.",
        ));
    }
    if !employee_exists(db, &subject).await {
        log.step(format!("employee {subject} not found in attendance.employees"));
        return Err(fail(StatusCode::NOT_FOUND, "EMPLOYEE_NOT_FOUND", "No staff record was found for this employee ID."));
    }
    log.step(format!("acting for employee {subject} (by {})", caller.employee_id));
    log.set_id(&subject);
    Ok(subject)
}

/// Enroll's image check: the selfie against itself. The service refuses a
/// photo with no clear face (4xx), which is exactly what must not be enrolled.
async fn check_enroll_image(log: &StepLogger, db: &PgPool, image: &str) -> Result<Verdict, HttpResponse> {
    log.step("checking the selfie with the face-verify service (selfie vs itself)");
    let check = compare_faces(log, db, image, image).await.map_err(verify_error_response)?;
    if !check.matched {
        return Err(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "LOW_IMAGE_QUALITY",
            "This photo isn't clear enough to use. Retake it facing the camera in good light.",
            json!({ "similarity": check.similarity, "detail": check.detail }),
        ));
    }
    Ok(check)
}

fn device_object(raw: Option<&str>) -> Value {
    raw.and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

// ---------------------------------------------------------------------
// POST /ext-api/face-attendance/enroll
// ---------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct EmployeeQuery {
    pub employee_id: Option<String>,
}

#[post("/face-attendance/enroll")]
pub async fn face_enroll(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<EmployeeQuery>,
    payload: Multipart,
) -> HttpResponse {
    let log = logger("ext-api/face-attendance/enroll", &req);
    let resp = face_enroll_inner(&log, &req, &db, query.into_inner(), payload).await;
    log_local_response(&log, resp).await
}

async fn face_enroll_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    query: EmployeeQuery,
    payload: Multipart,
) -> HttpResponse {
    log.step("request received");
    // Reject a bad token before accepting the upload.
    if let Err(resp) = require_token_caller(req) {
        return resp;
    }
    let up = match read_upload(log, payload, &enrolled_dir(), query.employee_id).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let caller = match identify_caller(log, req, db, up.employee_id.clone(), "employee_id", "FaceVerifyEnroll").await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(image) = up.image else {
        return fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", "`employee_selfie_image` is required (one image)");
    };

    let image = reduce_saved_image(&image).await;

    let check = match check_enroll_image(log, db.get_ref(), &image).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let mut device = device_object(up.device_info.as_deref());
    device["enroll_method"] = json!("FACE_VERIFY");

    log.step("persisting enrollment via attendance.wow_attendance_enroll");
    let result = sqlx::query_scalar::<_, Value>("SELECT attendance.wow_attendance_enroll($1, 'Employee', $2, $3, $4)")
        .bind(&caller.employee_id)
        .bind(&caller.token)
        .bind(&device)
        .bind(vec![image.clone()])
        .fetch_one(db.get_ref())
        .await;

    match result {
        Ok(body) if body.get("success").and_then(Value::as_bool) == Some(true) => {
            let d = body.get("data").cloned().unwrap_or(Value::Null);
            let reenroll = d.get("is_reenrollment").and_then(Value::as_bool).unwrap_or(false);
            reply(
                StatusCode::CREATED,
                "OK",
                if reenroll {
                    "Selfie updated. Your new photo will be used for attendance."
                } else {
                    "Enrolled. Choose your office to start marking attendance."
                },
                json!({
                    "employee_id":            caller.employee_id,
                    "enrollment_id":          d.get("enrollment_id"),
                    "version":                d.get("version"),
                    "is_reenrollment":        reenroll,
                    "previous_enrollment_id": d.get("previous_enrollment_id"),
                    "image_check":            { "passed": true, "similarity": check.similarity },
                    "next_step":              "SELECT_OFFICE",
                }),
            )
        }
        Ok(body) => {
            log.step(format!("enroll function refused: {body}"));
            fail(
                StatusCode::BAD_REQUEST,
                "ENROLL_FAILED",
                body.get("message").and_then(Value::as_str).unwrap_or("Enrollment failed").to_string(),
            )
        }
        Err(e) => {
            eprintln!("DB error in face_enroll: {e}");
            log.step(format!("DB enroll FAILED: {e}"));
            fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error")
        }
    }
}

// ---------------------------------------------------------------------
// POST /ext-api/face-attendance/mark
// ---------------------------------------------------------------------

#[post("/face-attendance/mark")]
pub async fn face_mark(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<EmployeeQuery>,
    payload: Multipart,
) -> HttpResponse {
    let log = logger("ext-api/face-attendance/mark", &req);
    let resp = face_mark_inner(&log, &req, &db, query.into_inner(), payload).await;
    log_local_response(&log, resp).await
}

/// The newest image of the employee's active enrollment that is still on disk.
/// Older rows can name files from a previous deploy, so existence is checked.
async fn enrolled_reference(db: &PgPool, employee_id: &str) -> Result<Option<Option<String>>, sqlx::Error> {
    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT i.image_path
           FROM attendance.wow_attendance_enrollments e
           JOIN attendance.wow_attendance_images i ON i.enrollment_id = e.id
          WHERE e.person_id = $1 AND e.id_type = 'Employee' AND e.is_active
          ORDER BY i.created_at DESC",
    )
    .bind(employee_id)
    .fetch_all(db)
    .await?;
    if paths.is_empty() {
        let enrolled: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM attendance.wow_attendance_enrollments
                             WHERE person_id = $1 AND id_type = 'Employee' AND is_active)",
        )
        .bind(employee_id)
        .fetch_one(db)
        .await?;
        return Ok(if enrolled { Some(None) } else { None });
    }
    for p in paths {
        if tokio::fs::metadata(&p).await.is_ok() {
            return Ok(Some(Some(p)));
        }
    }
    Ok(Some(None))
}

async fn log_face_failure(db: &PgPool, employee_id: &str, code: &str, lat: f64, long: f64, acc: Option<f64>) {
    let _ = sqlx::query(
        "INSERT INTO attendance.geo_attendance_attempt_log
                (employee_id, failure_code, latitude, longitude, accuracy_meters)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(employee_id)
    .bind(code)
    .bind(lat)
    .bind(long)
    .bind(acc)
    .execute(db)
    .await;
}

async fn face_mark_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    query: EmployeeQuery,
    payload: Multipart,
) -> HttpResponse {
    log.step("request received");
    if let Err(resp) = require_token_caller(req) {
        return resp;
    }
    let up = match read_upload(log, payload, &live_dir(), query.employee_id).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let caller = match identify_caller(log, req, db, up.employee_id.clone(), "employee_id", "FaceVerifyMark").await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let Some(selfie) = up.image else {
        return fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", "`selfie_image` is required (one image)");
    };

    mark_core(log, db, &caller, &caller.employee_id, None, selfie, up.device_info.as_deref()).await
}

/// The back half of a check-in, shared by the self path (`marker` None) and the
/// others path (`marker` = the colleague holding the phone): GPS, enrolled
/// reference, face match, approved-office location, record.
async fn mark_core(
    log: &StepLogger,
    db: &web::Data<PgPool>,
    caller: &Caller,
    subject: &str,
    marker: Option<&str>,
    selfie: String,
    device_info: Option<&str>,
) -> HttpResponse {
    let others = marker.is_some();
    let mut device_json: Option<Value> = device_info.and_then(|s| serde_json::from_str(s).ok());
    let Some((lat, long)) = device_coords(device_json.as_ref()) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "LOCATION_REQUIRED",
            "Your location is required: send device_info with latitude and longitude",
        );
    };
    let accuracy = device_accuracy(device_json.as_ref());

    // Reference image.
    let reference = match enrolled_reference(db.get_ref(), subject).await {
        Ok(None) => {
            return fail(
                StatusCode::NOT_FOUND,
                "NOT_ENROLLED",
                if others { "This employee isn't enrolled yet. Enroll their selfie first." }
                else { "You haven't enrolled your selfie yet. Enroll first." },
            )
        }
        Ok(Some(None)) => {
            log.step("active enrollment has no image on disk");
            return fail(
                StatusCode::CONFLICT,
                "ENROLLED_IMAGE_MISSING",
                if others { "This employee's enrolled photo is no longer available. Enroll them again." }
                else { "Your enrolled photo is no longer available. Enroll your selfie again." },
            );
        }
        Ok(Some(Some(p))) => p,
        Err(e) => {
            eprintln!("DB error in face_mark (enrollment): {e}");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error");
        }
    };
    log.step(format!("reference image: {reference}"));

    let selfie = reduce_saved_image(&selfie).await;

    // 1 · Face.
    let verdict = match compare_faces(log, db.get_ref(), &reference, &selfie).await {
        Ok(v) => v,
        Err(e) => return verify_error_response(e),
    };
    let face = json!({
        "matched":    verdict.matched,
        "similarity": verdict.similarity,
        "threshold":  verdict.threshold,
    });
    if !verdict.matched {
        log_face_failure(db.get_ref(), subject, "FACE_NOT_MATCHED", lat, long, accuracy).await;
        return reply(
            StatusCode::FORBIDDEN,
            "FACE_NOT_MATCHED",
            if others { "The selfie doesn't match this employee's enrolled photo. Try again facing the camera in good light." }
            else { "Your selfie doesn't match your enrolled photo. Try again facing the camera in good light." },
            json!({ "face_match": face, "detail": verdict.detail }),
        );
    }

    // 2 · Location: approved offices only on this path.
    let geo_owner = match marker {
        None => subject.to_string(),
        Some(m) => match location_owner(db.get_ref(), subject, m).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("DB error in face_mark (location owner): {e}");
                return fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error");
            }
        },
    };
    if marker.is_some() {
        log.step(format!("location checked against {geo_owner}'s approved offices"));
    }
    let loc = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.employee_geo_location_verify($1, $2, $3, $4, 'APPROVED_OFFICE')",
    )
    .bind(&geo_owner)
    .bind(lat)
    .bind(long)
    .bind(accuracy)
    .fetch_one(db.get_ref())
    .await;
    let loc = match loc {
        Ok(v) => v,
        Err(e) => {
            eprintln!("DB error in face_mark (location): {e}");
            log.step(format!("location check FAILED: {e}"));
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error");
        }
    };
    let mut location = loc.get("data").cloned().unwrap_or(Value::Null);
    if marker.is_some() && location.is_object() {
        location["checked_against"] = json!(if geo_owner == subject { "EMPLOYEE" } else { "ENROLLER" });
    }
    if loc.get("verified").and_then(Value::as_bool) != Some(true) {
        let code = loc.get("code").and_then(Value::as_str).unwrap_or("OUT_OF_LOCATION").to_string();
        let message = loc.get("message").and_then(Value::as_str).unwrap_or("Location not verified").to_string();
        log.step(format!("location refused ({code})"));
        return reply(StatusCode::FORBIDDEN, &code, message, json!({ "face_match": face, "geo_match": location }));
    }

    // 3 · Record.
    if let (Some(m), Some(Value::Object(d))) = (marker, device_json.as_mut()) {
        d.insert("marked_by".into(), json!(m));
    }
    let confidence = verdict.similarity.unwrap_or(1.0).clamp(0.0, 1.0);
    let rec = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.wow_attendance_verify($1, 'Employee', $2, $3, $4, true, $5)",
    )
    .bind(subject)
    .bind(&caller.token)
    .bind(device_json.clone())
    .bind(&selfie)
    .bind(confidence)
    .fetch_one(db.get_ref())
    .await;
    let rec = match rec {
        Ok(v) if v.get("success").and_then(Value::as_bool) == Some(true) => v,
        Ok(v) => {
            log.step(format!("record refused: {v}"));
            return fail(StatusCode::NOT_FOUND, "NOT_ENROLLED", "You haven't enrolled your selfie yet. Enroll first.");
        }
        Err(e) => {
            eprintln!("DB error in face_mark (record): {e}");
            log.step(format!("record FAILED: {e}"));
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error");
        }
    };
    let data = rec.get("data").cloned().unwrap_or(Value::Null);
    if let Some(id) = data.get("attendance_id").and_then(Value::as_str).and_then(|s| Uuid::parse_str(s).ok()) {
        let stamped = sqlx::query(
            "SELECT attendance.wow_attendance_record_location($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(lat)
        .bind(long)
        .bind(&location)
        .execute(db.get_ref())
        .await;
        let method = sqlx::query(
            "UPDATE attendance.wow_attendance_records
                SET verify_method = 'FACE_VERIFY', face_similarity = $2
              WHERE id = $1",
        )
        .bind(id)
        .bind(verdict.similarity)
        .execute(db.get_ref())
        .await;
        // Separate so the self path never depends on sql/012's column.
        let by = match marker {
            Some(m) => sqlx::query("UPDATE attendance.wow_attendance_records SET marked_by = $2 WHERE id = $1")
                .bind(id)
                .bind(m)
                .execute(db.get_ref())
                .await
                .map(|_| ()),
            None => Ok(()),
        };
        if let Err(e) = stamped.map(|_| ()).and(method.map(|_| ())).and(by) {
            // The attendance IS recorded; failing now would invite a duplicate retry.
            eprintln!("record stamp failed for {id}: {e}");
            log.step(format!("record stamp FAILED (attendance kept): {e}"));
        }
    }
    log.step("attendance recorded");

    reply(
        StatusCode::OK,
        "OK",
        "Attendance marked",
        json!({
            "employee_id":   subject,
            "marked_by":     marker,
            "attendance_id": data.get("attendance_id"),
            "marked_at":     data.get("matched_at"),
            "face_match":    face,
            "geo_match":     location,
        }),
    )
}

// ---------------------------------------------------------------------
// Others Selfie Attendance — one employee acting for another who has no phone.
//
//   POST /ext-api/face-attendance/others/enroll  multipart: employee_id, employee_selfie_image, enrolled_by, device_info?
//   POST /ext-api/face-attendance/others/mark    multipart: employee_id, selfie_image, enrolled_by, device_info (GPS)
//   POST /ext-api/face-attendance/others/list    query/json: enrolled_by?
//
// `enrolled_by` is ALWAYS the token holder (checked exactly like `employee_id`
// on the self path); `employee_id` is the colleague being enrolled / marked.
// The face is still checked 1:1 against the employee's enrolled selfie, so the
// employee has to be in front of the camera. sql/012_face_attendance_others.sql.
// ---------------------------------------------------------------------

/// Whose approved offices an others check-in is measured against: the
/// employee's own when they have any approved office (an admin can assign one
/// with /geo-location/requests/assign), otherwise the enroller's — the person
/// without a phone usually could not select an office, and the enroller is
/// standing next to them.
async fn location_owner(db: &PgPool, subject: &str, marker: &str) -> Result<String, sqlx::Error> {
    let has: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM attendance.employee_geo_location
                         WHERE employee_id = $1 AND status = 'APPROVED')",
    )
    .bind(subject)
    .fetch_one(db)
    .await?;
    Ok(if has { subject.to_string() } else { marker.to_string() })
}

#[derive(serde::Deserialize)]
pub struct OthersQuery {
    pub employee_id: Option<String>,
    pub enrolled_by: Option<String>,
}

#[post("/face-attendance/others/enroll")]
pub async fn face_others_enroll(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<OthersQuery>,
    payload: Multipart,
) -> HttpResponse {
    let log = logger("ext-api/face-attendance/others/enroll", &req);
    let resp = face_others_enroll_inner(&log, &req, &db, query.into_inner(), payload).await;
    log_local_response(&log, resp).await
}

async fn face_others_enroll_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    query: OthersQuery,
    payload: Multipart,
) -> HttpResponse {
    log.step("request received (others)");
    if let Err(resp) = require_token_caller(req) {
        return resp;
    }
    let up = match read_upload(log, payload, &enrolled_dir(), query.employee_id).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let enroller = up.enrolled_by.clone().or(query.enrolled_by);
    let caller = match identify_caller(log, req, db, enroller, "enrolled_by", "FaceVerifyEnrollOthers").await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let subject = match resolve_subject(log, db.get_ref(), up.employee_id.clone(), &caller, "USE_SELF_ENROLL").await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Cheap refusal before spending two face-verify calls; the SQL function
    // repeats it under a row lock, so this is not the guard, only a shortcut.
    let current: Option<Option<String>> = sqlx::query_scalar(
        "SELECT enrolled_by FROM attendance.wow_attendance_enrollments
          WHERE person_id = $1 AND id_type = 'Employee' AND is_active
          ORDER BY enrolled_at DESC, version DESC LIMIT 1",
    )
    .bind(&subject)
    .fetch_optional(db.get_ref())
    .await
    .unwrap_or(None);
    match current {
        Some(None) => {
            return reply(
                StatusCode::CONFLICT,
                "ALREADY_SELF_ENROLLED",
                "This employee enrolled their own selfie. Only they can replace it.",
                json!({ "employee_id": subject }),
            )
        }
        Some(Some(by)) if by != caller.employee_id => {
            return reply(
                StatusCode::CONFLICT,
                "ENROLLED_BY_OTHER",
                "Another colleague enrolled this employee. Only they, or the employee, can replace it.",
                json!({ "employee_id": subject, "enrolled_by": by }),
            )
        }
        _ => {}
    }

    let Some(image) = up.image else {
        return fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", "`employee_selfie_image` is required (one image)");
    };
    let image = reduce_saved_image(&image).await;

    let check = match check_enroll_image(log, db.get_ref(), &image).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // The enroller must not register their OWN face under a colleague's ID —
    // that would let them check that colleague in from anywhere. Compared
    // against the enroller's enrolled selfie when they have one.
    match enrolled_reference(db.get_ref(), &caller.employee_id).await {
        Ok(Some(Some(own))) => {
            log.step("comparing with the enroller's own enrolled selfie");
            match compare_faces(log, db.get_ref(), &own, &image).await {
                Ok(v) if v.matched => {
                    log.step("REFUSED: selfie matches the enroller");
                    record_token_mismatch(
                        db.get_ref(),
                        "FaceVerifyEnrollOthersSelf",
                        &subject,
                        caller.employee_id.parse().unwrap_or(0),
                        None,
                        v.similarity,
                        log,
                    )
                    .await;
                    return reply(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "SAME_AS_ENROLLER",
                        "This photo looks like you. Take the photo of the employee you are enrolling.",
                        json!({ "similarity": v.similarity }),
                    );
                }
                Ok(_) => {}
                Err(e) => return verify_error_response(e),
            }
        }
        Ok(_) => log.step("enroller has no enrolled selfie — same-face check skipped"),
        Err(e) => {
            eprintln!("DB error in face_others_enroll (enroller reference): {e}");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error");
        }
    }

    let mut device = device_object(up.device_info.as_deref());
    device["enroll_method"] = json!("FACE_VERIFY");

    log.step("persisting enrollment via attendance.face_enroll_for_other");
    let result = sqlx::query_scalar::<_, Value>("SELECT attendance.face_enroll_for_other($1, $2, $3, $4, $5)")
        .bind(&subject)
        .bind(&caller.employee_id)
        .bind(&caller.token)
        .bind(&device)
        .bind(vec![image.clone()])
        .fetch_one(db.get_ref())
        .await;

    match result {
        Ok(body) if body.get("success").and_then(Value::as_bool) == Some(true) => {
            let d = body.get("data").cloned().unwrap_or(Value::Null);
            let reenroll = d.get("is_reenrollment").and_then(Value::as_bool).unwrap_or(false);
            reply(
                StatusCode::CREATED,
                "OK",
                if reenroll { "Selfie updated for this employee." } else { "Employee enrolled. You can now mark their attendance." },
                json!({
                    "employee_id":            subject,
                    "enrolled_by":            caller.employee_id,
                    "enrollment_id":          d.get("enrollment_id"),
                    "version":                d.get("version"),
                    "is_reenrollment":        reenroll,
                    "previous_enrollment_id": d.get("previous_enrollment_id"),
                    "image_check":            { "passed": true, "similarity": check.similarity },
                    "next_step":              "MARK_ATTENDANCE",
                }),
            )
        }
        Ok(body) => {
            log.step(format!("enroll function refused: {body}"));
            let code = body.get("code").and_then(Value::as_str).unwrap_or("ENROLL_FAILED").to_string();
            let status = match code.as_str() {
                "ALREADY_SELF_ENROLLED" | "ENROLLED_BY_OTHER" => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            reply(
                status,
                &code,
                body.get("message").and_then(Value::as_str).unwrap_or("Enrollment failed").to_string(),
                body.get("data").cloned().unwrap_or_else(|| json!({})),
            )
        }
        Err(e) => {
            eprintln!("DB error in face_others_enroll: {e}");
            log.step(format!("DB enroll FAILED: {e}"));
            fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error")
        }
    }
}

#[post("/face-attendance/others/mark")]
pub async fn face_others_mark(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<OthersQuery>,
    payload: Multipart,
) -> HttpResponse {
    let log = logger("ext-api/face-attendance/others/mark", &req);
    let resp = face_others_mark_inner(&log, &req, &db, query.into_inner(), payload).await;
    log_local_response(&log, resp).await
}

async fn face_others_mark_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    query: OthersQuery,
    payload: Multipart,
) -> HttpResponse {
    log.step("request received (others)");
    if let Err(resp) = require_token_caller(req) {
        return resp;
    }
    let up = match read_upload(log, payload, &live_dir(), query.employee_id).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let marker = up.enrolled_by.clone().or(query.enrolled_by);
    let caller = match identify_caller(log, req, db, marker, "enrolled_by", "FaceVerifyMarkOthers").await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let subject = match resolve_subject(log, db.get_ref(), up.employee_id.clone(), &caller, "USE_SELF_MARK").await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(selfie) = up.image else {
        return fail(StatusCode::BAD_REQUEST, "INVALID_INPUT", "`selfie_image` is required (one image)");
    };

    mark_core(log, db, &caller, &subject, Some(&caller.employee_id), selfie, up.device_info.as_deref()).await
}

#[derive(serde::Deserialize, Default)]
pub struct OthersListBody {
    pub enrolled_by: Option<String>,
}

/// The colleagues the token holder has enrolled, with whether each one has
/// checked in today. `enrolled_by` is optional; when sent it must be the
/// token holder.
#[post("/face-attendance/others/list")]
pub async fn face_others_list(
    req: actix_web::HttpRequest,
    db: web::Data<PgPool>,
    query: web::Query<OthersQuery>,
    body: Option<web::Json<OthersListBody>>,
) -> HttpResponse {
    let log = logger("ext-api/face-attendance/others/list", &req);
    let resp = face_others_list_inner(&log, &req, &db, query.into_inner(), body.map(|b| b.into_inner())).await;
    log_local_response(&log, resp).await
}

async fn face_others_list_inner(
    log: &StepLogger,
    req: &actix_web::HttpRequest,
    db: &web::Data<PgPool>,
    query: OthersQuery,
    body: Option<OthersListBody>,
) -> HttpResponse {
    let (token_user_id, _) = match require_token_caller(req) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let enroller = body
        .and_then(|b| b.enrolled_by)
        .or(query.enrolled_by)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| token_user_id.to_string());
    let caller = match identify_caller(log, req, db, Some(enroller), "enrolled_by", "FaceVerifyListOthers").await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let rows = sqlx::query_scalar::<_, Value>("SELECT attendance.face_enrolled_by($1)")
        .bind(&caller.employee_id)
        .fetch_one(db.get_ref())
        .await;
    let mut rows = match rows {
        Ok(v) => v,
        Err(e) => {
            eprintln!("DB error in face_others_list: {e}");
            log.step(format!("list FAILED: {e}"));
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal server error");
        }
    };
    // Turn the stored file path into a URL the client can show; never expose
    // the server's filesystem layout.
    let base = public_base_url(req);
    if let Some(list) = rows.as_array_mut() {
        for r in list.iter_mut() {
            let url = r
                .get("image_path")
                .and_then(Value::as_str)
                .and_then(browsable_path)
                .map(|p| format!("{base}{p}"));
            if let Some(o) = r.as_object_mut() {
                o.remove("image_path");
                o.insert("image_url".into(), json!(url));
            }
        }
    }
    let count = rows.as_array().map_or(0, Vec::len);
    reply(
        StatusCode::OK,
        "OK",
        "OK",
        json!({ "enrolled_by": caller.employee_id, "count": count, "employees": rows }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_field_aliases_include_the_spec_spelling() {
        assert!(IMAGE_FIELDS.contains(&"employee_selfie_image"));
        assert!(IMAGE_FIELDS.contains(&"employee_selife_image"));
        assert!(IMAGE_FIELDS.contains(&"selfie_image"));
    }
}
