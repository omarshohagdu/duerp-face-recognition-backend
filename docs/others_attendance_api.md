# Others Attendance API: acting for a colleague with no phone

Some employees have no mobile phone, so they can't enroll a selfie, choose an office or mark attendance themselves. A colleague does it for them on the colleague's own phone (or the web app).

| | |
|---|---|
| Base URL, headers, envelope, timeouts | same as [`mobile_api.md`](mobile_api.md) §2 and §4 |
| Self-service equivalent | [`mobile_api.md`](mobile_api.md) |
| SQL | `sql/012_face_attendance_others.sql`, `sql/013_geo_location_others.sql` |
| Handlers | `src/routes/face_attendance.rs`, `src/routes/geo_location.rs` |

---

## 1. Menu

| Menu | Web route | What it does |
|---|---|---|
| **Others Selfie Attendance** | `/selfie/others` | enroll an employee's selfie, then mark their attendance |
| **Office Location (Self)** | `/attendance/my-offices` | your own offices (the old "My offices") |
| **Office Location (Others)** | `/attendance/others-offices` | request offices for an employee you enrolled |

All three are for staff only (not students).

## 2. The flow

```
Others Selfie Attendance     Office Location (Others)     (admin approves)     Others Selfie Attendance
  enroll employee   ─────►     request office       ─────►     in web admin  ─────►   mark attendance
  once                         once per office                                         every day
```

Requesting an office is optional. If the employee has no approved office, their check-in is checked against **your** approved offices instead.

## 3. The two IDs

| Param | Meaning |
|---|---|
| `employee_id` | the employee **without** a phone (10 digits) |
| `enrolled_by` | the signed-in user's own `user_data.emp_id`, **always**. Any other value gets `NOT_YOUR_ID` (401). Don't log the user out on it. |

---

## 4. Others Selfie Attendance

### 4.1 Screen

```
Others Selfie Attendance
 ├─ [Employee ID ______]  [Mark attendance] [Enroll]
 └─ Employees I enrolled           (POST /others/list)
     • photo · Name · ID · "Marked today 09:02" / "Not marked yet"   [Mark] [Re-enroll]
```

### 4.2 Enroll an employee

```
POST /ext-api/face-attendance/others/enroll
Content-Type: multipart/form-data
```

| Part | Type | Required | Notes |
|---|---|---|---|
| `employee_id` | text | yes | the employee being enrolled; not your own ID (`USE_SELF_ENROLL`) |
| `employee_selfie_image` | file | yes | one **live** photo of the employee (front or back camera, no gallery) |
| `enrolled_by` | text | yes | your own `emp_id` |
| `device_info` | text | no | as in mobile_api §5 |

```bash
curl -X POST "$BASE/ext-api/face-attendance/others/enroll" \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" -H "Authorization: Bearer $TOKEN" \
  -F employee_id=1957103001 -F enrolled_by=1958129001 \
  -F "employee_selfie_image=@colleague.jpg;type=image/jpeg"
```

**201**

```json
{
  "success": true, "code": "OK", "message": "Employee enrolled. You can now mark their attendance.",
  "data": {
    "employee_id": "1957103001", "enrolled_by": "1958129001",
    "enrollment_id": "5b1c…", "version": 1, "is_reenrollment": false,
    "image_check": { "passed": true, "similarity": 1.0 },
    "next_step": "MARK_ATTENDANCE"
  }
}
```

Rules:

- If the employee **enrolled themselves**, nobody else can replace it: `ALREADY_SELF_ENROLLED`.
- If **another colleague** enrolled them, only that colleague (or the employee) can replace it: `ENROLLED_BY_OTHER`.
- You can re-enroll an employee **you** enrolled. That replaces the photo.
- If the photo matches **your own** enrolled selfie, it is refused: `SAME_AS_ENROLLER`.
- If the employee later gets a phone and enrolls themselves, that replaces yours, and they leave your list and your Office Location (Others).

### 4.3 Mark attendance for an employee

```
POST /ext-api/face-attendance/others/mark
Content-Type: multipart/form-data
```

| Part | Type | Required | Notes |
|---|---|---|---|
| `employee_id` | text | yes | the employee being marked; not your own ID (`USE_SELF_MARK`) |
| `selfie_image` | file | yes | one **live** photo of the employee |
| `enrolled_by` | text | yes | your own `emp_id` (also accepted as `marked_by`) |
| `device_info` | text | yes | JSON with GPS, exactly as mobile_api §7 |

You can mark anyone with an active enrollment, not only people you enrolled. The selfie is still compared with the employee's enrolled photo.

Location is checked against the **employee's** approved offices. If they have none, it's checked against **your** approved offices. `data.geo_match.checked_against` says which one was used: `EMPLOYEE` or `ENROLLER`.

```bash
curl -X POST "$BASE/ext-api/face-attendance/others/mark" \
  -H "X-App-Id: $APP_ID" -H "X-App-Password: $APP_PASSWORD" -H "Authorization: Bearer $TOKEN" \
  -F employee_id=1957103001 -F enrolled_by=1958129001 \
  -F "selfie_image=@live.jpg;type=image/jpeg" \
  -F 'device_info={"latitude":23.72410,"longitude":90.40041,"accuracy":8.5,"device":"Pixel 7","os":"Android 14","app_version":"1.0.0"}'
```

**200**: the same as mobile_api §7, plus `"marked_by": "1958129001"` and `geo_match.checked_against`.

### 4.4 Employees I enrolled

```
POST /ext-api/face-attendance/others/list
{ "enrolled_by": "1958129001" }          // optional; defaults to the token holder
```

```json
{
  "success": true, "code": "OK", "message": "OK",
  "data": {
    "enrolled_by": "1958129001",
    "count": 1,
    "employees": [
      { "employee_id": "1957103001", "employee_name": "Rahim Uddin", "designation": "Office Assistant",
        "office_id": "490010", "enrollment_id": "5b1c…", "version": 1,
        "enrolled_at": "2026-11-10T09:00:00+06:00",
        "image_url": "https://attendence.du.ac.bd/uploads/wow_attendance/enrolled/….jpg",
        "last_marked_at": "2026-11-10T09:02:11+06:00", "last_marked_by": "1958129001", "marked_today": true }
    ]
  }
}
```

This list is also the employee picker for Office Location (Others).

---

## 5. Office Location (Others)

### 5.1 Screen

```
Office Location (Others)
 ├─ Employee: [Rahim Uddin · 1957103001 ▾]     (from /face-attendance/others/list)
 ├─ Rahim Uddin's offices                       (POST /geo-location/others/my)
 │    • Registrar Office   APPROVED   [Remove]
 └─ Add an office                               (POST /geo-location/available)
      ☐ Office of the Controller of Examinations     [Request]
```

You can only act for employees **whose current enrollment you made** (`NOT_ENROLLED_BY_YOU` otherwise). An administrator approves every request in the web admin panel, the same as for self-service. The admin's request history names **you** as the requester, with the note "On behalf of …".

These are JSON `POST`s. The list of offices to choose from is the normal `POST /ext-api/geo-location/available` (mobile_api §6.1).

### 5.2 Their offices

```
POST /ext-api/geo-location/others/my
{ "employee_id": "1957103001", "enrolled_by": "1958129001" }
```

**200**: the same shape and statuses as mobile_api §6.3.

### 5.3 Request offices

```
POST /ext-api/geo-location/others/select
{ "employee_id": "1957103001", "enrolled_by": "1958129001", "geo_location_office_ids": [4] }
```

**201**: the new requests, each `"status": "PENDING"`. The other errors are the same as mobile_api §6.2 (`ALREADY_SELECTED`, `LOCATION_LIMIT_EXCEEDED`, `OFFICE_REVOKED`, `GEO_LOCATION_OFFICE_NOT_FOUND`).

### 5.4 Remove / cancel

```
POST /ext-api/geo-location/others/remove
{ "employee_id": "1957103001", "enrolled_by": "1958129001", "id": 102 }
```

**200**: the entry with `"status": "REMOVED"`.

---

## 6. Error codes

All the codes in mobile_api §8 and §6.2 also apply, worded about the employee rather than "you".

| `code` | HTTP | Where | Show |
|---|---|---|---|
| `NOT_YOUR_ID` | 401 | all | `enrolled_by` is not the signed-in user (app bug); don't log out |
| `ALREADY_SELF_ENROLLED` | 409 | enroll | "This employee enrolled their own selfie" |
| `ENROLLED_BY_OTHER` | 409 | enroll | "Another colleague enrolled this employee" |
| `SAME_AS_ENROLLER` | 422 | enroll | "That photo looks like you. Photograph the employee." |
| `USE_SELF_ENROLL` / `USE_SELF_MARK` | 400 | enroll / mark | "That's your own ID; use Selfie attendance" |
| `NOT_ENROLLED` | 404 | mark, offices | "This employee isn't enrolled yet"; open Enroll |
| `NO_APPROVED_LOCATION` | 403 | mark | "Neither the employee nor you has an approved office"; open Office Location (Others) |
| `NOT_ENROLLED_BY_YOU` | 403 | offices | "You can only manage offices for employees you enrolled" |
| `USE_SELF_OFFICE` | 400 | offices | "That's your own ID; use Office Location (Self)" |
| `EMPLOYEE_NOT_FOUND` | 404 | all | "No staff record found. Check the employee ID." |

---

## 7. Audit trail

| What | Where |
|---|---|
| Who enrolled an employee | `wow_attendance_enrollments.enrolled_by` (NULL = self) |
| Who marked an attendance | `wow_attendance_records.marked_by` (NULL = self) |
| Who requested / removed an office | `employee_geo_location_audit.actor_id` = the colleague, `note` = "On behalf of …" |
| `enrolled_by` that wasn't the token holder, or an enroller's own face | `wow_attendance_token_mismatch_record` |

## 8. Permissions

Permission keys `face.enroll_others` (enroll, list, office location) and `face.mark_others` (mark). Their endpoint rules start in **audit** mode (`enforce = false`), so any signed-in employee can use these features today. To restrict them, grant the keys to a role and turn enforcement on ([`access_control.md`](access_control.md)).

## 9. Endpoint summary

| Method | Path | Body | Success |
|---|---|---|---|
| POST | `/ext-api/face-attendance/others/enroll` | multipart `employee_id`, `employee_selfie_image`, `enrolled_by`, `device_info?` | 201 |
| POST | `/ext-api/face-attendance/others/mark` | multipart `employee_id`, `selfie_image`, `enrolled_by`, `device_info` (GPS) | 200 |
| POST | `/ext-api/face-attendance/others/list` | `{"enrolled_by"?}` | 200 |
| POST | `/ext-api/geo-location/available` | `{}` | 200 |
| POST | `/ext-api/geo-location/others/my` | `{"employee_id","enrolled_by"}` | 200 |
| POST | `/ext-api/geo-location/others/select` | `{"employee_id","enrolled_by","geo_location_office_ids":[…]}` | 201 |
| POST | `/ext-api/geo-location/others/remove` | `{"employee_id","enrolled_by","id"}` | 200 |
