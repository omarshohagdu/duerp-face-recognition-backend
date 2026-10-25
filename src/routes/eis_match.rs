//! "Match with EIS" — the card-save and enroll checks against the EIS
//! studentship photo.
//!
//! The NFC card number IS the student's EIS studentship token: card
//! `Qvuec1c774mKd4DNt` belongs to
//! `https://academic.eis.du.ac.bd/en/studentship/Qvuec1c774mKd4DNt`. So when
//! `EIS_MATCH` is ON, `save_card_info` — after the card photo has matched the
//! selfie — builds that URL from the card being saved, takes the photo EIS
//! embeds in the page as a `data:` URI, and sends it with the selfie to the
//! same face-match service the card gate uses. Only `match: true` lets the
//! save through.
//!
//! ENROLL runs the same check (`check_enroll`): a student enroll has no card
//! number, so the student's registered card is looked up by registration
//! number, and every enroll image must match the EIS photo before the AI
//! platform learns the face. Employees are skipped — EIS only has students.
//!
//! `EIS_MATCH_URL` is the BASE, `https://academic.eis.du.ac.bd/en/studentship/`;
//! the card number is appended. A URL containing `{card_number}` has it
//! substituted there instead, in case EIS ever moves the token.
//!
//! FAILS CLOSED, like the card gate: with the switch ON, "EIS was down or does
//! not know this card" (EIS answers both with a bare 500), "the page had no
//! photo" or "the face service is not configured" refuse the save rather than write a card
//! that was never matched. OFF skips the check and says so in the step log.

use std::sync::LazyLock;
use std::time::Duration;

use actix_web::http::StatusCode;
use base64::Engine;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::routes::nfc_card::{
    face_verify_api_key, face_verify_message, forward_name_and_mime, forward_part,
    FACE_VERIFY_CLIENT,
};
use crate::utils::settings;
use crate::utils::step_logger::StepLogger;

/// Fetching the EIS page, `EIS_MATCH_TIMEOUT_SECS` (default 15s). Separate
/// from the face service's client: a different host, and a slow EIS should
/// become a 503 well before the card desk gives up on the save.
static EIS_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let secs = std::env::var("EIS_MATCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(15);
    reqwest::Client::builder()
        .timeout(Duration::from_secs(secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// Why the check refused. The caller wraps it in its own envelope, so this
/// module does not pick a response shape for an endpoint it does not own.
#[derive(Debug)]
pub(crate) struct EisRejection {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub data: Option<Value>,
}

fn unavailable(message: &str) -> EisRejection {
    EisRejection {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "eis_match_unavailable",
        message: message.to_string(),
        data: None,
    }
}

/// The studentship page for one card. `None` for a card number that is not a
/// plain token — it goes into a URL path, so `/`, `?`, `..` and the like must
/// never reach EIS as part of someone else's address.
fn studentship_url(base: &str, card_number: &str) -> Option<String> {
    let card = card_number.trim();
    let plain = !card.is_empty()
        && card.len() <= 64
        && card.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return None;
    }
    let base = base.trim();
    Some(if base.contains("{card_number}") {
        base.replace("{card_number}", card)
    } else {
        format!("{}/{card}", base.trim_end_matches('/'))
    })
}

/// The first `<img src="data:…;base64,…">` on the page, decoded.
///
/// EIS labels its JPEG `data:png;base64,` — so the declared type is ignored
/// and the bytes are sent on with a type sniffed from their magic number. The
/// first data-URI image is the student's photo: the only other image on the
/// page is the university logo, which is a plain URL.
fn photo_from_page(html: &str) -> Option<Vec<u8>> {
    let mut rest = html;
    while let Some(at) = rest.find("src=") {
        rest = &rest[at + 4..];
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let value = &rest[1..];
        let end = value.find(quote)?;
        let src = &value[..end];
        if let Some(uri) = src.strip_prefix("data:") {
            if let Some((_, b64)) = uri.split_once(";base64,") {
                let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(cleaned) {
                    if !bytes.is_empty() {
                        return Some(bytes);
                    }
                }
            }
        }
        rest = &value[end..];
    }
    None
}

/// Run the check for one card save. `Ok(true)` = matched, `Ok(false)` = the
/// switch is OFF and nothing was compared.
pub(crate) async fn check_card(
    log: &StepLogger,
    db: &PgPool,
    card_number: &str,
    selfie_path: &str,
) -> Result<bool, EisRejection> {
    let eis = match settings::eis_match(db).await {
        Ok(e) => e,
        Err(e) => {
            log.step(format!("EIS match setting unreadable ({e}) — refusing (fails closed)"));
            return Err(unavailable("Could not read the EIS match setting; card not saved"));
        }
    };
    if !eis.enabled {
        log.step("EIS match is OFF — saving without comparing the selfie to the EIS photo");
        return Ok(false);
    }
    check(log, db, eis.url, card_number, &[("student_selfie", selfie_path)], "card not saved").await?;
    Ok(true)
}

/// Run the check for one enroll. The enroll carries no card number, so the
/// student's REGISTERED card is looked up — enroll `id` is the student's
/// registration number, which is what `nfc_student_cards.student_applicant_id`
/// holds. Every enroll image must match the EIS photo.
///
/// `Ok(true)` = matched, `Ok(false)` = not checked (switch OFF, or an employee:
/// EIS has no studentship page for one, and checking would refuse them all).
/// A student with no registered card is refused while the switch is ON — the
/// check cannot run, and it fails closed.
pub(crate) async fn check_enroll(
    log: &StepLogger,
    db: &PgPool,
    id: &str,
    id_type: &str,
    image_paths: &[String],
) -> Result<bool, EisRejection> {
    let eis = match settings::eis_match(db).await {
        Ok(e) => e,
        Err(e) => {
            log.step(format!("EIS match setting unreadable ({e}) — refusing (fails closed)"));
            return Err(unavailable("Could not read the EIS match setting; nothing was enrolled"));
        }
    };
    if !eis.enabled {
        log.step("EIS match is OFF — enrolling without comparing to the EIS photo");
        return Ok(false);
    }
    if !id_type.eq_ignore_ascii_case("Student") {
        log.step(format!("EIS match is ON but id_type={id_type} — EIS only has students, skipping"));
        return Ok(false);
    }
    let outcome = "nothing was enrolled";
    if image_paths.is_empty() {
        log.step("EIS match is ON and the enroll carried no images — nothing to compare");
        return Err(EisRejection {
            status: StatusCode::BAD_REQUEST,
            code: "eis_no_image",
            message: format!("An image is required to match against EIS; {outcome}"),
            data: None,
        });
    }

    let card = sqlx::query_scalar::<_, String>(
        "SELECT card_number FROM attendance.nfc_student_cards
          WHERE student_applicant_id = $1
          ORDER BY updated_at DESC NULLS LAST, id DESC
          LIMIT 1",
    )
    .bind(id.trim())
    .fetch_optional(db)
    .await;
    let card = match card {
        Ok(Some(c)) => c,
        Ok(None) => {
            log.step(format!("EIS match is ON and student {id} has no registered card — refusing the enroll"));
            return Err(EisRejection {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "eis_no_card",
                message: format!(
                    "EIS matching needs the student's registered card number, and none is registered; {outcome}"
                ),
                data: None,
            });
        }
        Err(e) => {
            log.step(format!("card lookup FAILED: {e}"));
            return Err(unavailable(&format!("Could not look up the student's card; {outcome}")));
        }
    };
    log.step(format!("EIS match: student {id} has card {card}"));

    let labelled: Vec<(String, &str)> = image_paths
        .iter()
        .enumerate()
        .map(|(i, p)| (format!("enroll image {}/{}", i + 1, image_paths.len()), p.as_str()))
        .collect();
    let images: Vec<(&str, &str)> = labelled.iter().map(|(l, p)| (l.as_str(), *p)).collect();
    check(log, db, eis.url, &card, &images, outcome).await?;
    Ok(true)
}

/// The shared part: fetch the studentship page for `card_number`, take its
/// photo, and require every `(label, path)` image to match it. `outcome` ends
/// each refusal message — the caller knows what was not done.
async fn check(
    log: &StepLogger,
    db: &PgPool,
    base_url: Option<String>,
    card_number: &str,
    images: &[(&str, &str)],
    outcome: &str,
) -> Result<(), EisRejection> {

    let Some(base) = base_url else {
        log.step("EIS match is ON with no EIS_MATCH_URL — refusing (fails closed)");
        return Err(unavailable(&format!("EIS matching is on but no EIS URL is configured; {outcome}")));
    };
    let Some(page_url) = studentship_url(&base, card_number) else {
        log.step("card number is not a plain EIS token — refusing");
        return Err(EisRejection {
            status: StatusCode::BAD_REQUEST,
            code: "eis_invalid_card_number",
            message: format!("The card number is not a valid EIS studentship token; {outcome}"),
            data: None,
        });
    };
    let Some(face_url) = settings::face_verify(db).await.url else {
        log.step("EIS match is ON with no face-match service URL — refusing (fails closed)");
        return Err(unavailable(&format!("EIS matching needs the face-match service URL (Face verification settings); {outcome}")));
    };

    log.step(format!("EIS match: fetching studentship page GET {page_url}"));
    let html = match EIS_CLIENT.get(&page_url).send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        // EIS answers an unknown token with a bare 500, the same as a real
        // fault, so the two cannot be told apart — the message names both.
        Ok(r) => {
            log.step(format!(
                "EIS page answered {} — unknown card number or EIS fault; refusing",
                r.status().as_u16()
            ));
            return Err(EisRejection {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "eis_lookup_failed",
                message: format!("EIS has no studentship record for this card number, or EIS is unavailable; {outcome}"),
                data: None,
            });
        }
        Err(e) => {
            log.step(format!("EIS page request FAILED: {e}"));
            return Err(unavailable(&format!("EIS is unavailable; {outcome}")));
        }
    };
    // The page is one student's personal record, so it is not logged — only
    // what came of reading it.
    let Some(photo) = photo_from_page(&html) else {
        log.step(format!(
            "EIS page ({} bytes) carried no photo — unknown card number, or EIS changed its page",
            html.len()
        ));
        return Err(EisRejection {
            status: StatusCode::BAD_REQUEST,
            code: "eis_no_photo",
            message: format!("No EIS studentship photo was found for this card number; {outcome}"),
            data: None,
        });
    };
    log.step(format!("EIS photo extracted ({} bytes)", photo.len()));

    for (label, path) in images {
        compare(log, &face_url, &photo, label, path, outcome).await?;
    }
    log.step("EIS match: every image matches the EIS studentship photo");
    Ok(())
}

/// One image against the EIS photo. The face service's field names are the
/// card gate's — `card_image` is the reference, `selfie_image` the capture.
async fn compare(
    log: &StepLogger,
    face_url: &str,
    photo: &[u8],
    label: &str,
    path: &str,
    outcome: &str,
) -> Result<(), EisRejection> {
    let (name, mime) = forward_name_and_mime("card_image", &photo[..photo.len().min(16)]);
    let reference = match reqwest::multipart::Part::bytes(photo.to_vec())
        .file_name(name)
        .mime_str(mime)
    {
        Ok(p) => p,
        Err(e) => {
            log.step(format!("EIS photo could not be attached: {e}"));
            return Err(unavailable(&format!("EIS matching could not prepare the images; {outcome}")));
        }
    };
    let capture = match forward_part("selfie_image", path).await {
        Ok(p) => p,
        Err(e) => {
            log.step(format!("{label} could not be read back: {e}"));
            return Err(unavailable(&format!("EIS matching could not read the {label}; {outcome}")));
        }
    };
    let form = reqwest::multipart::Form::new()
        .part("card_image", reference)
        .part("selfie_image", capture);

    log.step(format!("EIS match: comparing {label} with the EIS photo: POST {face_url}"));
    let resp = match FACE_VERIFY_CLIENT
        .post(face_url)
        .header("X-API-Key", face_verify_api_key())
        .multipart(form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log.step(format!("face-match request FAILED: {e}"));
            return Err(unavailable(&format!("Face-match service is unavailable; {outcome}")));
        }
    };

    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    let Ok(body) = serde_json::from_str::<Value>(&raw) else {
        log.ai_response("/verify (eis)", status.as_u16(), &json!({ "non_json_body": raw }));
        return Err(unavailable(&format!("Face-match service returned an unreadable response; {outcome}")));
    };
    log.ai_response("/verify (eis)", status.as_u16(), &body);

    if matches!(status.as_u16(), 401 | 403) || status.is_server_error() {
        log.step(format!("face-match service failed ({}) — refusing", status.as_u16()));
        return Err(unavailable(&format!("Face-match service is unavailable; {outcome}")));
    }
    if status.is_client_error() {
        let message = face_verify_message(&body)
            .unwrap_or_else(|| format!("The {label} could not be compared with the EIS photo"));
        log.step(format!("{label} and EIS photo could not be compared ({}) — returning 400", status.as_u16()));
        return Err(EisRejection {
            status: StatusCode::BAD_REQUEST,
            code: "eis_face_not_comparable",
            message,
            data: None,
        });
    }

    let null = Value::Null;
    log.step(format!(
        "EIS verdict for {label}: match={} similarity={} threshold={}",
        body.get("match").unwrap_or(&null),
        body.get("similarity").unwrap_or(&null),
        body.get("threshold").unwrap_or(&null),
    ));
    match body.get("match").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => Err(EisRejection {
            status: StatusCode::BAD_REQUEST,
            code: "eis_face_mismatch",
            message: format!("The {label} does not match the EIS studentship photo; {outcome}"),
            data: Some(json!({
                "image":      label,
                "match":      false,
                "similarity": body.get("similarity").cloned().unwrap_or(Value::Null),
                "threshold":  body.get("threshold").cloned().unwrap_or(Value::Null),
                "detail":     face_verify_message(&body),
            })),
        }),
        None => {
            log.step("face-match reply carried no `match` field — refusing");
            Err(unavailable(&format!("Face-match service returned an unexpected response; {outcome}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_photo_is_found_and_the_logo_url_is_skipped() {
        // EIS's own shape: a logo by URL first, then the photo as a data URI
        // whose declared type ("png") is wrong for its JPEG bytes.
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let html = format!(
            r#"<img src="https://academic.eis.du.ac.bd/images/dulogo/dulogo.png" width="50">
               <img src="data:png;base64,{b64}" alt="x">"#
        );
        assert_eq!(photo_from_page(&html).as_deref(), Some(&jpeg[..]));
    }

    #[test]
    fn single_quotes_and_wrapped_base64_still_decode() {
        let html = "<img src='data:image/jpeg;base64,/9j/\n4A=='>";
        assert_eq!(photo_from_page(html), Some(vec![0xFF, 0xD8, 0xFF, 0xE0]));
    }

    #[test]
    fn the_card_number_is_appended_to_the_base() {
        let want = Some("https://academic.eis.du.ac.bd/en/studentship/Qvuec1c774mKd4DNt".to_string());
        for base in [
            "https://academic.eis.du.ac.bd/en/studentship/",
            "https://academic.eis.du.ac.bd/en/studentship",
            "https://academic.eis.du.ac.bd/en/studentship/{card_number}",
        ] {
            assert_eq!(studentship_url(base, " Qvuec1c774mKd4DNt "), want, "base={base}");
        }
    }

    #[test]
    fn a_card_number_that_is_not_a_plain_token_never_reaches_eis() {
        let base = "https://academic.eis.du.ac.bd/en/studentship/";
        for bad in ["", "  ", "../admin", "a/b", "a?x=1", "a#b", "a b", "a%2F", &"x".repeat(65)] {
            assert_eq!(studentship_url(base, bad), None, "card={bad:?}");
        }
        assert!(studentship_url(base, "AB-12_cd").is_some());
    }

    #[test]
    fn a_page_without_a_photo_yields_nothing() {
        assert_eq!(photo_from_page(r#"<img src="/logo.png"><p>login</p>"#), None);
        assert_eq!(photo_from_page(r#"<img src="data:png;base64,!!!">"#), None);
        assert_eq!(photo_from_page(""), None);
    }
}
