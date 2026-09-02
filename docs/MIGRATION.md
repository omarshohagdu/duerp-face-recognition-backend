# Migration — splitting attendance out of duerp-api

What moved, what stayed, and how to cut over without an outage.

> **The one thing that matters:** the URLs did not change. Clients still call
> `/login`, `/ext-api/wow-attendance/*` and `/uploads/**` with the same headers
> and the same bodies. Only the host:port behind those paths changes, and the
> reverse proxy hides even that. **This is a routing change, not an API change.**

---

## What moved into `duerp-attendance`

| From `duerp-api` | To `duerp-attendance` | Note |
|---|---|---|
| `src/routes/wow_attendance.rs` | `src/routes/wow_attendance.rs` | **byte-identical** — verified with `diff` |
| `src/routes/auth.rs` | `src/routes/auth.rs` | `POST /login`; duerp-api keeps its own copy, both mint the same token |
| `src/models/auth.rs` | `src/models/auth.rs` | login request/response |
| `src/utils/step_logger.rs` | `src/utils/step_logger.rs` | removed from duerp-api at the split, then **re-added there** (2026-08-19) so duerp-api logs its own `/ext-api` calls into the same folder — see below |
| `src/utils/{db,jwt,constants}.rs` | same paths | trimmed to what attendance uses |
| `src/middleware/{ext_auth_middleware,api_logger}.rs` | same paths | duerp-api keeps its copies — both serve `/ext-api` |
| `sp/wow_attendance` | `sql/001_wow_attendance.sql` | |
| `duerp-db/wow_attendance_location_verify.sql` | `sql/002_location_verify.sql` | |
| `duerp-db/wow_attendance_token_mismatch.sql` | `sql/003_token_mismatch.sql` | |
| `tests/location_verify_sql.rs` | `tests/location_verify_sql.rs` | **removed** from duerp-api |
| `docs/wow_attendance*.{md,json}` | `docs/` | example URLs repointed to `:8081` (the service later moved to **:8083** — see below) |
| `docs/requirement/wow_attendance_location_verify/` | `docs/requirement/wow_attendance_location_verify/` | geo-fence spec + implementation notes; SQL paths repointed at `sql/002`,`sql/003` |
| `docs/body_building_mapping.md` | `docs/body_building_mapping.md` | MySQL `ict_mis` spec for the buildings/mapping tables the geo-fence reads; no duerp-api code referenced it |

New in the split, with no counterpart in duerp-api:

- `sql/000_ext_api_infra.sql` — reconstructs `ext_api_allowed_ips` and
  `ext_api_call_logs` so a **fresh** database can run this service alone. It is
  a no-op against the shared production database, where both tables already
  exist.
- `GET /health` — a standalone service behind a proxy needs a cheap
  unauthenticated liveness probe; the monolith was checked through its UI.
- `ATTENDANCE_BIND` / `ATTENDANCE_PORT` / `DB_MAX_CONNECTIONS` — the monolith
  hard-coded `0.0.0.0:8080` and a pool of 10.

## Changes made to `duerp-api`

1. The eight `.service(routes::wow_attendance::…)` registrations in `main.rs`
   were replaced with a comment pointing here.
2. `src/routes/wow_attendance.rs`, `src/utils/step_logger.rs` and
   `tests/location_verify_sql.rs` were deleted, and their `mod` entries removed.
3. `GET_BY_EMPLOYEE_ID_ENDPOINT` was dropped from `utils/constants.rs`
   (attendance was its only caller). `SSL_SECRET_KEY` and `LOGIN_ENDPOINT` stay
   — duerp-api still uses them.
4. The `image` dependency was dropped from `Cargo.toml`; nothing else in
   duerp-api decodes images.
5. **`/uploads` and the `/uploads/log` block were deliberately KEPT.** See
   below — this is the easiest thing to get wrong.

`cargo build` and `cargo test` both pass on duerp-api after these changes.

### `<uploads>/log` is shared — do not "clean it up"

**Update (2026-08-19): the face images moved out.** `wow_attendance/` now lives
under `duerp-attendance/uploads/`, so each service owns its own uploads tree:

| Folder | Owner | Served by |
|---|---|---|
| `duerp-api/uploads/{lectures,assignments,course_materials,notice_board_materials}` | duerp-api | :8080 |
| `duerp-attendance/uploads/wow_attendance/{enrolled,live}` | duerp-attendance | :8083 |
| `duerp-api/uploads/log` | duerp-api | neither (blocked) |
| `duerp-attendance/uploads/log` | duerp-attendance | neither (blocked) |

Because both processes still mount their own folder at the same `/uploads` URL
prefix, the proxy must split the prefix: `/uploads/wow_attendance/` to :8083,
the rest to :8080. Without that rule face images 404 — see `DEPLOYMENT.md`.

The 39 existing image files were moved with the config, so nothing was orphaned.
The `image_path` values already stored in `ictcell.wow_attendance_images` still
name the OLD location, and were deliberately left alone: nothing in either
service ever re-opens them (the AI platform holds the embeddings; the column is
an audit reference), and those rows were already a mix of `/app/...`,
`./uploads/...` and dev-box absolute paths from different deploys. Rewriting
them would imply a durability the column has never had.

Each service's `<uploads>/log` sits inside the folder that service serves, so
the 404 block on `/uploads/log` is load-bearing in **both** processes now — it
used to be belt-and-braces on the attendance side. The logs carry client IPs and
employee ids; removing either block exposes that service's own files through its
static route. Both crates carry `tests/uploads_log_block.rs`, which fails if the
block is dropped or registered after the `Files` service.

### Both services write step logs — two folders, one format

duerp-api got its own `utils/step_logger.rs` back on 2026-08-19. It is **not** a
partial revert of the split: attendance logic did not come back, only the
file-logging utility, so duerp-api's `/ext-api` calls appear in its own
`GET /api/logs` viewer instead of the viewer showing attendance traffic alone.

**On 2026-09-01 the folders were separated too.** Attendance logs had still been
landing in `duerp-api/uploads/log`, which contradicted the ownership rule the
rest of the split follows. Each service now writes to its own
`<crate>/uploads/log`. The single-viewer constraint that had justified sharing
was removed rather than lived with: `GET /api/logs` reads **both** folders and
merges them newest-first, and every row carries a `service` field naming its
writer. duerp-api learns about the other folder from `WOW_ATTENDANCE_LOG_DIR`
in its own `.env` — that key is the only link between the two, and clearing it
just makes attendance rows stop appearing.

If both keys ever name one folder (the pre-2026-09-01 layout), `log_sources()`
collapses them so nothing is listed twice — pinned by
`build_page_does_not_double_count_a_shared_folder`.

|  | duerp-attendance | duerp-api |
|---|---|---|
| Driven from | each handler, explicitly | `middleware::api_logger` |
| Detail | real progress (`parsing multipart body`, `calling AI platform`) | call boundaries (arrival, body shape, outcome) |
| `id` in the file name | the person / entity id | the caller's `X-App-Id`, else client IP |
| Scope | the wow-attendance routes | every `/ext-api` route |

The **on-disk format is identical** on both sides, because one parser in
`duerp-api/src/routes/log_routes.rs` reads both. Since 2026-09-01 a file is
grouped into sections rather than written as one chronological stream:

```
route: ext-api/wow-attendance/enroll        <- header, parsed by the viewer
id: 2020111007
started_at: 2026-09-01 15:17:44.064
endpoint: POST /ext-api/wow-attendance/enroll?id=2020111007&token=[redacted]
----
Params:
  query: {"id":"2020111007","session":"2024-25"}
  form: {"device_info":"{...}"}

Steps:
  [15:17:44.065] request received (client_ip=127.0.0.1)
  [15:17:44.068] File: /var/.../enrolled/x.jpeg | url: https://host/uploads/.../x.jpeg

Images:
  https://host/uploads/wow_attendance/enrolled/x.jpeg

Response (AI /recognize) 200: {"recognized":true,"identifier":"2020111007"}

Response (backend) 401: {"success":false,"message":"..."}
```

Reading a failure means going straight to the responses, and a timestamped
jumble buries them among the steps — hence the grouping. Each section stays
individually ordered and every step keeps its timestamp, so no sequence
information is lost.

`Params:` and `Steps:` are always emitted (with `(none)` when empty) so a fixed
skeleton is guaranteed; the two response sections appear only when there was
one, since most calls never reach the AI platform. Several AI calls in one
request are listed in call order.

Two things are easy to get wrong here:

- **The `endpoint:` line is redacted too.** It is built from the raw URI, so
  without masking it becomes the one place a `?token=` survives after `Params`
  masked it. `redact_query()` handles it inside `set_endpoint`, not at the call
  sites, because a call site eventually forgets.
- **Image URLs are absolute where possible.** `Images:` lists every file the
  call wrote, as a URL that opens from the log viewer; each `File:` step keeps
  the filesystem path beside it, because that is what an operator needs on the
  box. The origin comes from `WOW_PUBLIC_BASE_URL`, else from the request
  (`connection_info()`, which honours `X-Forwarded-Proto`/`-Host`). With neither,
  the relative `/uploads/...` path is kept rather than a guessed host — a wrong
  origin looks clickable and 404s somewhere unrelated. Filenames are
  percent-escaped, so names with spaces stay openable.
- **`steps_of()` matches the `[hh:mm:ss.mmm]` prefix**, not "every non-empty
  line". Section headings and response payloads are not steps, and counting them
  would inflate the step count and make `last_step` a fragment of JSON. The same
  rule reads pre-2026-09 flat files unchanged, so old logs keep listing. Changing the shape in either
`step_logger.rs` breaks the viewer for **every** file, not just that service's;
both files carry a comment saying so.

duerp-api's logs deliberately record body *shape* (`JSON keys: [std_id]`), never
body values, and never headers — the full payload already goes to
`ictcell.ext_api_call_logs`, which sits behind database access rather than on a
shared uploads volume. `middleware::api_logger`'s test asserts credentials and
request values stay out of the file.

---

## Cutover runbook

Order matters: **start the new service before you stop serving the old paths.**

1. **Apply the schema.** Against the shared database this is a no-op for
   `000`–`002` (already applied) and creates the audit table for `003`:
   ```bash
   for f in sql/000_ext_api_infra.sql sql/001_wow_attendance.sql \
            sql/002_location_verify.sql sql/003_token_mismatch.sql; do
     psql "$DATABASE_URL" -f "$f"
   done
   ```

2. **Configure `.env`.** Copy `duerp-api`'s values for the shared keys —
   `DATABASE_URL`, `JWT_SECRET`, `EXT_APP_ID`, `EXT_APP_PASSWORD`,
   `SSL_API_ENDPOINT`, and every `WOW_*`. `JWT_SECRET` must be **byte-identical**
   or clients get intermittent 401s depending on which service minted their
   token.

3. **Point the storage paths at the existing folder.** `WOW_UPLOAD_DIR` and
   `WOW_LOG_DIR` must resolve to the *same* directories duerp-api was writing,
   or previously enrolled images become unreachable. Verify:
   ```bash
   ls "$WOW_UPLOAD_DIR/enrolled" | head    # must show existing enrollments
   ```

4. **Start duerp-attendance and smoke-test it directly**, before any proxy
   change:
   ```bash
   curl -s localhost:8083/health
   curl -s -X POST "localhost:8083/ext-api/wow-attendance/check?person_id=<known-id>" \
        -H "X-App-Id: $EXT_APP_ID" -H "X-App-Password: $EXT_APP_PASSWORD"
   ```
   A `403 IP address not allowed` here means the allow-list row for that exact
   path does not include the caller IP — the rows are per full path, not per
   prefix.

5. **Flip the proxy.** Route `/ext-api/wow-attendance/` to `:8083`, leave
   everything else on `:8080`. See `DEPLOYMENT.md` for the nginx block. This is
   the moment of cutover and it is the one step to roll back.

6. **Deploy the trimmed duerp-api.** Only after step 5 is verified — until then
   the old build is your rollback.

7. **Watch.** For the first day:
   ```sql
   SELECT endpoint, status_code, count(*)
     FROM ictcell.ext_api_call_logs
    WHERE created_at > now() - interval '1 hour'
      AND endpoint LIKE '/ext-api/wow-attendance%'
    GROUP BY 1, 2 ORDER BY 3 DESC;
   ```
   and check `$WOW_LOG_DIR` is filling with per-call step logs.

### Rolling back

Revert the proxy to send `/ext-api/wow-attendance/` back to `:8080` and
redeploy the previous duerp-api build. Nothing needs to be undone in the
database: both builds read and write the same tables in the same shapes, and
`003_token_mismatch.sql` only adds a table the old build ignores.

---

## The service moved to port 8083 (2026-09-01)

The split originally put duerp-attendance on `:8081`. It now defaults to
**`:8083`** (`ATTENDANCE_PORT`, and the fallback in `src/main.rs`), because
another project on the same host — `student_scholarship_api` — also binds 8081
and starts independently; whichever lost the race died with
`Os { code: 98, AddrInUse }`.

Two mentions of `:8081` survive above on purpose: they record what the split did
at the time and are not instructions. Everything actionable — both nginx
`proxy_pass` blocks, the curl examples, the Postman `base_url` — says 8083.

## Follow-ups this split leaves open

These are known and deliberate — listed so they are not rediscovered as
surprises.

- ~~**Duplicate SQL.**~~ **Closed.** `duerp-api/sp/wow_attendance`,
  `duerp-db/wow_attendance_location_verify.sql` and
  `duerp-db/wow_attendance_token_mismatch.sql` have been deleted, together with
  the stale `duerp-api/docs/wow_attendance*.{md,json}` copies. The `duerp-db`
  files were **byte-identical** to `sql/002`/`sql/003` (verified with `diff`);
  the docs differed only in the `:8080` → `:8081` example URLs the copies here
  already carry. `duerp-attendance/sql/` and `duerp-attendance/docs/` are the
  only copies now — a deploy script that still names a `duerp-db/wow_*` path
  must be repointed at `duerp-attendance/sql/`.
- **`duerp-api`'s package is still named `attendance-system`** in its
  `Cargo.toml`, which now describes the wrong service. Renaming it changes the
  built binary path (`target/release/attendance-system`), which systemd units
  and deploy scripts may reference, so it was not done as part of this split.
- **Two `/login` implementations.** Both services mint the same token from the
  same DU endpoint with the same secret. Harmless today; if login logic changes,
  it must change in both.
- **`WOW_ACCEPT_DU_TOKEN` and `WOW_IDTYPE_FALLBACK`** are both migration-window
  flags that came across unchanged. See `ARCHITECTURE.md` for what closes them.
