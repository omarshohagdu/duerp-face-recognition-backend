# Attendance location verification — implementation notes

**Status:** database applied to `lms_dev`; Rust code compiles but is **not deployed**.
**Date:** 2026-07-20 (updated 2026-07-21)
**Schema:** `ictcell` | **Engine:** PostgreSQL

This documents what was actually built for the WOW-attendance location gate and the
related `wow_verify` / `wow_enroll` changes made alongside it. It supersedes
`wow_attendance_location_verification.md` / `.rs` in this folder, which describe an
earlier MySQL design — see [Deviations](#1-deviations-from-the-original-spec) for
why each part changed. The original spec is kept as the requirement of record.

**SQL to apply (two files, both additive):**

```
psql "$DATABASE_URL" -f duerp-attendance/sql/002_location_verify.sql   # tables + functions for the location gate
psql "$DATABASE_URL" -f duerp-attendance/sql/003_token_mismatch.sql    # audit table for ownership mismatches
```

---

## Table of Contents

1. [Deviations from the original spec](#1-deviations-from-the-original-spec)
2. [Schema](#2-schema)
3. [The location gate in POST /verify](#3-the-location-gate-in-post-verify)
4. [Admin: POST /mapping-save](#4-admin-post-mapping-save)
5. [Recognition responses & mismatch auditing](#5-recognition-responses--mismatch-auditing)
6. [Directories, logging & static serving](#6-directories-logging--static-serving)
7. [Files changed](#7-files-changed)
8. [Deployment checklist](#8-deployment-checklist)
9. [Current state](#9-current-state)

---

## 1. Deviations from the original spec

| Spec said | Built instead | Why |
|---|---|---|
| MySQL `CALL wow_attendance_location_verification(?,?,?)` | Postgres `SELECT ictcell.wow_attendance_location_verify($1,$2,$3)` | The whole `wow_attendance` module runs on `PgPool`. The MySQL pool is commented out in `main.rs`, points at a different database (`ict_mis`), and `ictcell.employees` lives in Postgres. |
| `sqlx::query_scalar` over a `CALL` | Plain `SELECT fn(...)` | `CALL` returns a multi-result-set in MySQL; `fetch_one` leaves the pooled connection mid-stream, surfacing later as `PacketOutOfOrder` on an unrelated query. |
| New endpoint `POST /wow-attendance/verify` | Gate folded **into** the existing `POST /ext-api/wow-attendance/verify` | That path already exists (`wow_verify`, face recognition). Two handlers on one path means the second never runs. |
| `employees.office` → `body_id INT` | `body_code varchar(50)` | `employees.office` holds `ictcell.body.body_code` (`'490010'`), **not** `body.body_id` (`'OES'`). Codes are zero-paddable, so an int column would break the join. |
| Procedure returns raw `@msg` on `SQLEXCEPTION` | DB errors logged; caller gets a flat `"Internal server error"` | The spec's handler returned the raw message to the client, leaking schema internals. |
| No auth | Bearer token + ownership check; admin writes need `X-Admin-Key` | The spec's handler read `emp_id` straight from the body, letting any caller probe which building any employee is mapped to. |
| `radius numeric` parameter | `radius double precision` | The handler binds an `f64` → `float8`, and Postgres only has an **assignment** cast `float8 → numeric`. A `numeric` parameter fails resolution at runtime with *"function does not exist"* — invisible to `cargo check`, caught only against a real database. |

The Haversine formula itself was correct and is unchanged in substance; it now uses
`radians()` instead of `PI()/360`, and wraps the `asin` argument in `least(1, ...)`
so a device sitting on a building's exact coordinates cannot push the argument
above 1 through floating-point error and raise a math error.

---

## 2. Schema

Defined in `duerp-attendance/sql/002_location_verify.sql`. The file is idempotent —
safe to re-run on a fresh database or on one that already has an earlier cut.

```
ictcell.buildings
  id          serial PK
  name        varchar(256)
  status      varchar(32)   -- 'Active' required for a mapping to count
  created_at  timestamptz

ictcell.body_building_mapping
  id          serial PK
  body_code   varchar(50)   -- joins ictcell.employees.office
  building_id int  FK -> buildings(id)
  lat         double precision   -- NULL = not surveyed yet; row is ignored
  "long"      double precision
  radius      numeric(10,2) NOT NULL DEFAULT 50   -- metres
  is_active   boolean NOT NULL DEFAULT true
  created_at, updated_at timestamptz
  UNIQUE (body_code, building_id)
  CHECK lat in [-90,90], long in [-180,180], radius > 0
```

A mapping is only considered when `is_active`, the building is `'Active'`, **and**
both coordinates are non-NULL. Any of those being false makes the row inert — which
is what lets a mapping be staged before its GPS is known.

### Functions

| Function | Purpose |
|---|---|
| `ictcell.wow_attendance_location_verify(p_emp_id varchar, p_device_lat float8, p_device_long float8) → jsonb` | Closest active mapped building for the employee's office; `verified` = inside its radius. |
| `ictcell.wow_attendance_body_building_mapping_save(p_body_code varchar, p_building_id int, p_building_name varchar, p_lat float8, p_long float8, p_radius float8, p_is_active boolean) → jsonb` | Upsert one mapping on `(body_code, building_id)`. |

`wow_attendance_location_verify` returns:

```json
{
  "success": true,
  "verified": true,
  "message": "Location verified",
  "data": {
    "emp_id": "1961064001",
    "emp_name": "Md. Jasim Uddin",
    "body_code": "490010",
    "building_id": 1,
    "building_name": "Estate Office",
    "distance_m": 17.79,
    "radius_m": 50
  }
}
```

Non-verified outcomes, each with `verified: false` and a distinct `message`:
`Invalid device coordinates`, `Employee not found`, `Employee has no office
assigned`, `No building mapping found for this employee office`, `Device location
does not match any mapped building`.

---

## 3. The location gate in POST /verify

**Endpoint:** `POST /ext-api/wow-attendance/verify` (unchanged path, unchanged
multipart contract except for the coordinates below).

The gate sits in `wow_verify` **after** face recognition and the enrollment check,
**before** attendance is recorded. That placement is deliberate: it geofences
`recog_identifier` — the person the AI actually identified — rather than the
optional, client-supplied `id` form field, which a caller could set to someone
else's employee id to pick a favourable geofence.

```
POST /ext-api/wow-attendance/verify   (multipart: images, device_info, id?, id_type?)
        │
        ├─ bearer token → user_from_token
        ├─ AI /recognize → recog_identifier
        ├─ du_identify → ownership + id_type
        ├─ enrollment lookup
        │
        ├─ LOCATION GATE ─────────────────────────────
        │    id_type == "Employee" ?
        │      no  → skip (students have no employees.office row)
        │      yes → coords from device_info JSON
        │              missing/unusable → 400
        │              ictcell.wow_attendance_location_verify(...)
        │                verified false → 403, no attendance recorded
        │                verified true  → continue
        │
        └─ ictcell.wow_attendance_verify(...)  → 200 + "location" block
```

### Coordinates

Read from the **existing `device_info` JSON multipart field** — no new form fields.
`device_coords()` accepts a number or a numeric string, and these key spellings:

- latitude: `device_lat`, `lat`, `latitude`
- longitude: `device_long`, `long`, `lng`, `longitude`

```json
{ "device_lat": 23.72815, "device_long": 90.39925 }
```

A `0, 0` reading is rejected as "no GPS fix" rather than treated as a position off
West Africa.

### Behaviour

- **Employees fail closed.** Missing or unusable coordinates return **400**, not a
  skipped check — a geofence that can be bypassed by omitting a field is not a
  geofence.
- **Students are skipped** and logged. The mapping hangs off `employees.office`,
  which students have no row for. They are still recognized and recorded.
- **Rejection returns 403** with the distance/radius detail and records **no**
  attendance.
- **Success** merges the building detail into the response as `location`.

Rejected response:

```json
{
  "success": false,
  "matched": true,
  "verified": false,
  "message": "Device location does not match any mapped building",
  "recognized_identifier": "1961064001",
  "location": { "body_code": "490010", "distance_m": 312.7, "radius_m": 50, "...": "..." },
  "live_image": "./uploads/wow_attendance/live/....jpg"
}
```

---

## 4. Admin: POST /mapping-save

**Endpoint:** `POST /ext-api/wow-attendance/mapping-save`
**Content-Type:** `application/json`

Sets the GPS position and radius the gate checks against, so this endpoint defines
the geofence for every employee in an office.

### Authorization

Three layers: `ExtAuthMiddleware` (app id/password + IP allow-list) → bearer token
→ `X-Admin-Key` header matching the `WOW_ADMIN_KEY` env var.

`Claims` carries only `sub` and `exp` — there is no role in the token — so admin
cannot be established from the token alone. The shared key is the interim gate.
Notes on it:

- **Fails closed:** with `WOW_ADMIN_KEY` unset the endpoint returns **503**. An
  unset variable never means "allow everyone".
- **Constant-time compare**, so a wrong key can't be recovered by timing.
- It identifies *"someone holding the key"*, not a person. Each write logs the
  token's `sub` so a geofence change still traces back to a login.
- **Replace with a role claim** once the token carries one. `auth.rs` already reads
  `user_role` from DU's login response but does not put it in the JWT. Adding a
  required field to `Claims` breaks every token issued before the change, and
  `ACCESS_TOKEN_EXPIRY` is one month — so that migration needs its own plan.

### Request

| Field | Type | Required | Description |
|---|---|---|---|
| `body_code` | string | ✅ | `ictcell.body.body_code`, e.g. `"490010"` — what `employees.office` holds |
| `building_id` | int | — | Target an existing building |
| `building_name` | string | — | Used when `building_id` is omitted: find-or-create by name |
| `lat` | float | ✅ | Building latitude |
| `long` | float | ✅ | Building longitude |
| `radius` | float | — | Metres; defaults to 50 |
| `is_active` | bool | — | Defaults to true |

One of `building_id` / `building_name` is required. The name path is what
bootstraps the initially-empty `buildings` table.

Upserts on `(body_code, building_id)` — calling twice with the same pair edits the
existing mapping rather than duplicating it.

```bash
curl -X POST https://<host>/ext-api/wow-attendance/mapping-save \
  -H "Authorization: Bearer <token>" \
  -H "X-Admin-Key: $WOW_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"body_code":"490010","building_name":"Estate Office",
       "lat":23.72815,"long":90.39925,"radius":50,"is_active":true}'
```

### Response

`200` on success, `400` on a validation rejection, `403` on a bad admin key, `503`
when `WOW_ADMIN_KEY` is unset.

```json
{
  "success": true,
  "message": "Mapping created",
  "data": {
    "mapping_id": 3, "body_code": "490010",
    "building_id": 1, "building_name": "Estate Office", "building_created": true,
    "lat": 23.72815, "long": 90.39925, "radius": 50.00, "is_active": true,
    "employee_count": 308
  },
  "warnings": []
}
```

`warnings` is non-blocking and reports two mistakes worth catching early:

- a `body_code` matching **zero** employees (almost always a typo — the mapping
  saves but can never verify anyone)
- a radius **below 20m** (GPS hardware alone drifts 3–50m, so it will reject valid
  check-ins)

### Radius guidance

| Situation | Radius |
|---|---|
| Normal department building | 50m (default) |
| Small room / lab | 30m |
| Large campus area | 100m |
| Indoor only | 20m — ⚠️ at the edge of GPS drift |

---

## 5. Recognition responses & mismatch auditing

Three changes to `wow_verify` / `wow_enroll` responses, made alongside the location
gate.

### `reason` on a no-match

Both no-match branches of `wow_verify` now carry a `reason` field with the AI
platform's own text, so a caller can tell an unenrolled face from an undetected
one. `message` stays the stable generic string; `reason` is the detail.

```json
{
  "success": false, "matched": false,
  "message": "No matching enrolled person found",
  "reason": "Face not recognized",
  "live_image": "…"
}
```

Source: the recognize error text on the `Err` path; the platform's `error`/`message`
field (fallback `"Face not recognized"`) on the 200-but-no-identifier path.

### Ownership-mismatch message

When the person the request acts on is not the token holder, the message now names
both ids, in the response **and** the step log:

| Flow | Message |
|---|---|
| Verify | `Person identified but mismatch. face recognized as identifier=<X> but Logged in User id=<Y>` |
| Enroll | `Person mismatch. enroll id=<X> but Logged in User id=<Y>` |

The wording differs on purpose: verify's `<X>` is an AI-recognized face; enroll's is
the id from the request (enroll runs no face recognition). Both responses also
expose the two ids as separate fields — verify: `recognized_identifier` +
`logged_in_user_id`; enroll: `enroll_id` + `logged_in_user_id`.

### Mismatch audit table

Every such rejection is also written to `ictcell.wow_attendance_token_mismatch_record`
(defined in `duerp-attendance/sql/003_token_mismatch.sql`).

| Column | Meaning |
|---|---|
| `id` | serial PK |
| `action` | `Enroll` \| `Verify` |
| `ai_recognized_id` | Verify: the identifier the AI recognized; Enroll: the id being enrolled |
| `requested_user_id` | the logged-in / token holder |
| `ai_requested_id` | AI platform's `request_id` (nullable) |
| `ai_similarity` | AI recognition similarity score (nullable) |
| `created_at` | timestamp |

`ai_requested_id` and `ai_similarity` come from the AI `/recognize` response
(`request_id` / `similarity` keys) and are stored when present. They are **always
NULL for Enroll**, which rejects on the ownership check *before* any AI call — and
NULL for Verify until the AI platform actually returns those keys (not in its
response yet at time of writing; the code reads them defensively).

Writing the record is **best-effort**: `record_token_mismatch()` logs a warning and
moves on if the insert fails (e.g. table not yet migrated), so the request still
returns its 401 and attendance flow is never blocked.

> If the AI platform names those fields differently (e.g. `req_id` / `score`),
> update the `recog.get("request_id")` / `recog.get("similarity")` keys in
> `wow_verify`.

---

## 6. Directories, logging & static serving

Three directory paths are configurable via env vars. All default to **relative**
paths (`./…`) resolved against the process working directory — which under
systemd/Docker is **not** the repo root, so on a server the defaults silently
write to the wrong place or fail with `Permission denied`. **Set all three to
absolute paths on every server.**

| Env var | Purpose | Default | Notes |
|---|---|---|---|
| `WOW_UPLOAD_DIR` | Where face images are **written** | `./uploads/wow_attendance` | Enroll/live images land here. |
| `WOW_UPLOADS_SERVE_DIR` | Root the `/uploads` URL is **served** from | `./uploads` | Must be the **parent** of `WOW_UPLOAD_DIR` — the `/uploads` prefix supplies the rest. |
| `WOW_LOG_DIR` | Per-call step logs | `./docs/log` | See below — now placed under uploads. |

### Log location & the HTTP block

`WOW_LOG_DIR` is set to **`<uploads>/log`** (e.g. `…/uploads/log`), not the old
`docs/log`. Reason: on the Docker server the uploads folder is already
bind-mounted to the host, so putting logs there makes them visible on the host
over the same mount — no separate volume needed. This was the fix for "logs don't
appear on the server": the earlier `docs/log` path was inside the container only.

**Security consequence, handled:** that path sits inside the folder the `/uploads`
static route serves (`Files::new("/uploads", …)` with directory listing on). The
step logs carry bearer-token user ids, client IPs and employee ids. So `main.rs`
registers a block **before** the Files service:

```rust
web::scope("/uploads/log")
    .default_service(web::route().to(|| async { HttpResponse::NotFound().finish() }))
```

Registered first, it shadows the static route: `/uploads/log`, `/uploads/log/`,
and `/uploads/log/<file>` all return 404, while other uploads serve normally.
Verified by `tests/uploads_log_block.rs`.

> ⚠️ If `WOW_LOG_DIR` is ever moved **out** of the served uploads folder, that is
> fine for exposure (nothing serves it) — but if it is moved to a *different*
> subfolder of uploads, the `/uploads/log` block no longer covers it. Re-check.

### Path values per environment

The `.env` file is **gitignored** — it does not deploy with the code. Each server
has its own, so these values are per-machine:

| | Dev box (this repo) | Docker server (in-container) |
|---|---|---|
| `WOW_UPLOAD_DIR` | `/var/www/Rust/duerp/duerp-attendance/uploads/wow_attendance` | `<container-path>/uploads/wow_attendance` |
| `WOW_UPLOADS_SERVE_DIR` | `/var/www/Rust/duerp/duerp-attendance/uploads` | `<container-path>/uploads` |
| `WOW_LOG_DIR` | `/var/www/Rust/duerp/duerp-api/uploads/log` | `<container-path>/uploads/log` |

> The first two dev-box values moved from `duerp-api/uploads` to
> `duerp-attendance/uploads` on 2026-08-19 — face captures now live with the
> service that writes them. `WOW_LOG_DIR` deliberately did not move; see
> [`DEPLOYMENT.md`](../../DEPLOYMENT.md#configuration).

`<container-path>` is whatever the app folder is mounted to **inside** the
container — find it with `docker inspect <c> --format '{{range .Mounts}}{{.Source}} -> {{.Destination}}{{println}}{{end}}'`.
The dir must be writable by the container's process user, and env changes need a
container **recreate** (`docker compose up -d`), not just `restart`.

---

## 7. Files changed

> Paths below are as they were at the time: attendance still lived inside
> `duerp-api`. It has since been split into the `duerp-attendance` crate — the
> Rust files kept the same relative paths under it, and the SQL landed in
> `duerp-attendance/sql/`. See [`../../MIGRATION.md`](../../MIGRATION.md) for the
> full mapping. `duerp-api` keeps only the `/uploads` static mount and its
> `/uploads/log` block, because both services share that folder.

| File | Change |
|---|---|
| `duerp-attendance/sql/002_location_verify.sql` | **New.** Tables, index, both functions, and an idempotent `body_id → body_code` rename migration. |
| `duerp-attendance/sql/003_token_mismatch.sql` | **New.** `wow_attendance_token_mismatch_record` audit table + indexes. |
| `duerp-api/src/routes/wow_attendance.rs` | `device_coords()`; location gate inside `wow_verify`; `reason` on both no-match responses; named ownership-mismatch messages (verify + enroll) with id fields; `record_token_mismatch()` on both mismatch paths; `require_admin_key()`; `MappingSaveRequest` + `wow_mapping_save`; `device_coords` unit tests. |
| `duerp-api/src/main.rs` | Registered `wow_mapping_save`; made the `/uploads` serve dir configurable (`WOW_UPLOADS_SERVE_DIR`); added the `/uploads/log` HTTP block. |
| `duerp-api/src/utils/step_logger.rs` | Log file name uses a readable 12-hour timestamp (`…_01_04_39_PM.log`). |
| `duerp-api/tests/location_verify_sql.rs` | **New.** 16 DB-backed SQL function tests. |
| `duerp-api/tests/uploads_log_block.rs` | **New.** Proves the `/uploads/log` block shadows the static route. |
| `duerp-api/Cargo.toml` | Added `[dev-dependencies]` (tokio test features). |
| `duerp-api/.env` | Absolute `WOW_UPLOAD_DIR` / `WOW_UPLOADS_SERVE_DIR` / `WOW_LOG_DIR` (log under uploads). |

---

## 8. Deployment checklist

The database change is **inert on its own** — the running API does not have the new
handler, so live traffic is unaffected. The behaviour change lands when the Rust
side deploys.

**⚠️ On deploy, every employee check-in through `/verify` returns 403 until offices
have active mappings with real coordinates.** That is the fail-closed design
working, but it means seeding must land *with* the deploy, not after it.

1. Apply **both** SQL files (already done on `lms_dev`):
   ```
   psql "$DATABASE_URL" -f duerp-attendance/sql/002_location_verify.sql
   psql "$DATABASE_URL" -f duerp-attendance/sql/003_token_mismatch.sql
   ```
   Both are additive and idempotent. If the mismatch table is missing, ownership
   rejections still work — the audit insert just logs a warning and is skipped.
2. Set `WOW_ADMIN_KEY` in the server `.env` — until then `/mapping-save` returns 503.
3. Set the three directory paths in the server `.env` to **absolute, in-container**
   values (§5), create them, and make them writable by the container's process
   user. Env changes need a container **recreate**, not just `restart`.
   ```
   WOW_UPLOAD_DIR=<container-path>/uploads/wow_attendance
   WOW_UPLOADS_SERVE_DIR=<container-path>/uploads
   WOW_LOG_DIR=<container-path>/uploads/log
   ```
4. Seed a mapping for every office code with staff checking in. Ranked by
   headcount so the ones that matter come first:
   ```
   psql "$DATABASE_URL" -c "select office, count(*) from ictcell.employees
                            group by office order by 2 desc;"
   ```
   Office codes resolve to names via `ictcell.body` (`body_code` → `name`).
5. Confirm the mobile app sends `device_lat` / `device_long` inside `device_info`.
   **Employee check-ins 400 without them.**
6. Deploy the API.
7. Confirm logs write **and** are not web-readable: after one request, the file
   appears under `<uploads>/log` on the host, but `GET /uploads/log/` returns 404.

---

## 9. Current state

Applied to `lms_dev` (`10.224.224.211`):

- Both tables, the partial index, and both functions exist.
- `ictcell.buildings` — 1 row: `Estate Office`.
- `ictcell.body_building_mapping` — 1 row, **staged and inert**:

  | id | body_code | building | lat | long | radius | is_active |
  |---|---|---|---|---|---|---|
  | 3 | 490010 | Estate Office | `NULL` | `NULL` | 50.00 | `false` |

`490010` is the Estate Office (`ictcell.body`: `body_id='OES'`, 308 employees).
Coordinates are **NULL, not placeholder values** — the real GPS for the building
was never supplied, and a staged row is exactly the kind of thing that gets flipped
active later without re-checking. NULL coordinates are excluded by the verify query,
so the row cannot fence anyone even if `is_active` is switched on prematurely.

To activate once the building has been surveyed:

```sql
SELECT ictcell.wow_attendance_body_building_mapping_save(
  '490010', NULL, 'Estate Office', <lat>, <long>, 50, true);
```

That upserts onto the existing row rather than creating a second one.

### Tests

These are the first tests in the repository; `[dev-dependencies]` and `tests/`
did not exist before.

| Suite | Command | Count |
|---|---|---|
| `device_coords` unit tests (`src/routes/wow_attendance.rs`) | `cargo test --bin duerp-attendance` | 9 |
| SQL function tests (`tests/location_verify_sql.rs`) | `cargo test --test location_verify_sql` | 16 |
| `/uploads/log` HTTP block (`tests/uploads_log_block.rs`) | `cargo test --test uploads_log_block` | 1 |

**Unit (9)** — the coordinate contract between the mobile app and the gate:
numeric and string values, the accepted key spellings, negative coordinates,
`0,0` "no fix" rejection, out-of-range, partial/missing, unparseable, and a
`device_info` payload with no GPS at all (the case that must 400).

**SQL (16)** — both functions driven with the exact bind types the handlers use
(`f64 → float8`). Verify: inside radius (~18m), outside radius, exact coordinates
(the `asin` domain guard, 0m), closest-building-wins with two mappings,
per-building radius honoured, inactive mapping ignored, NULL-coordinate mapping
ignored, office with no mapping, unknown employee, invalid coordinates. Save:
create-by-name then upsert-without-duplicating, defaults when optional fields are
omitted, save→verify round trip, deactivation disabling verification, both
warning cases, and five validation rejections.

Every SQL test runs inside a transaction that is dropped without committing, and
none writes to `ictcell.employees` — an existing employee is borrowed instead, so
the tests exercise the real `employees.office` shape. The database is left exactly
as found.

> ⚠️ With `DATABASE_URL` unset the SQL tests **skip and report success**, so a CI
> job without it goes green while testing nothing. Assert the variable is set in CI.

The `float8`/`numeric` parameter bug was found this way — `cargo check` cannot see
it — and is fixed; the deployed signature is `double precision`.

### Not covered

- **No HTTP-level test of `wow_verify`.** The gate's wiring — 400 on missing
  coordinates, 403 on an unverified location, skip for students, and that no
  attendance row is written on rejection — is verified by reading, not execution.
  Exercising it needs mocked AI and DU endpoints plus employee/enrollment fixtures,
  which means either a throwaway database or writes to shared `lms_dev`; this was
  deliberately deferred. The SQL underneath those paths is covered above; the
  handler branching is not.
- **No student location check.** Students have no `employees.office` row, so they
  bypass the gate entirely.
- **Legacy DU tokens** are still accepted unverified (`WOW_ACCEPT_DU_TOKEN`), so on
  that path the ownership check rests on `ExtAuthMiddleware` alone. Pre-existing,
  not introduced here.