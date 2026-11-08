# Selfie Enrollment & Selfie Attendance API

A second way to enroll and check in. Instead of the AI platform identifying faces (`/wow-attendance/enroll` + `/verify`), each employee enrolls **one selfie**. At check-in, the face-verify service compares the new selfie **1:1** with that enrolled selfie, and the device must be inside one of the employee's **approved offices**.

| | |
|---|---|
| Source | `src/routes/face_attendance.rs` |
| SQL | `sql/011_face_verify_attendance.sql` (needs `010`) |
| Face service | `NFC_FACE_VERIFY_URL` (e.g. `http://10.224.224.101:8089/verify`), key `NFC_FACE_VERIFY_API_KEY`. The URL on the admin settings screen wins over `.env`; the NFC ON/OFF switch does **not** apply here. |
| Web app | menu **Selfie attendance → Enroll selfie / Mark attendance (selfie)** |
| Office approval | [`geo_location_api.md`](geo_location_api.md) |

Every call needs `X-App-Id`, `X-App-Password` and `Authorization: Bearer <token>` (from `POST /login`). Both endpoints are **self only**: `employee_id` must belong to the token holder.

Response envelope: `{ "success": bool, "code": "...", "message": "...", "data": {...} }`. Branch on `code`.

---

## 1. Enroll

```
POST /ext-api/face-attendance/enroll        multipart/form-data
```

| Field | Required | Notes |
|---|---|---|
| `employee_id` | yes | 10-digit employee ID (also accepted as `?employee_id=`) |
| `employee_selfie_image` | yes | **one** image. `selfie_image`, `image` and the spelling `Employee_selife_image` are accepted too. JPEG/PNG/WEBP/BMP/HEIC, up to 25 MB. |
| `device_info` | no | JSON string |

**What happens**

1. The token must own `employee_id`, and the employee must exist in `attendance.employees`.
2. **Image check:** the selfie is sent to the face-verify service as both `card_image` and `selfie_image`. The service refuses a photo without a clear face, so that photo is never enrolled.
3. The enrollment is saved as the employee's new active enrollment in `wow_attendance_enrollments` / `wow_attendance_images`, with `device_info.enroll_method = "FACE_VERIFY"`. Any previous enrollment is retired and kept for history.

```bash
curl -X POST http://localhost:8083/ext-api/face-attendance/enroll \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" -H "Authorization: Bearer $TOKEN" \
  -F employee_id=1957103001 \
  -F employee_selfie_image=@selfie.jpg
```

**201**

```json
{
  "success": true,
  "code": "OK",
  "message": "Enrolled. Choose your office to start marking attendance.",
  "data": {
    "employee_id": "1957103001",
    "enrollment_id": "5b1c…",
    "version": 1,
    "is_reenrollment": false,
    "previous_enrollment_id": null,
    "image_check": { "passed": true, "similarity": 1.0 },
    "next_step": "SELECT_OFFICE"
  }
}
```

---

## 2. Mark attendance

```
POST /ext-api/face-attendance/mark          multipart/form-data
```

| Field | Required | Notes |
|---|---|---|
| `employee_id` | yes | 10-digit employee ID |
| `selfie_image` | yes | **one** live photo (same aliases as enroll) |
| `device_info` | yes | JSON string with GPS: `{"latitude":23.7241,"longitude":90.4004,"accuracy":12}`. The key spellings `lat`/`lng`/`device_lat`/`device_long` and `accuracy_meters` also work. |

**Checks, in order**

| # | Check | Fails with |
|---|---|---|
| 1 | Token owns `employee_id`; staff record exists | `401 NOT_YOUR_ID` · `404 EMPLOYEE_NOT_FOUND` |
| 2 | GPS present | `400 LOCATION_REQUIRED` |
| 3 | Active enrollment with an image on disk (newest image of the active enrollment, whichever way it was enrolled) | `404 NOT_ENROLLED` · `409 ENROLLED_IMAGE_MISSING` |
| 4 | **Face:** face-verify service, `card_image` = enrolled image, `selfie_image` = new selfie, needs `match: true` | `403 FACE_NOT_MATCHED` · `422 NO_FACE_DETECTED` · `503 FACE_VERIFY_UNAVAILABLE` |
| 5 | **Location:** inside one of the employee's **APPROVED + ACTIVE** offices. Always this rule here, whatever `GEO_LOCATION_MODE` says. | `403 NO_APPROVED_LOCATION` · `OUT_OF_LOCATION` · `GPS_ACCURACY_TOO_LOW` |
| 6 | Record attendance | — |

The attendance is saved in `wow_attendance_records` with `verify_method = 'FACE_VERIFY'`, `face_similarity`, `confidence` (= similarity), the matched office, the distance and the coordinates. Failures at steps 4–5 are logged in `geo_attendance_attempt_log`.

```bash
curl -X POST http://localhost:8083/ext-api/face-attendance/mark \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" -H "Authorization: Bearer $TOKEN" \
  -F employee_id=1957103001 \
  -F selfie_image=@live.jpg \
  -F 'device_info={"latitude":23.72410,"longitude":90.40041,"accuracy":12}'
```

**200**

```json
{
  "success": true,
  "code": "OK",
  "message": "Attendance marked",
  "data": {
    "employee_id": "1957103001",
    "attendance_id": "8b0f…",
    "marked_at": "2026-11-08T09:02:11+06:00",
    "face_match": { "matched": true, "similarity": 0.905, "threshold": 0.36 },
    "geo_match": {
      "source": "APPROVED_OFFICE",
      "geo_location_office_id": 4,
      "employee_geo_location_id": 102,
      "office_id": "490010",
      "office_name": "Office of the Controller of Examinations",
      "label": null,
      "distance_meters": 34.6,
      "radius_meters": 120
    }
  }
}
```

**403, example**

```json
{
  "success": false,
  "code": "OUT_OF_LOCATION",
  "message": "You are not inside any of your approved offices",
  "data": {
    "face_match": { "matched": true, "similarity": 0.88, "threshold": 0.36 },
    "geo_match": { "office_name": "Office of the Controller of Examinations", "distance_meters": 2410.3, "radius_meters": 120 }
  }
}
```

---

## 3. Error codes

| HTTP | `code` | Meaning |
|---|---|---|
| 400 | `INVALID_INPUT` | missing `employee_id` or image, bad multipart |
| 400 | `INVALID_IMAGE` | the file isn't an image |
| 400 | `LOCATION_REQUIRED` | mark without GPS |
| 401 | `NOT_YOUR_ID` | `employee_id` isn't the token holder's (recorded in `wow_attendance_token_mismatch_record`) |
| 403 | `FACE_NOT_MATCHED` | selfie doesn't match the enrolled image; `data.face_match` has the score |
| 403 | `NO_APPROVED_LOCATION` · `OUT_OF_LOCATION` · `GPS_ACCURACY_TOO_LOW` · `INVALID_COORDINATES` | location refused; `data.geo_match` explains |
| 404 | `EMPLOYEE_NOT_FOUND` | no staff record |
| 404 | `NOT_ENROLLED` | no active enrollment |
| 409 | `ENROLLED_IMAGE_MISSING` | the enrolled photo file is gone; enroll again |
| 413 | `IMAGE_TOO_LARGE` | over `WOW_MAX_UPLOAD_MB` (25 MB) |
| 422 | `NO_FACE_DETECTED` | the service found no clear face; `data.detail` has its words |
| 422 | `LOW_IMAGE_QUALITY` | enroll: the photo didn't even match itself |
| 503 | `FACE_VERIFY_UNAVAILABLE` | URL not set, service down, or API key rejected |
| 500 | `INTERNAL_ERROR` | database error |

---

## 4. Setup

1. `psql "$DATABASE_URL" -f sql/011_face_verify_attendance.sql`
2. Make sure `NFC_FACE_VERIFY_URL` and `NFC_FACE_VERIFY_API_KEY` are set, or set the URL on the admin face-verification settings screen.
3. Restart the service.
4. Employee: **Enroll selfie** → **My offices** (request an office) → admin approves → **Mark attendance (selfie)**.
