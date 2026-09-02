# WOW Attendance — Postman Guide

Import `docs/wow_attendance.postman_collection.json` into Postman, then set the
collection variables below.

## Collection variables

| Variable           | Example                                  | Notes                                   |
|--------------------|------------------------------------------|-----------------------------------------|
| `base_url`         | `http://localhost:8083`                  | API host                                |
| `app_id`           | *(from your `.env` `EXT_APP_ID`)*        | sent as `X-App-Id` header               |
| `app_password`     | *(from your `.env` `EXT_APP_PASSWORD`)*  | sent as `X-App-Password` header         |
| `token`            | `eyJhbGci...`                            | DU login `access_token` (from `local.duwebadmin.com/api/login`) sent as `Authorization: Bearer <token>` header |
| `student_token`    | `eyJhbGci...`                            | DU `access_token` of a **Student** user — enroll resolves id_type via `getUserInfo` |
| `employee_token`   | `eyJhbGci...`                            | DU `access_token` of an **Employee/faculty** user — enroll resolves id_type via `getUserInfo` |
| `student_id`       | `550e8400-e29b-41d4-a716-446655440000`   | `lms_student.id`                        |
| `employee_emp_id`  | `EMP-1024`                               | `lms_faculty.emp_id`                    |

## Common rules for every request

- **Auth headers (required):** `X-App-Id` and `X-App-Password`. Missing/wrong → `401`.
- **Bearer token (required on every endpoint):** `Authorization: Bearer <jwt>` — the
  DU login `access_token` from `local.duwebadmin.com/api/login` (its `sub` is the DU
  user id). Missing/empty → `401`. It is no longer a `token` form field. Enroll
  **decodes it without verifying the signature** (it is signed with DU's own secret),
  so the `/ext-api` scope is protected only by X-App-Id/password + the IP allow-list.
- **IP allow-list:** the caller IP must be present for that exact endpoint path in
  `ictcell.ext_api_allowed_ips`, else `403`.
- **`id` / `id_type`** may be query params or form fields — **except on `enroll`,
  where `id_type` is resolved from the token's user** (the backend decodes the token
  for `sub`, then calls the DU `getUserInfo` endpoint with that user id) and is not sent.
- **`id_type`** is `Student` or `Employee`. For `Employee`, `id` is the faculty `emp_id`.
- **Image size:** each uploaded image must be ≤ `WOW_MAX_IMAGE_MB` (default 5 MB), else `413`.
- In Postman, set the file rows' **Body → form-data** type to **File** and pick the image(s).

---

## 1. Enroll

`POST {{base_url}}/ext-api/wow-attendance/enroll?id={{student_id}}`

**Header:** `Authorization: Bearer {{student_token}}` (or `{{employee_token}}`) —
the backend resolves `id_type` from the token's user via the DU `getUserInfo`
endpoint; do **not** send `id_type`.

**Body — form-data**

| Key           | Type | Required | Value                                            |
|---------------|------|----------|--------------------------------------------------|
| `id`          | Text | Yes\*    | person id (or query param)                        |
| `device_info` | Text | No       | `{"device":"Android","os":"14"}` (JSON string)   |
| `images`      | File | Yes      | one or more face images (repeat the key per file); each ≤ 5 MB |

\* `id` required — query param or form field.

The enrollment is written to the DB **only after the AI platform confirms
success**. If the AI platform is unset/unreachable/does not confirm, the request
**fails closed** (`502`, saved images deleted, nothing written).

**Response**
```json
{
  "success": true,
  "message": "Enrolled successfully",
  "data": {
    "id": "550e8400-...",
    "id_type": "Student",
    "enrolled_image_count": 2,
    "enrollment_id": "uuid"
  },
  "ai_enrolled": true
}
```

**cURL**
```bash
curl -X POST \
  "http://localhost:8083/ext-api/wow-attendance/enroll?id=550e8400-..." \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" \
  -H "Authorization: Bearer eyJhbGci..." \
  -F 'device_info={"device":"Android","os":"14"}' \
  -F "images=@photo1.jpg" \
  -F "images=@photo2.jpg"
```

---

## 2. Enrolled List

`POST {{base_url}}/ext-api/wow-attendance/enrolled?id_type=Student`

**Header:** `Authorization: Bearer {{token}}` (required).

**Body — form-data**

| Key       | Type | Required | Value         |
|-----------|------|----------|---------------|
| `id_type` | Text | Yes\*    | `Student` / `Employee` (or query param) |
| `page`    | Text | No       | `1` (default) |
| `limit`   | Text | No       | `20` (default)|

\* `id_type` required — query param or form field.

**Response**
```json
{
  "success": true,
  "data": {
    "id_type": "Student",
    "total": 120,
    "page": 1,
    "limit": 20,
    "list": [
      {
        "id": "550e8400-...",
        "name": "John Doe",
        "enrollment_id": "uuid",
        "enrolled_at": "2025-06-01T10:00:00Z",
        "image_count": 3,
        "is_active": true
      }
    ]
  }
}
```

---

## 3. Verify & Mark Attendance

`POST {{base_url}}/ext-api/wow-attendance/verify?id={{student_id}}&id_type=Student`

**Header:** `Authorization: Bearer {{token}}` (required).

**Body — form-data**

| Key           | Type | Required | Value                            |
|---------------|------|----------|----------------------------------|
| `device_info` | Text | No       | `{"device":"iPhone","os":"17"}`  |
| `image`       | File | Yes      | single live capture (≤ 5 MB)     |

**Live image input — flexible:**
- The file field may be named **`image`, `images`, `file`, or `photo`** (any one works).
- Alternatively send the image as a **base64 string in a Text field** with the same
  name (`image` / `images` / `file` / `photo`). A `data:image/jpeg;base64,...`
  data-URL prefix is accepted and stripped automatically.

**Two modes:**

- **1:1 (id supplied):** matches the live image only against that person's enrolled
  images. Use the `?id=...&id_type=...` query params as above.
- **1:N identify (id omitted):** send just the live image (Bearer header still
  required). The AI platform matches it against everyone enrolled and returns the
  person's `identifier` and `id_type`; attendance is marked for that person.
  `POST {{base_url}}/ext-api/wow-attendance/verify`

**Response — match**
```json
{
  "success": true,
  "matched": true,
  "message": "Attendance marked",
  "data": {
    "id": "550e8400-...",
    "id_type": "Student",
    "attendance_id": "uuid",
    "matched_at": "2025-06-15T09:30:00Z",
    "confidence": 0.97
  }
}
```

**Response — no match (1:1)**
```json
{
  "success": true,
  "matched": false,
  "message": "Face did not match enrolled images",
  "data": { "id": "550e8400-...", "id_type": "Student", "confidence": 0.31 }
}
```

**Response — no match (1:N identify, no one matched)**
```json
{
  "success": false,
  "matched": false,
  "message": "No matching enrolled person found",
  "live_image": "./uploads/wow_attendance/live/uuid.jpg"
}
```

> Recognition is delegated to the AI platform at `WOW_AI_BASE_URL` (`POST
> /recognize`). If it is not configured, verify fails with
> `"Face recognition failed: WOW_AI_BASE_URL not configured"`. A successful match
> is recorded with `confidence = 1.0` (the platform returns no numeric score).