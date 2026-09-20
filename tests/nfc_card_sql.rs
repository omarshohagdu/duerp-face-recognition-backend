//! DB-backed tests for the NFC card SQL functions.
//!
//! These cover the rules `src/routes/nfc_card.rs` deliberately does NOT own —
//! card-number normalisation, the 1/2 enums, the forced `is_verified = 2` for
//! self-registration, card ownership and the audit trail — because all of them
//! live in `attendance.nfc_card_save_info` so they hold for any caller.
//!
//! Every test runs inside a transaction that is dropped without committing, so
//! the database is left exactly as it was found. Applicant ids and card numbers
//! are suffixed with a per-call random value, so a run against a database that
//! already holds real card records cannot collide with them.
//!
//! Requires `DATABASE_URL`, and the functions must already be applied:
//!
//!     psql "$DATABASE_URL" -f sql/004_nfc_card.sql
//!     cargo test --test nfc_card_sql
//!
//! Without either, every test skips rather than fails, so `cargo test` still
//! works on a machine with no database.

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()
}

/// Is `sql/004_nfc_card.sql` applied to this database?
///
/// Checked rather than assumed so an unapplied migration SKIPS with a message
/// naming the file, instead of failing every test with an opaque "function
/// does not exist" from Postgres.
async fn functions_present(pool: &PgPool) -> bool {
    // Every function this file exercises, not just one: a database carrying an
    // earlier version of the migration would otherwise pass this check and
    // then fail the newer tests with an opaque "function does not exist".
    sqlx::query_scalar::<_, bool>(
        "SELECT count(*) = 2 FROM pg_proc p
           JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'attendance'
            AND p.proname IN ('nfc_card_save_info', 'nfc_card_reg_status')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

/// Skip the test (printing why) when there is nothing to test against.
macro_rules! db_or_skip {
    () => {{
        match pool().await {
            Some(p) => {
                if !functions_present(&p).await {
                    eprintln!(
                        "skipping: attendance.nfc_card_save_info not found — \
                         apply sql/004_nfc_card.sql first"
                    );
                    return;
                }
                p
            }
            None => {
                eprintln!("skipping: DATABASE_URL unset or unreachable");
                return;
            }
        }
    }};
}

/// A per-test unique suffix, so fixtures cannot collide with real rows or with
/// a concurrently running test.
fn unique() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_uppercase()
}

/// Applicant id / card number pair scoped to one test.
fn fixture_ids() -> (String, String) {
    let u = unique();
    (format!("APPTEST-{u}"), format!("CARDTEST{u}"))
}

/// Call the save function exactly as the handler does, with the face gate ON.
#[allow(clippy::too_many_arguments)]
async fn save(
    tx: &mut Transaction<'_, Postgres>,
    applicant: &str,
    card: Option<&str>,
    is_verified: Option<i16>,
    registration_type: Option<i16>,
    card_image: Option<&str>,
    student_selfie: Option<&str>,
    force_reassign: bool,
) -> Value {
    save_gated(
        tx, applicant, card, is_verified, registration_type, card_image,
        student_selfie, force_reassign, true,
    )
    .await
}

/// The same, with `p_face_checked` spelled out — `false` is a save made while
/// the face gate was OFF.
#[allow(clippy::too_many_arguments)]
async fn save_gated(
    tx: &mut Transaction<'_, Postgres>,
    applicant: &str,
    card: Option<&str>,
    is_verified: Option<i16>,
    registration_type: Option<i16>,
    card_image: Option<&str>,
    student_selfie: Option<&str>,
    force_reassign: bool,
    face_checked: bool,
) -> Value {
    sqlx::query_scalar::<_, Value>(
        "SELECT attendance.nfc_card_save_info($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(applicant)
    .bind(card)
    .bind(is_verified)
    .bind(registration_type)
    .bind(card_image)
    .bind(student_selfie)
    .bind(force_reassign)
    .bind(45320_i64)
    .bind("127.0.0.1")
    .bind(face_checked)
    .fetch_one(&mut **tx)
    .await
    .expect("card save call failed")
}

/// The common case: an admin registration with a card and no images.
async fn save_admin(
    tx: &mut Transaction<'_, Postgres>,
    applicant: &str,
    card: &str,
) -> Value {
    save(tx, applicant, Some(card), Some(1), Some(1), None, None, false).await
}

async fn get_info(tx: &mut Transaction<'_, Postgres>, card: Option<&str>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.nfc_card_get_info($1)")
        .bind(card)
        .fetch_one(&mut **tx)
        .await
        .expect("card get call failed")
}

/// Call the registration-status function the way its handler does: keyed on
/// the student's registration number, not on a card.
async fn reg_status(tx: &mut Transaction<'_, Postgres>, registration_no: Option<&str>) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT attendance.nfc_card_reg_status($1)")
        .bind(registration_no)
        .fetch_one(&mut **tx)
        .await
        .expect("card reg-status call failed")
}

async fn normalize(tx: &mut Transaction<'_, Postgres>, raw: Option<&str>) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT attendance.nfc_card_normalize($1)")
        .bind(raw)
        .fetch_one(&mut **tx)
        .await
        .expect("normalize call failed")
}

/// The NFC contract reports outcome as `status: "success" | "error"`, not as the
/// `success` boolean the rest of the service uses. Fails closed on anything
/// unexpected so a missing or misspelled status can never read as a pass.
fn ok(v: &Value) -> bool {
    v.get("status").and_then(Value::as_str) == Some("success")
}

fn code(v: &Value) -> String {
    v.get("code").and_then(Value::as_str).unwrap_or("").to_string()
}

fn message(v: &Value) -> String {
    v.get("message").and_then(Value::as_str).unwrap_or("").to_string()
}

fn data<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get("data")
        .and_then(|d| d.get(key))
        .unwrap_or(&Value::Null)
}

fn warnings(v: &Value) -> Vec<String> {
    v.get("warnings")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The audit rows for one applicant, oldest first, as (action, changed_fields).
async fn audit_for(
    tx: &mut Transaction<'_, Postgres>,
    applicant: &str,
) -> Vec<(String, Vec<String>)> {
    sqlx::query_as::<_, (String, Vec<String>)>(
        "SELECT action, changed_fields FROM attendance.nfc_card_audit
          WHERE student_applicant_id = $1 ORDER BY id",
    )
    .bind(applicant)
    .fetch_all(&mut **tx)
    .await
    .expect("audit read failed")
}

// ---------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------

#[tokio::test]
async fn one_card_read_three_ways_is_one_card_number() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // Whatever separator style the reader emits, the stored form is the same —
    // this is what stops one physical card occupying three rows.
    let expected = Some("04A1B2C3D4E5".to_string());
    assert_eq!(normalize(&mut tx, Some("04:a1:b2:c3:d4:e5")).await, expected);
    assert_eq!(normalize(&mut tx, Some("04-A1-B2-C3-D4-E5")).await, expected);
    assert_eq!(normalize(&mut tx, Some(" 04a1b2c3d4e5 ")).await, expected);
    assert_eq!(normalize(&mut tx, Some("04 a1 b2 c3 d4 e5")).await, expected);
}

#[tokio::test]
async fn a_value_that_normalises_away_is_null_not_empty() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // "No card" must have exactly one representation, or an empty string would
    // become a card that many students could hold at once.
    assert_eq!(normalize(&mut tx, Some("---")).await, None);
    assert_eq!(normalize(&mut tx, Some("   ")).await, None);
    assert_eq!(normalize(&mut tx, Some("")).await, None);
    assert_eq!(normalize(&mut tx, None).await, None);
}

// ---------------------------------------------------------------------
// get_card_info
// ---------------------------------------------------------------------

#[tokio::test]
async fn lookup_finds_a_card_saved_in_a_different_separator_style() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // Saved bare, scanned with colons: the reader that enrolled the card and
    // the turnstile that reads it need not agree on formatting.
    assert!(ok(&save_admin(&mut tx, &applicant, &card).await));

    let colonised = card
        .chars()
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(":");
    let found = get_info(&mut tx, Some(&colonised)).await;

    assert!(ok(&found), "lookup failed: {found}");
    assert_eq!(data(&found, "student_applicant_id"), applicant.as_str());
    assert_eq!(data(&found, "card_number"), card.as_str());
}

// ---------------------------------------------------------------------
// The response envelope
//
// `status` + `message` replaced the `success` boolean for this module only, so
// these pin the exact strings a client branches on.
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_envelope_uses_status_and_never_the_success_boolean() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    let saved = save_admin(&mut tx, &applicant, &card).await;
    let found = get_info(&mut tx, Some(&card)).await;
    let missing = get_info(&mut tx, Some("ZZNOSUCH")).await;

    for (label, v) in [("save", &saved), ("lookup", &found), ("not-found", &missing)] {
        assert!(
            v.get("success").is_none(),
            "{label}: the `success` boolean must be gone, got {v}"
        );
        assert!(
            v.get("status").and_then(Value::as_str).is_some(),
            "{label}: `status` must be a string, got {v}"
        );
        assert!(
            v.get("error").is_none(),
            "{label}: the redundant `error` mirror must be gone, got {v}"
        );
    }
    assert_eq!(saved.get("status").unwrap(), "success");
    assert_eq!(found.get("status").unwrap(), "success");
    assert_eq!(missing.get("status").unwrap(), "error");
}

#[tokio::test]
async fn lookup_messages_are_the_agreed_strings() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save_admin(&mut tx, &applicant, &card).await;

    // These two exact strings are the contract; a reword is a client change.
    assert_eq!(message(&get_info(&mut tx, Some(&card)).await), "Data Found");
    assert_eq!(
        message(&get_info(&mut tx, Some("ZZNOSUCHCARD")).await),
        "No Data Found"
    );
}

#[tokio::test]
async fn every_failure_carries_a_status_a_code_and_a_message() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();
    let (other, _) = fixture_ids();
    save_admin(&mut tx, &applicant, &card).await;

    let failures = vec![
        ("missing card", get_info(&mut tx, None).await),
        ("unknown card", get_info(&mut tx, Some("ZZNOSUCH")).await),
        (
            "missing fields",
            save(&mut tx, "", Some(&card), Some(1), Some(1), None, None, false).await,
        ),
        (
            "bad enum",
            save(&mut tx, &applicant, Some(&card), Some(7), Some(1), None, None, false).await,
        ),
        (
            "short card",
            save(&mut tx, &applicant, Some("A"), Some(1), Some(1), None, None, false).await,
        ),
        (
            "conflict",
            save(&mut tx, &other, Some(&card), Some(1), Some(1), None, None, false).await,
        ),
    ];

    for (label, v) in failures {
        assert_eq!(v.get("status").unwrap(), "error", "{label}: {v}");
        assert!(!code(&v).is_empty(), "{label} has no code: {v}");
        assert!(!message(&v).is_empty(), "{label} has no message: {v}");
    }
}

// ---------------------------------------------------------------------
// Enum labels
// ---------------------------------------------------------------------

#[tokio::test]
async fn labels_accompany_the_numbers_on_both_endpoints() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (admin_student, admin_card) = fixture_ids();
    let (self_student, self_card) = fixture_ids();

    // 1/1 -> Verified / Admin
    let saved = save(
        &mut tx,
        &admin_student,
        Some(&admin_card),
        Some(1),
        Some(1),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(data(&saved, "is_verified"), 1);
    assert_eq!(data(&saved, "is_verified_label"), "Verified");
    assert_eq!(data(&saved, "registration_type"), 1);
    assert_eq!(data(&saved, "registration_type_label"), "Admin");

    // The lookup must agree with the save.
    let found = get_info(&mut tx, Some(&admin_card)).await;
    assert_eq!(data(&found, "is_verified_label"), "Verified");
    assert_eq!(data(&found, "registration_type_label"), "Admin");

    // 2/2 -> Not Verified / Self (is_verified is forced to 2 here anyway)
    let self_saved = save(
        &mut tx,
        &self_student,
        Some(&self_card),
        Some(1),
        Some(2),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(data(&self_saved, "is_verified_label"), "Not Verified");
    assert_eq!(data(&self_saved, "registration_type_label"), "Self");

    let self_found = get_info(&mut tx, Some(&self_card)).await;
    assert_eq!(data(&self_found, "is_verified_label"), "Not Verified");
    assert_eq!(data(&self_found, "registration_type_label"), "Self");
}

#[tokio::test]
async fn the_label_helper_refuses_to_invent_a_label() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // A CHECK constraint keeps these out of stored rows, but the function is
    // callable directly. Labelling an unknown code as one of the two valid
    // meanings would be worse than saying nothing.
    let out = sqlx::query_scalar::<_, Value>(
        "SELECT attendance.nfc_card_labels($1::smallint, $2::smallint)",
    )
    .bind(9_i16)
    .bind(0_i16)
    .fetch_one(&mut *tx)
    .await
    .expect("labels call failed");

    assert_eq!(out["is_verified_label"], Value::Null);
    assert_eq!(out["registration_type_label"], Value::Null);
}

#[tokio::test]
async fn lookup_returns_only_the_specified_fields() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // Images and timestamps must not ride along: a turnstile calls this on
    // every tap, and the payload is meant to stay small and free of PII.
    save(
        &mut tx,
        &applicant,
        Some(&card),
        Some(1),
        Some(1),
        Some("/app/uploads/nfc_card/cards/a.jpg"),
        Some("/app/uploads/nfc_card/selfies/a.jpg"),
        false,
    )
    .await;

    let found = get_info(&mut tx, Some(&card)).await;
    let obj = found
        .get("data")
        .and_then(Value::as_object)
        .expect("data object");

    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort();
    // The four contract fields plus the two labels — and nothing else. The
    // exact-list assertion is the point: it fails if a future change starts
    // leaking image paths or timestamps into the scan response.
    assert_eq!(
        keys,
        vec![
            "card_number",
            "is_verified",
            "is_verified_label",
            "registration_type",
            "registration_type_label",
            "student_applicant_id"
        ]
    );
    // Explicitly: the images that ARE stored on this row must not appear.
    assert!(obj.get("card_image").is_none());
    assert!(obj.get("student_selfie").is_none());
}

#[tokio::test]
async fn lookup_reports_an_unverified_mapping_rather_than_hiding_it() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save(&mut tx, &applicant, Some(&card), Some(2), Some(1), None, None, false).await;

    // The flag is the answer, not an error: the caller decides what an
    // unreviewed mapping may do.
    let found = get_info(&mut tx, Some(&card)).await;
    assert!(ok(&found), "unverified lookup should still succeed: {found}");
    assert_eq!(data(&found, "is_verified"), 2);
}

#[tokio::test]
async fn lookup_distinguishes_missing_input_from_unknown_card() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // The handler maps these to 400 and 404 respectively, so they must not
    // collapse into one code.
    for empty in [None, Some(""), Some("   "), Some("--")] {
        let r = get_info(&mut tx, empty).await;
        assert!(!ok(&r));
        assert_eq!(code(&r), "missing_card_number", "for input {empty:?}");
    }

    let unknown = get_info(&mut tx, Some(&format!("NOSUCH{}", unique()))).await;
    assert!(!ok(&unknown));
    assert_eq!(code(&unknown), "card_not_found");
}

// ---------------------------------------------------------------------
// checking_card_reg_status — "does this student already hold a card?"
//
// Keyed on the registration number (`student_applicant_id`), not on the card,
// and with a deliberately different payload from get_info: `data` on every
// response, and the whole row rather than the four lean fields. These pin that
// difference, because the two are one `CREATE OR REPLACE` away from being
// quietly harmonised into each other.
// ---------------------------------------------------------------------

#[tokio::test]
async fn reg_status_returns_the_whole_registration() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save(
        &mut tx,
        &applicant,
        Some(&card),
        Some(1),
        Some(1),
        Some("/app/uploads/nfc_card/cards/a.jpg"),
        Some("/app/uploads/nfc_card/selfies/a.jpg"),
        false,
    )
    .await;

    let r = reg_status(&mut tx, Some(&applicant)).await;
    assert!(ok(&r), "a registered student should be found: {r}");
    assert_eq!(data(&r, "is_registered"), true);
    assert_eq!(data(&r, "student_applicant_id"), applicant.as_str());
    // The caller asked with `registration_no`, so it is echoed under that name
    // too — one column, two names, neither of which a client must translate.
    assert_eq!(data(&r, "registration_no"), applicant.as_str());
    assert_eq!(data(&r, "card_number"), card.as_str());
    assert_eq!(data(&r, "is_verified_label"), "Verified");
    assert_eq!(data(&r, "registration_type_label"), "Admin");

    // The images and timestamps get_info deliberately withholds. The handler
    // swaps the two paths for public URLs; SQL callers see the stored paths.
    assert_eq!(data(&r, "card_image"), "/app/uploads/nfc_card/cards/a.jpg");
    assert_eq!(
        data(&r, "student_selfie"),
        "/app/uploads/nfc_card/selfies/a.jpg"
    );
    assert!(
        data(&r, "registered_at").is_string(),
        "registered_at should be a timestamp: {r}"
    );
    assert!(data(&r, "updated_at").is_string(), "no updated_at: {r}");
}

#[tokio::test]
async fn reg_status_carries_a_data_object_on_every_answer() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();
    save_admin(&mut tx, &applicant, &card).await;

    // The whole point of this endpoint's envelope: a client may read `data`
    // without first branching on `status`. An omitted `data` — which is what
    // get_info does on failure — would be a null dereference there.
    for (label, v) in [
        ("registered", reg_status(&mut tx, Some(&applicant)).await),
        ("unregistered", reg_status(&mut tx, Some("NOSUCHSTUDENT")).await),
        ("missing input", reg_status(&mut tx, None).await),
    ] {
        assert!(
            v.get("data").map(Value::is_object).unwrap_or(false),
            "{label}: data must be an object, got {v}"
        );
    }

    // And it is EMPTY when there is nothing to report — not a row with nulls.
    assert_eq!(
        reg_status(&mut tx, Some("NOSUCHSTUDENT")).await["data"],
        serde_json::json!({})
    );
    assert_eq!(reg_status(&mut tx, None).await["data"], serde_json::json!({}));
}

#[tokio::test]
async fn reg_status_messages_are_the_agreed_strings() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();
    save_admin(&mut tx, &applicant, &card).await;

    // The success message names the student the answer is about.
    let found = reg_status(&mut tx, Some(&applicant)).await;
    assert!(
        message(&found).contains(&applicant),
        "message should name the registration no: {}",
        message(&found)
    );

    // "No Data Found" is the contract string for the not-registered case; a
    // reword is a client change.
    assert_eq!(
        message(&reg_status(&mut tx, Some("NOSUCHSTUDENT")).await),
        "No Data Found"
    );
}

#[tokio::test]
async fn reg_status_distinguishes_missing_input_from_an_unregistered_student() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    // The handler maps these to 400 and 404, so they must not collapse into one
    // code. Note `--` is NOT a missing value here, unlike on the card lookup:
    // an applicant id is not normalised, so it is simply a student nobody has
    // heard of.
    for empty in [None, Some(""), Some("   ")] {
        let r = reg_status(&mut tx, empty).await;
        assert!(!ok(&r));
        assert_eq!(code(&r), "missing_registration_no", "for input {empty:?}");
    }

    let unknown = reg_status(&mut tx, Some(&format!("NOSUCH{}", unique()))).await;
    assert!(!ok(&unknown));
    assert_eq!(code(&unknown), "card_not_found");
}

#[tokio::test]
async fn reg_status_trims_but_does_not_otherwise_rewrite_the_registration_no() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();
    save_admin(&mut tx, &applicant, &card).await;

    // Whitespace is stripped — the save stores a btrim'd value, so a padded
    // lookup has to find it.
    assert!(ok(&reg_status(&mut tx, Some(&format!("  {applicant}  "))).await));

    // But nothing else is. The card-UID rules (case-folding, separators
    // stripped) must NOT leak onto this key: matching more loosely than the
    // save does would report one student's card under another's id.
    for altered in [
        applicant.to_lowercase(),
        applicant.replace('-', ""),
        format!("{applicant}X"),
    ] {
        let r = reg_status(&mut tx, Some(&altered)).await;
        assert!(!ok(&r), "{altered} must not match {applicant}: {r}");
        assert_eq!(code(&r), "card_not_found");
    }
}

#[tokio::test]
async fn reg_status_counts_an_unverified_mapping_as_registered() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // "Registered" and "verified" are different questions. A card awaiting
    // review is still taken, and a desk that read this as "not registered"
    // would issue a second card to the same student.
    save(&mut tx, &applicant, Some(&card), Some(2), Some(2), None, None, false).await;

    let r = reg_status(&mut tx, Some(&applicant)).await;
    assert!(ok(&r), "unverified registration is still a registration: {r}");
    assert_eq!(data(&r, "is_registered"), true);
    assert_eq!(data(&r, "is_verified"), 2);
    assert_eq!(data(&r, "is_verified_label"), "Not Verified");
}

#[tokio::test]
async fn a_student_whose_card_was_reissued_is_no_longer_registered() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (first, card) = fixture_ids();
    let (second, _) = fixture_ids();

    save_admin(&mut tx, &first, &card).await;
    assert!(ok(&reg_status(&mut tx, Some(&first)).await));

    // The card moves to another student. The first student's RECORD survives,
    // images and all, with a NULL card number — but they hold no card, so the
    // answer to "is their card registered?" flips to no. Reporting them as
    // registered would stop the desk issuing them a replacement.
    save(&mut tx, &second, Some(&card), Some(1), Some(1), None, None, true).await;

    let old_holder = reg_status(&mut tx, Some(&first)).await;
    assert!(!ok(&old_holder), "card was taken away: {old_holder}");
    assert_eq!(code(&old_holder), "card_not_found");
    assert_eq!(old_holder["data"], serde_json::json!({}));

    // ...and the new holder is the one who now has it.
    let new_holder = reg_status(&mut tx, Some(&second)).await;
    assert!(ok(&new_holder));
    assert_eq!(data(&new_holder, "card_number"), card.as_str());
}

// ---------------------------------------------------------------------
// save_card_info — create and update
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_second_save_updates_the_same_row() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    let first = save_admin(&mut tx, &applicant, &card).await;
    assert_eq!(data(&first, "created"), true);

    let second = save_admin(&mut tx, &applicant, &card).await;
    assert_eq!(data(&second, "created"), false);

    // One student, one card record — a repeated save is not a second row.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM attendance.nfc_student_cards WHERE student_applicant_id = $1",
    )
    .bind(&applicant)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn omitting_an_image_keeps_the_stored_one() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    let card_path = "/app/uploads/nfc_card/cards/original.jpg";
    let selfie_path = "/app/uploads/nfc_card/selfies/original.jpg";
    save(
        &mut tx,
        &applicant,
        Some(&card),
        Some(2),
        Some(1),
        Some(card_path),
        Some(selfie_path),
        false,
    )
    .await;

    // The verification step: an admin flipping is_verified sends no files, and
    // must not thereby wipe the images the previous save uploaded.
    let verified = save(&mut tx, &applicant, Some(&card), Some(1), Some(1), None, None, false).await;

    assert!(ok(&verified));
    assert_eq!(data(&verified, "is_verified"), 1);
    assert_eq!(data(&verified, "card_image"), card_path);
    assert_eq!(data(&verified, "student_selfie"), selfie_path);
}

#[tokio::test]
async fn a_supplied_image_replaces_the_stored_one() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save(
        &mut tx,
        &applicant,
        Some(&card),
        Some(1),
        Some(1),
        Some("/app/uploads/nfc_card/cards/old.jpg"),
        None,
        false,
    )
    .await;

    let replaced = save(
        &mut tx,
        &applicant,
        Some(&card),
        Some(1),
        Some(1),
        Some("/app/uploads/nfc_card/cards/new.jpg"),
        None,
        false,
    )
    .await;

    assert_eq!(
        data(&replaced, "card_image"),
        "/app/uploads/nfc_card/cards/new.jpg"
    );
    let changed: Vec<&str> = data(&replaced, "changed_fields")
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(changed.contains(&"card_image"), "changed: {changed:?}");
}

#[tokio::test]
async fn self_registration_cannot_mark_itself_verified() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // is_verified = 1 is asked for and must be ignored: a student registering
    // their own card cannot promote it to trusted.
    let r = save(&mut tx, &applicant, Some(&card), Some(1), Some(2), None, None, false).await;

    assert!(ok(&r));
    assert_eq!(data(&r, "is_verified"), 2);
    assert!(
        warnings(&r).iter().any(|w| w.contains("forced to 2")),
        "the override should be reported, not silent: {:?}",
        warnings(&r)
    );

    // And it is the stored value, not just the response.
    let stored: i16 = sqlx::query_scalar(
        "SELECT is_verified FROM attendance.nfc_student_cards WHERE student_applicant_id = $1",
    )
    .bind(&applicant)
    .fetch_one(&mut *tx)
    .await
    .expect("read back");
    assert_eq!(stored, 2);
}

#[tokio::test]
async fn a_save_made_with_the_face_gate_off_is_recorded_as_not_face_checked() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // The desk asked for "Verified". Nothing compared the two photographs, so
    // the row must not say so — and must not say "2" either, which would be
    // indistinguishable from a save that WAS checked and is awaiting review.
    let r = save_gated(&mut tx, &applicant, Some(&card), Some(1), Some(1), None, None, false, false).await;
    assert!(ok(&r), "{r}");
    assert_eq!(data(&r, "is_verified"), 0);
    assert_eq!(data(&r, "is_verified_label"), "Not Face-Checked");

    // And the caller is told, rather than having to notice.
    assert!(
        warnings(&r).iter().any(|w| w.contains("Not Face-Checked")),
        "the forced value should be warned about: {:?}",
        warnings(&r)
    );

    // The lookups report it like any other state.
    let found = get_info(&mut tx, Some(&card)).await;
    assert_eq!(data(&found, "is_verified"), 0);
    assert_eq!(data(&found, "is_verified_label"), "Not Face-Checked");
}

#[tokio::test]
async fn not_face_checked_outranks_the_self_registration_rule() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // Self-registration normally forces 2. With no face check it is 0, which
    // still satisfies what that rule exists for — a self-registration cannot
    // call itself verified — while saying something 2 does not: nobody looked.
    let r = save_gated(&mut tx, &applicant, Some(&card), Some(1), Some(2), None, None, false, false).await;
    assert!(ok(&r));
    assert_eq!(data(&r, "is_verified"), 0);
}

#[tokio::test]
async fn a_later_checked_save_clears_the_not_face_checked_state() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save_gated(&mut tx, &applicant, Some(&card), Some(1), Some(1), None, None, false, false).await;
    assert_eq!(data(&get_info(&mut tx, Some(&card)).await, "is_verified"), 0);

    // Turning the gate back on and re-saving is how a card leaves that state —
    // it is not sticky, it is a record of what happened at write time.
    let again = save(&mut tx, &applicant, Some(&card), Some(1), Some(1), None, None, false).await;
    assert!(ok(&again));
    assert_eq!(data(&again, "is_verified"), 1);
}

#[tokio::test]
async fn zero_cannot_be_claimed_by_a_caller() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // 0 means "this service did not compare the photographs". A desk cannot
    // assert that about us, and a client that read a 0 back and echoed it is
    // telling us something it cannot know.
    let r = save(&mut tx, &applicant, Some(&card), Some(0), Some(1), None, None, false).await;
    assert!(!ok(&r), "{r}");
    assert_eq!(code(&r), "invalid_value");
}

#[tokio::test]
async fn is_verified_defaults_to_unreviewed() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    // Omitted, not zero: a new mapping is unreviewed until someone says so.
    let r = save(&mut tx, &applicant, Some(&card), None, Some(1), None, None, false).await;
    assert!(ok(&r));
    assert_eq!(data(&r, "is_verified"), 2);
}

// ---------------------------------------------------------------------
// save_card_info — card ownership
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_card_held_by_another_student_is_refused() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (first_student, card) = fixture_ids();
    let (second_student, _) = fixture_ids();

    save_admin(&mut tx, &first_student, &card).await;
    let refused = save_admin(&mut tx, &second_student, &card).await;

    assert!(!ok(&refused));
    assert_eq!(code(&refused), "card_conflict");
    // Naming the current holder is what lets a desk resolve the clash without
    // a DBA; without it the operator only knows "someone".
    assert_eq!(data(&refused, "assigned_to"), first_student.as_str());

    // Nothing was written for the rejected student.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM attendance.nfc_student_cards WHERE student_applicant_id = $1",
    )
    .bind(&second_student)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn the_same_student_resaving_their_own_card_is_not_a_conflict() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save_admin(&mut tx, &applicant, &card).await;
    // Idempotent, per the spec: re-scanning the card you already hold is an
    // update, not someone else's card.
    let again = save_admin(&mut tx, &applicant, &card).await;
    assert!(ok(&again), "resave should succeed: {again}");
}

#[tokio::test]
async fn force_reassign_moves_the_card_and_strips_the_old_holder() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (first_student, card) = fixture_ids();
    let (second_student, _) = fixture_ids();

    save(
        &mut tx,
        &first_student,
        Some(&card),
        Some(1),
        Some(1),
        Some("/app/uploads/nfc_card/cards/first.jpg"),
        None,
        false,
    )
    .await;

    let moved = save(
        &mut tx,
        &second_student,
        Some(&card),
        Some(1),
        Some(1),
        None,
        None,
        true,
    )
    .await;

    assert!(ok(&moved), "reassign failed: {moved}");
    assert_eq!(data(&moved, "reassigned_from"), first_student.as_str());
    assert!(
        warnings(&moved).iter().any(|w| w.contains(&first_student)),
        "losing a card must be reported: {:?}",
        warnings(&moved)
    );

    // The old holder keeps their record and images; only the card is gone.
    let (old_card, old_image): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT card_number, card_image FROM attendance.nfc_student_cards
          WHERE student_applicant_id = $1",
    )
    .bind(&first_student)
    .fetch_one(&mut *tx)
    .await
    .expect("read old holder");
    assert_eq!(old_card, None, "old holder should have no card");
    assert_eq!(
        old_image.as_deref(),
        Some("/app/uploads/nfc_card/cards/first.jpg"),
        "reassignment must not destroy the old holder's images"
    );

    // And a lookup now resolves to the new holder.
    let found = get_info(&mut tx, Some(&card)).await;
    assert_eq!(data(&found, "student_applicant_id"), second_student.as_str());
}

#[tokio::test]
async fn a_student_who_lost_a_card_can_be_given_a_new_one() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (first_student, card) = fixture_ids();
    let (second_student, replacement) = fixture_ids();

    save_admin(&mut tx, &first_student, &card).await;
    save(&mut tx, &second_student, Some(&card), Some(1), Some(1), None, None, true).await;

    // The card-loss path end to end: the row left with a NULL card_number must
    // still accept an assignment, or a re-issue would strand that student.
    let reissued = save_admin(&mut tx, &first_student, &replacement).await;
    assert!(ok(&reissued), "re-issue failed: {reissued}");
    assert_eq!(data(&reissued, "card_number"), replacement.as_str());
    assert_eq!(data(&reissued, "created"), false);
}

// ---------------------------------------------------------------------
// save_card_info — validation
// ---------------------------------------------------------------------

#[tokio::test]
async fn required_fields_are_enforced() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    let cases: Vec<(&str, Value)> = vec![
        (
            "no applicant id",
            save(&mut tx, "", Some(&card), Some(1), Some(1), None, None, false).await,
        ),
        (
            "blank applicant id",
            save(&mut tx, "   ", Some(&card), Some(1), Some(1), None, None, false).await,
        ),
        (
            "no card number",
            save(&mut tx, &applicant, None, Some(1), Some(1), None, None, false).await,
        ),
        (
            "no registration type",
            save(&mut tx, &applicant, Some(&card), Some(1), None, None, None, false).await,
        ),
    ];

    for (label, r) in cases {
        assert!(!ok(&r), "{label} should be rejected");
        assert_eq!(code(&r), "missing_fields", "{label}");
    }
}

#[tokio::test]
async fn the_enums_accept_only_one_and_two_from_callers() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    for bad in [0_i16, 3, -1, 99] {
        let r = save(&mut tx, &applicant, Some(&card), Some(bad), Some(1), None, None, false).await;
        assert_eq!(code(&r), "invalid_value", "is_verified={bad}");

        let r = save(&mut tx, &applicant, Some(&card), Some(1), Some(bad), None, None, false).await;
        assert_eq!(code(&r), "invalid_value", "registration_type={bad}");
    }
}

#[tokio::test]
async fn a_truncated_scan_is_rejected_rather_than_stored() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, _) = fixture_ids();

    // A one- or two-character "UID" is a failed read. Storing it would put a
    // card number on a student that no real card can ever match.
    for short in ["A", "AB", "A:B", "1-2"] {
        let r = save(&mut tx, &applicant, Some(short), Some(1), Some(1), None, None, false).await;
        assert!(!ok(&r), "{short:?} should be rejected");
        assert_eq!(code(&r), "invalid_card_number", "for {short:?}");
    }

    // Four characters is the floor and must pass.
    let r = save(&mut tx, &applicant, Some("ABCD"), Some(1), Some(1), None, None, false).await;
    assert!(ok(&r), "4 chars is valid: {r}");
}

#[tokio::test]
async fn an_over_long_card_number_is_rejected() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, _) = fixture_ids();

    // 65 characters: one past the column width. Caught by the function so the
    // caller gets a message rather than a truncation error.
    let long = "A".repeat(65);
    let r = save(&mut tx, &applicant, Some(&long), Some(1), Some(1), None, None, false).await;
    assert!(!ok(&r));
    assert_eq!(code(&r), "invalid_card_number");
}

#[tokio::test]
async fn an_over_long_applicant_id_is_rejected() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");

    let long = "A".repeat(65);
    let r = save(&mut tx, &long, Some("ABCD1234"), Some(1), Some(1), None, None, false).await;
    assert!(!ok(&r));
    assert_eq!(code(&r), "invalid_value");
}

// ---------------------------------------------------------------------
// Audit trail
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_write_leaves_an_audit_row() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save_admin(&mut tx, &applicant, &card).await;
    let rows = audit_for(&mut tx, &applicant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "created");
    assert!(rows[0].1.contains(&"card_number".to_string()));

    save_admin(&mut tx, &applicant, &card).await;
    let rows = audit_for(&mut tx, &applicant).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, "updated");
    // Nothing actually differed, so the row must not claim a change.
    assert!(
        rows[1].1.is_empty(),
        "a no-op save should record no changed fields, got {:?}",
        rows[1].1
    );
}

#[tokio::test]
async fn a_reassignment_is_audited_on_both_students() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (first_student, card) = fixture_ids();
    let (second_student, _) = fixture_ids();

    save_admin(&mut tx, &first_student, &card).await;
    save(&mut tx, &second_student, Some(&card), Some(1), Some(1), None, None, true).await;

    // "Who had this card before" has to be answerable from the old holder's
    // own history, which is the whole reason the unassigned row exists.
    let old = audit_for(&mut tx, &first_student).await;
    let actions: Vec<&str> = old.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["created", "unassigned"]);

    let new = audit_for(&mut tx, &second_student).await;
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].0, "created");
}

#[tokio::test]
async fn a_rejected_save_writes_no_audit_row() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (first_student, card) = fixture_ids();
    let (second_student, _) = fixture_ids();

    save_admin(&mut tx, &first_student, &card).await;
    let refused = save_admin(&mut tx, &second_student, &card).await;
    assert_eq!(code(&refused), "card_conflict");

    // A conflict changed nothing, so the trail must not imply it did.
    assert!(audit_for(&mut tx, &second_student).await.is_empty());
    assert_eq!(audit_for(&mut tx, &first_student).await.len(), 1);
}

#[tokio::test]
async fn the_audit_records_who_performed_the_write() {
    let pool = db_or_skip!();
    let mut tx = pool.begin().await.expect("begin");
    let (applicant, card) = fixture_ids();

    save_admin(&mut tx, &applicant, &card).await;

    let (performed_by, client_ip): (Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT performed_by, client_ip FROM attendance.nfc_card_audit
          WHERE student_applicant_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(&applicant)
    .fetch_one(&mut *tx)
    .await
    .expect("audit read");

    // The bearer token's `sub`, not the applicant id — that is all the token
    // proves, and the audit must not imply more.
    assert_eq!(performed_by, Some(45320));
    assert_eq!(client_ip.as_deref(), Some("127.0.0.1"));
}
