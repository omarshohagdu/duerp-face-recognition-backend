# NFC Card Reader API Specification

**System:** University of Dhaka — Attendance/Dashboard App
**Module:** NFC Card ↔ Student Mapping

**Base URL pattern (matching your existing API):** `https://api.atten.du.ac.bd/ext-api/nfc-card/...`

---

## Data Model

### Table: `student_table`

| Field | Type (suggested) | Notes |
|---|---|---|
| id | BIGINT, PK, auto-increment | |
| Student Applicant ID | VARCHAR(64), UNIQUE, NOT NULL | Student's registration/applicant identifier |
| Card Number | VARCHAR(64), UNIQUE, NULLABLE | NFC card UID; null until a card is assigned |
| Card Image | VARCHAR(255), NULLABLE | Path/URL to stored card image |
| Student Selfie | VARCHAR(255), NULLABLE | Path/URL to stored selfie |
| is_verified | TINYINT | `1 = Yes`, `2 = No` |
| registration_type | TINYINT | `1 = Admin`, `2 = Self` |

> Recommend a **unique index on `Card Number`** so the same card can never be assigned to two students at once.

---

## 1. `get_card_info`

Given a scanned card number, returns the associated student's core info.

**Endpoint:** `GET /ext-api/nfc-card/get_card_info`

**Auth:** Required (device/staff token)

### Query Parameters

| Param | Type | Required | Notes |
|---|---|---|---|
| card_number | string | Yes | e.g. `?card_number=04A1B2C3D4E5` |

### Success Response — `200 OK`

```json
{
  "success": true,
  "data": {
    "student_applicant_id": "APP-2026-00123",
    "card_number": "04A1B2C3D4E5",
    "is_verified": 1,
    "registration_type": 1
  }
}
```

### Error Responses

| Status | Case | Body |
|---|---|---|
| 400 | Missing/invalid `card_number` | `{ "success": false, "error": "card_number is required" }` |
| 404 | No student has this `card_number` | `{ "success": false, "error": "Card not recognized" }` |
| 401/403 | Unauthorized device/user | `{ "success": false, "error": "Unauthorized" }` |
| 500 | Server/DB error | `{ "success": false, "error": "Internal server error" }` |

### Business Logic

1. Validate `card_number` is present and well-formed.
2. Normalize it the same way it's normalized on save (e.g. uppercase).
3. `SELECT Student Applicant ID, Card Number, is_verified, registration_type FROM student_table WHERE Card Number = ?`.
4. If no row → `404`.
5. Return the four fields only — no images, no other student PII, keeping the scan-response payload light and fast.

---

## 2. `save_card_info`

Creates/updates a student's card record, including uploaded images.

**Endpoint:** `POST /ext-api/nfc-card/save_card_info`

**Auth:** Required (admin/staff role, or the student's own token for self-registration — see note below)

**Content-Type:** `multipart/form-data` (Card Image and Student Selfie are files)

### Request Body (multipart/form-data fields)

| Field | Type | Required | Notes |
|---|---|---|---|
| student_applicant_id | string | Yes | Must match an existing row in `student_table` |
| card_number | string | Yes | Normalize casing/format before storing |
| is_verified | integer (1 or 2) | Optional, default `2` | `1 = Yes`, `2 = No` |
| registration_type | integer (1 or 2) | Yes | `1 = Admin`, `2 = Self` |
| card_image | file (jpg/png) | Optional | Photo of the physical card |
| student_selfie | file (jpg/png) | Optional | Student's selfie/photo |

> If `registration_type = 2` (Self), consider forcing `is_verified = 2` server-side regardless of what's passed in, so self-registrations always require admin review before becoming trusted.

### Success Response — `200 OK`

```json
{
  "success": true,
  "message": "Card info saved successfully",
  "data": {
    "student_applicant_id": "APP-2026-00123",
    "card_number": "04A1B2C3D4E5",
    "is_verified": 2,
    "registration_type": 2,
    "card_image": "https://.../cards/APP-2026-00123.jpg",
    "student_selfie": "https://.../selfies/APP-2026-00123.jpg"
  }
}
```

### Error Responses

| Status | Case | Body |
|---|---|---|
| 400 | Missing required fields (`student_applicant_id`, `card_number`, `registration_type`) | `{ "success": false, "error": "student_applicant_id, card_number, and registration_type are required" }` |
| 400 | Invalid `is_verified`/`registration_type` value (not 1 or 2) | `{ "success": false, "error": "is_verified/registration_type must be 1 or 2" }` |
| 400 | Invalid file type/size for `card_image` or `student_selfie` | `{ "success": false, "error": "Invalid image file" }` |
| 404 | `student_applicant_id` not found | `{ "success": false, "error": "Student not found" }` |
| 409 | `card_number` already assigned to a **different** student | `{ "success": false, "error": "Card already assigned to another student" }` |
| 401/403 | Not authenticated / not authorized | `{ "success": false, "error": "Unauthorized" }` |
| 500 | Server/DB/file-storage error | `{ "success": false, "error": "Internal server error" }` |

### Business Logic

1. Validate required fields and that `is_verified` / `registration_type` are each `1` or `2`.
2. Look up the student by `Student Applicant ID`. If not found → `404`.
3. Check `card_number` uniqueness:
   - Same student already has this card → update idempotently.
   - A different student holds this card → reject with `409`.
4. If `card_image` / `student_selfie` are provided, upload to storage (S3/local disk) and store the resulting URL/path.
5. Update the row: `Card Number`, `is_verified`, `registration_type`, `Card Image`, `Student Selfie`.
6. Log the action (who saved it, timestamp, fields changed) for audit purposes.

---

## Endpoint Summary

| # | Method | Path | Purpose |
|---|---|---|---|
| 1 | GET | `/ext-api/nfc-card/get_card_info?card_number=...` | Scan lookup → returns applicant ID, card number, verification & registration type |
| 2 | POST | `/ext-api/nfc-card/save_card_info` | Save/update full card record, including images |

---

## Notes / Open Questions

- **Card number format:** confirm what your NFC scanner returns (hex UID, decimal, formatted string) and normalize consistently on both save and lookup.
- **Re-issuing cards:** decide whether re-mapping a card to a new student should overwrite silently, require a `force_reassign` flag, or be blocked entirely until an admin clears the old assignment.
- **Unverified students (`is_verified = 2`):** decide if `get_card_info` should still return data for unverified students, or return a `403`/flag instead.
- **Self vs Admin (`registration_type`):** confirm whether self-service saves need to go through `save_card_info` with a restricted token (only allowed to set their own `student_applicant_id`, and forced `is_verified = 2`).
- **Attendance linkage:** clarify whether `get_card_info` should also log an attendance check-in, or if that's a separate downstream call.
- **Image storage:** confirm storage backend (S3, GCS, local disk) and max file size/type constraints for `card_image` and `student_selfie`.
