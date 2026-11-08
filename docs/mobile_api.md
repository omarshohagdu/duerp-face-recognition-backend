# Mobile App API: Selfie Attendance

Everything the employee mobile app needs: sign in, enroll a selfie, choose an office, and mark attendance with a selfie and GPS.

| | |
|---|---|
| Base URL | production `https://attendence.du.ac.bd` · dev `http://<host>:8083` |
| Format | JSON, except enroll and mark, which are `multipart/form-data` |
| Detailed references | [`face_attendance_api.md`](face_attendance_api.md) · [`geo_location_api.md`](geo_location_api.md) |

---

## 1. The flow

```
Sign in ─► Enroll selfie ─► Choose office ─► (admin approves) ─► Mark attendance
 once        once            once per office                      every day
```

| Step | Endpoint | Screen |
|---|---|---|
| 1 | `POST /login` | Login |
| 2 | `POST /ext-api/face-attendance/enroll` | Enroll selfie |
| 3 | `POST /ext-api/geo-location/available` → `POST /ext-api/geo-location/select` | Choose office |
| 4 | `POST /ext-api/geo-location/my` | My offices: wait for **APPROVED** |
| 5 | `POST /ext-api/face-attendance/mark` | Mark attendance |

An administrator approves the office in the web admin panel. The app only waits, and shows the status from step 4.

Suggested start-up logic after login:

```
my = POST /ext-api/geo-location/my
if the user has never enrolled → Enroll selfie screen        (you'll learn this from mark: NOT_ENROLLED)
else if no office in `my` has status APPROVED → My offices screen
else → Mark attendance screen
```

---

## 2. Headers

| Header | `/login` | every `/ext-api/...` call |
|---|---|---|
| `X-App-Id` | – | **required**, the app ID |
| `X-App-Password` | – | **required**, the app password |
| `Authorization` | – | **required**, `Bearer <token>` |
| `Content-Type` | `application/json` | `application/json`, or `multipart/form-data` for enroll and mark (let the HTTP library set the boundary) |

The app ID and password ship inside the app and can be extracted from it, so treat them as identifying the app, not as a secret. The bearer token is what identifies the user.

**Timeouts:** use at least **45 s** for enroll and mark, because the server waits up to 30 s for the face service. A shorter client timeout can report a failure for a check-in that was actually recorded.

---

## 3. Sign in

```
POST /login
Content-Type: application/json
```

```json
{ "username": "rahim@du.ac.bd", "password": "••••••" }
```

**200**

```json
{
  "token": "eyJhbGciOiJIUzI1NiJ9…",
  "username": "rahim@du.ac.bd",
  "user_data": {
    "user_id": 45320,
    "emp_id": "1957103001",
    "user_role": "Officer",
    "name": "Rahim Uddin",
    "…": "…"
  }
}
```

| Store | Use |
|---|---|
| `token` | `Authorization: Bearer <token>` on every call. Valid for **730 hours (~30 days)**; on any `401` "Token has expired", send the user back to Login. |
| `user_data.emp_id` | the **`employee_id`** you send to enroll and mark |
| `user_data.user_role` | `student` means this flow doesn't apply (students don't select offices) |

**Errors:** `401` with a JSON string body: `"Login failed at external API"` (wrong credentials) or `"Invalid user data from external API"`. `500` `"Login service unavailable"`.

Store the token in the platform's secure storage (Keychain / EncryptedSharedPreferences), not plain preferences.

---

## 4. Response envelope

The selfie and office endpoints all answer:

```json
{ "success": true,  "code": "OK", "message": "…", "data": { … } }       // face-attendance
{ "status": "success",            "message": "…", "data": { … } }       // geo-location
{ "status": "failed", "code": "…", "message": "…", "data": { … } }      // geo-location failure
```

- **Branch on `code`.** It's stable. `message` is readable English you may show, but it can change.
- Rejections from the gateway, before your request reaches the endpoint, look different: `{"error": "Invalid App ID or Password"}` (401) or `{"error": "IP address not allowed …"}` (403). A missing or invalid token gives `{"success": false, "message": "Missing or empty token…"}` (401).

---

## 5. Enroll selfie

```
POST /ext-api/face-attendance/enroll
Content-Type: multipart/form-data
```

| Part | Type | Required | Notes |
|---|---|---|---|
| `employee_id` | text | yes | `user_data.emp_id`, 10 digits; must be the signed-in user's own ID |
| `employee_selfie_image` | file | yes | **one** photo, JPEG recommended, ≤ 25 MB (resize to ~1280 px on the long side) |
| `device_info` | text | no | JSON string, e.g. `{"device":"Pixel 7","os":"Android 14","app_version":"1.0.0"}` |

The server asks the face service to check the photo before saving it. A photo without one clear face is refused (`NO_FACE_DETECTED`), so the app must show the camera again. Enrolling again **replaces** the previous selfie.

```bash
curl -X POST "$BASE/ext-api/face-attendance/enroll" \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" -H "Authorization: Bearer $TOKEN" \
  -F employee_id=1957103001 \
  -F "employee_selfie_image=@selfie.jpg;type=image/jpeg" \
  -F 'device_info={"device":"Pixel 7","os":"Android 14","app_version":"1.0.0"}'
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
    "image_check": { "passed": true, "similarity": 1.0 },
    "next_step": "SELECT_OFFICE"
  }
}
```

After success, go to **Choose office** (`next_step: SELECT_OFFICE`).

---

## 6. Offices

The server takes the employee from the token, so none of these calls send an employee ID. Each is a `POST` with an empty JSON body `{}` unless shown otherwise.

### 6.1 Offices to choose from

```
POST /ext-api/geo-location/available
```

```json
{
  "status": "success",
  "message": "OK",
  "data": [
    { "id": 1, "office_id": "440000", "office_name": "Registrar Office", "label": null,
      "latitude": 23.7340, "longitude": 90.3928, "radius_meters": 100 },
    { "id": 4, "office_id": "490010", "office_name": "Office of the Controller of Examinations", "label": "Annex building",
      "latitude": 23.724032, "longitude": 90.400358, "radius_meters": 120 }
  ]
}
```

Show `office_name` (plus ` · label` when set). You can show the point on a map with a `radius_meters` circle.

### 6.2 Request offices

```
POST /ext-api/geo-location/select
{ "geo_location_office_ids": [4] }
```

**201**: the new requests, each `"status": "PENDING"`.

| `code` | HTTP | Show |
|---|---|---|
| `NOT_ENROLLED` | 404 | "Enroll your selfie first", then open Enroll |
| `ALREADY_SELECTED` | 409 | "Already requested" |
| `LOCATION_LIMIT_EXCEEDED` | 422 | "You can hold at most {data.max} offices" |
| `OFFICE_REVOKED` | 409 | "An administrator removed you from this office" |
| `GEO_LOCATION_OFFICE_NOT_FOUND` | 404 | "This office is no longer available"; refresh the list |
| `EMPLOYEE_NOT_FOUND` | 404 | "Only staff can choose an office" |

### 6.3 My offices

```
POST /ext-api/geo-location/my
```

```json
{
  "status": "success",
  "message": "OK",
  "data": [
    { "id": 102, "geo_location_office_id": 4, "office_name": "Office of the Controller of Examinations", "label": null,
      "latitude": 23.724032, "longitude": 90.400358, "radius_meters": 120,
      "status": "APPROVED", "office_status": "ACTIVE",
      "requested_at": "2026-11-08T09:16:00+06:00", "reviewed_at": "2026-11-08T11:02:00+06:00", "review_note": null }
  ]
}
```

| `status` | Show |
|---|---|
| `PENDING` | "Waiting for approval" |
| `APPROVED` | "Approved". If `office_status` is `INACTIVE`, show "Temporarily unavailable". |
| `REJECTED` | "Not approved: {review_note}"; the user may request it again |
| `REVOKED` | "Removed by admin: {review_note}" |

The user can mark attendance once at least one office is `APPROVED` with `office_status = ACTIVE`. Refresh this list when the app returns to the foreground.

### 6.4 Remove / cancel

```
POST /ext-api/geo-location/remove
{ "id": 102 }
```

Works on PENDING (cancel) and APPROVED (remove) entries. **200**: the entry with `"status": "REMOVED"`.

---

## 7. Mark attendance

```
POST /ext-api/face-attendance/mark
Content-Type: multipart/form-data
```

| Part | Type | Required | Notes |
|---|---|---|---|
| `employee_id` | text | yes | `user_data.emp_id` |
| `selfie_image` | file | yes | **one live** photo from the front camera, JPEG |
| `device_info` | text | yes | JSON string with GPS, see below |

```json
{
  "latitude": 23.72410,
  "longitude": 90.40041,
  "accuracy": 8.5,
  "device": "Pixel 7",
  "os": "Android 14",
  "app_version": "1.0.0"
}
```

| Key | Notes |
|---|---|
| `latitude`, `longitude` | required; never send `0,0` (treated as "no fix") |
| `accuracy` | metres, from the OS location API. If sent and worse than **50 m**, the check-in is refused (`GPS_ACCURACY_TOO_LOW`). |

What the server checks, in order:

1. The ID is the user's own.
2. The user has an enrolled selfie.
3. The new selfie matches the enrolled one, judged by the face service.
4. The location is inside an **approved, active** office's radius; the nearest one is used.
5. The attendance is recorded.

```bash
curl -X POST "$BASE/ext-api/face-attendance/mark" \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" -H "Authorization: Bearer $TOKEN" \
  -F employee_id=1957103001 \
  -F "selfie_image=@live.jpg;type=image/jpeg" \
  -F 'device_info={"latitude":23.72410,"longitude":90.40041,"accuracy":8.5,"device":"Pixel 7","os":"Android 14","app_version":"1.0.0"}'
```

**200: marked**

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
      "office_id": "490010",
      "office_name": "Office of the Controller of Examinations",
      "label": null,
      "geo_location_office_id": 4,
      "employee_geo_location_id": 102,
      "distance_meters": 34.6,
      "radius_meters": 120
    }
  }
}
```

Show "Attendance marked · {time of marked_at} · {office_name}".

**403: refused (example)**

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

## 8. Error codes → what the app should do

| `code` | HTTP | Show | Buttons |
|---|---|---|---|
| `FACE_NOT_MATCHED` | 403 | "That doesn't look like your enrolled photo" | Try again · Enroll again |
| `NO_FACE_DETECTED` | 422 | "No clear face in the photo. Face the camera in good light." | Try again |
| `LOW_IMAGE_QUALITY` | 422 | "Photo isn't clear enough" (enroll) | Try again |
| `NOT_ENROLLED` | 404 | "Enroll your selfie first" | Enroll |
| `ENROLLED_IMAGE_MISSING` | 409 | "Your enrolled photo is no longer available" | Enroll again |
| `NO_APPROVED_LOCATION` | 403 | "You don't have an approved office yet" | Choose office |
| `OUT_OF_LOCATION` | 403 | "You're {distance} from {office_name}; you need to be within {radius_meters} m" (from `data.geo_match`) | Try again |
| `GPS_ACCURACY_TOO_LOW` | 403 | "Your location isn't precise enough. Move outdoors and try again." | Try again |
| `LOCATION_REQUIRED` | 400 | "Turn on location" | Settings |
| `INVALID_COORDINATES` | 403 | "Location couldn't be read" | Try again |
| `NOT_YOUR_ID` | 401 | "You can only check in for yourself" | – (app bug: wrong `employee_id`) |
| `EMPLOYEE_NOT_FOUND` | 404 | "No staff record found. Contact HR." | – |
| `FACE_VERIFY_UNAVAILABLE` | 503 | "Face check is unavailable. Try again shortly." | Try again |
| `IMAGE_TOO_LARGE` | 413 | "Photo too large" (resize before upload) | Try again |
| `INVALID_IMAGE` / `INVALID_INPUT` | 400 | "Something went wrong" (app bug) | – |
| `INTERNAL_ERROR` | 500 | "Something went wrong. Try again." | Try again |
| (401, no `code`) | 401 | token missing or expired | Sign in again |

---

## 9. Implementation notes for the app

**Camera**
- Use the **front camera** with a live preview; for mark, don't offer the gallery.
- Encode as **JPEG** (quality ~85), resized to about 1280 px on the long side. iOS HEIC photos are accepted but much larger.
- One face, centred, good light, no mask or sunglasses. A face-outline overlay helps.

**Location**
- Request **precise / fine** location. On Android 12+, the user can grant only approximate location, and that will fail the 50 m limit, so ask for precise.
- Use high accuracy and wait for a fresh fix (`accuracy ≤ 50`) before enabling the shutter. Show "Getting your location…" until then.
- Send the reading taken **at capture time**, not a cached one.
- Never send `0,0`.

**Requests**
- Enroll and mark: multipart with exactly the part names above. Disable the button while a request is in flight, and don't retry automatically: a slow `200` is a recorded attendance.
- Timeout: 45 s or more.
- Handle the HTTP status **and** `code`. A gateway rejection has an `error` field instead of `code`.

**Session**
- Token lasts ~30 days. On a `401` without a `code`, clear the token and show Login.
- `NOT_YOUR_ID` is not a session problem; don't log the user out on it.

---

## 10. Endpoint summary

| Method | Path | Body | Success |
|---|---|---|---|
| POST | `/login` | JSON `username`, `password` | 200 `{token, user_data}` |
| POST | `/ext-api/face-attendance/enroll` | multipart `employee_id`, `employee_selfie_image`, `device_info?` | 201 |
| POST | `/ext-api/geo-location/available` | `{}` | 200 |
| POST | `/ext-api/geo-location/select` | `{"geo_location_office_ids":[…]}` | 201 |
| POST | `/ext-api/geo-location/my` | `{}` | 200 |
| POST | `/ext-api/geo-location/remove` | `{"id":…}` | 200 |
| POST | `/ext-api/face-attendance/mark` | multipart `employee_id`, `selfie_image`, `device_info` (GPS) | 200 |
