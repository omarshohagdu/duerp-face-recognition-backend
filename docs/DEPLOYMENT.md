# Deployment — duerp-attendance

Runs as an ordinary Rust binary on its own port, behind **its own nginx
vhost**, which also serves the admin frontend. Only `POST /login` is proxied
back to duerp-api. See [`MIGRATION.md`](MIGRATION.md) for the cutover order —
this document covers the steady state.

Paths throughout assume this deployment root:

| | |
|---|---|
| Service | `/var/www/face_recognition/duerp-face-recognition-backend` |
| Frontend | `/var/www/face_recognition/duerp-face-recognition-frontend` |
| Uploads | `/var/www/face_recognition/duerp-face-recognition-backend/uploads` |

They must agree with `WOW_UPLOAD_DIR`, `WOW_UPLOADS_SERVE_DIR` and
`WOW_LOG_DIR` in `.env`, and with `ReadWritePaths` in the unit file. Move the
root and all four change together.

---

## Build

```bash
cargo build --release        # -> target/release/duerp-attendance
```

### Frontend

```bash
cd /var/www/face_recognition/duerp-face-recognition-frontend
npm ci
npm run build                # -> dist/  (~400 KB)
```

`npm run build` loads `.env.production` on top of `.env`. That file blanks
`VITE_ATTENDANCE_END_POINT`, so the bundle calls `/login`,
`/ext-api/wow-attendance/*` and `/uploads/wow_attendance/*` as **relative**
URLs against whatever origin serves it — which is why the vhost below has to
serve the SPA and proxy the API, and why no CORS is needed.

`VITE_*` values are inlined at build time. Changing `.env` on the server does
nothing; rebuild instead.

## Database

The service owns its own schema, `attendance`. Only the shared ext-api
infrastructure still lives in `ictcell`.

```bash
# 1. shared ext-api infrastructure (ictcell) — a no-op where duerp-api
#    already created it.
psql "$DATABASE_URL" -f sql/000_ext_api_infra.sql

# 2. everything this service owns: 15 tables, the attendance.employees view
#    and 17 functions, all under the `attendance` schema. (7 of those tables
#    and the last 2 functions are the access-control layer — created EMPTY,
#    and read by nothing until the middleware ships; see access_control.md.)
psql "$DATABASE_URL" -v app_role=duerp_attendance \
     -f docs/attendance_schema.sql
```

Both are idempotent — every statement is `CREATE ... IF NOT EXISTS` or
`CREATE OR REPLACE`, so re-running changes nothing.

`sql/001`–`sql/003` are the **superseded** `ictcell` originals, kept for
reference and for reading the pre-cutover database. Do not apply them to a new
deployment; `docs/attendance_schema.sql` is generated from them and is the
current source of truth.

`sql/004_nfc_card.sql` is different: it targets the current `attendance` schema
and is the incremental migration for the NFC card module, for an existing
deployment that does not want to re-run the whole schema file. Its objects are
already in `docs/attendance_schema.sql` (section 5), so step 2 above covers a
fresh deployment on its own. See [`nfc_card.md`](nfc_card.md).

`sql/006_access_control.sql` is optional, and it is what fills the
access-control tables section 6 of the schema file creates empty. It copies the
ERP's RBAC rows — roles, resources, accounts, role grants, per-person overrides
and menu items — out of `ictcell` into `attendance`, ids preserved, then seeds
one rule per endpoint in **audit mode** (`enforce = false`). Applying it refuses
nothing, and the service does not read any of it yet.

`sql/007_access_admin.sql` adds the API behind the Access Roles screen —
`POST /ext-api/access/*`, their rule and allow-list rows, and an audit table
for every change. **Those seven endpoints enforce their permission
immediately**, whatever `EXT_ACCESS_CONTROL` says: they are new, so nobody
breaks, and they are what hands out permissions.

sql/006 **reads** `ictcell` and never writes to it; the ERP's own tables are
left exactly as they are. The flip side is the thing to decide before going further:
the ERP's admin screens keep writing `ictcell`, so a role assigned there no
longer reaches this service. §9 of that file has the three ways to resolve it
(point the ERP at `attendance`, re-sync on a schedule, or administer roles
here). Design and rollout: [`access_control.md`](access_control.md).

Cutting over a database that already ran the `ictcell` version needs the data
copied across as well — the commented appendix at the end of
`docs/attendance_schema.sql` has the statements, in dependency order, with the
sequence resets. Run it with the service stopped.

The service needs `SELECT` on the identity tables (`employees`, `lms_student`,
`lms_faculty`, `body`) in `ictcell`, full DML on everything in `attendance`,
and full DML on the `ext_api_*` tables in `ictcell`. It creates no tables at
runtime — schema changes are always an explicit `psql` run.

### Opening the IP allow-list

`sql/000` seeds every endpoint with localhost only. Every real client IP must be
added per endpoint, because the check matches the **full path**, not a prefix:

```sql
UPDATE attendance.ext_api_allowed_ips
   SET ip_address = ip_address || '{203.0.113.10}'
 WHERE endpoint IN ('/ext-api/wow-attendance/verify',
                    '/ext-api/wow-attendance/enroll');
```

The admin screens call seven more endpoints from the SAME browser, so an IP that
can check in cannot necessarily open the reports — each path needs its own row:
`enrolled`, `check`, `reports/by-date`, `reports/by-person`, `mapping-save`,
`logs/login` and `logs/attendance`.

The NFC card reader is a separate client on separate paths, and it has its own
three rows — `/ext-api/nfc-card/get_card_info`,
`/ext-api/nfc-card/save_card_info` and
`/ext-api/nfc-card/checking_card_reg_status` — **all open to every IP**. They
hold the `'*'` wildcard rather than a list of addresses, because the readers are
on campus DHCP. That leaves the app
credentials and a bearer token as the only gate on them, and any token holder
who can reach the service can reassign a card. See
[`nfc_card.md`](nfc_card.md#auth).

`'*'` works on any row, not just these. Use it only where the endpoint's own
auth is enough by itself; drop it from the array to put the allow-list back,
with no redeploy.

Behind a reverse proxy the recorded IP is whatever `X-Forwarded-For` resolves
to, so the proxy **must** set it — otherwise every request appears to come from
the proxy itself and the allow-list stops meaning anything.

## Configuration

Copy `.env.example` to `.env` and fill it in. The keys that must match
duerp-api exactly are marked SHARED there; `JWT_SECRET` is the one that causes
confusing intermittent 401s if it drifts.

Storage paths should be **absolute**. Under systemd the working directory is
not the crate root, so the relative defaults (`./uploads/...`) resolve
somewhere unintended and enrollments silently write to a fresh empty folder.

This service writes into **two different trees**, and they are not
interchangeable:

| Key | Points at | Why there |
|---|---|---|
| `WOW_UPLOAD_DIR` | `<root>/uploads/wow_attendance` | Face captures are this service's data; duerp-api never reads them. |
| `WOW_UPLOADS_SERVE_DIR` | `<root>/uploads` | Parent of the above — the `/uploads` URL prefix supplies the rest. |
| `WOW_PUBLIC_BASE_URL` | *(unset)* | Public origin used to build openable image URLs in the step logs. Leave unset when the service is reached directly, or when your proxy sets `X-Forwarded-Proto`/`-Host`. Set it when the proxy does neither, or the logs will carry `http://127.0.0.1:8083/...` links no admin can open. |
| `WOW_LOG_DIR` | `<root>/uploads/log` | This service's own step logs, alongside its images. duerp-api's admin viewer still shows them: it reads this folder too, via `WOW_ATTENDANCE_LOG_DIR` in **duerp-api's** `.env`, which must name this same path. |

The first two moved out of duerp-api on 2026-08-19; `WOW_LOG_DIR` did not.

### The face-verification toggle

`sql/008_system_settings.sql` adds `attendance.system_settings` and
`GET|PUT /admin-api/settings/nfc-face-verify` — an admin switch for the NFC
face check, so turning it off during a face-service outage is an API call
rather than an `.env` edit and a restart.

Applying it changes nothing on its own. The seeded row says `OFF`, but the
reader only obeys the table once an admin has written to it (`updated_by IS NOT
NULL`); until then `NFC_FACE_VERIFY_URL` still decides, exactly as before. That
is deliberate — the alternative would be a migration that silently disables
face verification on deploy day.

The admin panel screen is **`/settings/face-verification`** (nav: "Face
verification"), alongside `/access-roles`. Both are admin-only in the nav and
enforced server-side — hiding a link hides a link.

`/admin-api` is mounted behind the **same** `ExtAuthMiddleware` as `/ext-api`,
so it needs its allow-list row (seeded) and the app credentials, plus
`admin.settings.manage`. Values are cached in-process for 30s and the cache is
dropped on every successful write.

### Access control (per-person permissions)

Two keys, both safe to leave at their defaults — together they are why a
deployment carrying this code refuses nothing until somebody decides otherwise:

| Key | Default | Meaning |
|---|---|---|
| `EXT_ACCESS_CONTROL` | `audit` | `off` = never ask. `audit` = ask on every `/ext-api` request, record the verdict, serve it anyway. `on` = also refuse, but only where a rule row says `enforce = true`. Anything unrecognised is `audit`. |
| `EXT_ACCESS_FAIL_CLOSED` | `false` | Whether a *failed* check (unapplied migration, database down) refuses. `false` means a binary deployed ahead of `sql/006_access_control.sql` behaves exactly as today. |

The evidence the rollout is read from lands in
`attendance.ext_api_access_audit` (one row per person/endpoint/reason, with a
hit counter) and in the service log, greppable on `[access]`:

```bash
journalctl -u duerp-attendance | grep '\[access\]' | tail
```

**Before `on`:** every `app_users.role_id` is NULL until backfilled, so
enforcing today denies everybody — and `WOW_ACCEPT_DU_TOKEN` must be `false`
first, or a forged token can claim any identity. Both are covered in
[`access_control.md`](access_control.md#7--preconditions--read-this-before-enabling-anything).

### The face-match service (NFC saves depend on it)

`POST /ext-api/nfc-card/save_card_info` sends the card photo and the selfie to
a face-match service and writes the row only when it answers `match: true`:

| Key | Points at | Why there |
|---|---|---|
| `NFC_FACE_VERIFY_URL` | e.g. `http://10.224.224.101:8089/verify` | The match endpoint. **Leave it unset and every card save is rejected** with a 503 — the gate fails closed on purpose. |
| `NFC_FACE_VERIFY_API_KEY` | *(the service's key)* | Sent as `X-API-Key`. A wrong key is a 503, not a 400: it is this service's credential, not the student's photo. |
| `NFC_FACE_VERIFY_TIMEOUT_SECS` | 30 | Bounds a save. Keep it **below** the proxy's read timeout, or the caller gets the proxy's error instead of ours. |

Two operational consequences:

- **This host must be able to reach that service.** It is an outbound call from
  the app server, not from the reader, so a firewall rule that only covers the
  readers' subnet is not enough.
- **If it goes down, card registration stops.** That is the designed behaviour
  — an unchecked card mapping is what the gate prevents — so treat the service
  as a dependency of card registration, not as an optional extra. The
  give-up-and-save switch does not exist; the fix is to bring it back up.

See [`nfc_card.md`](nfc_card.md#the-face-match-gate) for the request shape and
the error codes.

### Endpoints that used to be admin-gated

These five required a shared `X-Admin-Key` matching `WOW_ADMIN_KEY`. **That key
has been removed** — from the service, from `.env.example`, and from the SPA —
and they are now guarded by a valid bearer token and the ext-api gate alone:

| Endpoint | Who can call it now |
|---|---|
| `mapping-save` | any signed-in account — it writes the geo-fence |
| `reports/by-date` | any signed-in account — every check-in, university-wide |
| `reports/by-person` | any signed-in account, for **any** `person_id`, not just its own |
| `logs/login`, `logs/attendance` | any signed-in account |

**Nothing on this box restricts them to admins any more.** The token carries only
`sub` and `exp`, so the service cannot see a role; the SPA hides these screens
from non-admin accounts, but that is navigation, not authorization, and a
hand-made request bypasses it. Until a real role check exists, the ext-api IP
allow-list (`attendance.ext_api_allowed_ips`) is the only remaining boundary on the
`wow-attendance` paths — which makes those rows, and who holds a login, the whole
security story. (The two `nfc-card` paths have opted out of it with `'*'`.)

Drop `WOW_ADMIN_KEY` from any deployed `.env`; nothing reads it. Clients that
were sending `X-Admin-Key` keep working — an unrecognised header is ignored.

### The step-log viewers

`/uploads/log` and `/uploads/login` stay 404 — at the nginx rules above AND
inside the service (`main.rs` registers those blocks ahead of the static
route). Nothing serves those files.

The two admin screens read them through JSON endpoints instead:

| Endpoint | Reads |
|---|---|
| `POST /ext-api/wow-attendance/logs/login` | `LOGIN_LOG_DIR` — one file per sign-in |
| `POST /ext-api/wow-attendance/logs/attendance` | `WOW_LOG_DIR` — one file per enroll/verify/mapping-save |

Both take `page`, `limit`, `person_id`, `from_date`, `to_date` as query params
and return a listing; add `file=<name>` to read one file's content. The
requested name is matched against the directory's own listing before it is
opened, so a path outside the folder cannot be reached by naming it.

**A `person_id` ending in `-2`, `-3`, …** is not a different person. Files are
named `{id}_{timestamp}.log` to the second, and when one id makes two calls
inside one second the second file is suffixed (`2017001010-2_…`) so it cannot
overwrite the first — which is what used to happen. Filtering is a substring
match, so a search for the bare id still returns them all.

> **duerp-api has its own copy of the writer** (`src/utils/step_logger.rs`,
> which the two services are supposed to keep identical) and it **still
> overwrites**. The readers need no change — the suffix goes on the id, so the
> five trailing timestamp segments both parsers split on are untouched — but
> the fix itself has to be ported, or duerp-api keeps losing a log every time a
> client retries within the same second.

Do not "simplify" this by dropping the `/uploads/log` blocks and letting the
static route serve the folder. The files carry usernames, client IPs, GPS,
employee ids and full request and response bodies. Reaching one now requires
only a valid login (the admin key that used to gate the JSON readers is gone),
so the 404 blocks are what keep them off an unauthenticated URL entirely.

## systemd

```ini
# /etc/systemd/system/duerp-attendance.service
[Unit]
Description=DU ERP — WOW attendance service
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
User=www-data
Group=www-data
WorkingDirectory=/var/www/face_recognition/duerp-face-recognition-backend
EnvironmentFile=/var/www/face_recognition/duerp-face-recognition-backend/.env
ExecStart=/var/www/face_recognition/duerp-face-recognition-backend/target/release/duerp-attendance
Restart=on-failure
RestartSec=5

# One writable tree: this service's own uploads, holding both the face captures
# and its step logs. ProtectSystem=full makes everything else read-only.
#
# duerp-api's unit needs READ access here so its /api/logs viewer can list these
# files — reads are allowed by default under ProtectSystem, so nothing extra is
# required there, but the two units must run as users that can traverse it.
ReadWritePaths=/var/www/face_recognition/duerp-face-recognition-backend/uploads
ProtectSystem=full
PrivateTmp=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

### If it restart-loops on `DATABASE_URL is not set`

The service reads config from the process environment. That environment is
populated by **one** of three things, and the first line it logs says which one
it found:

```
config: loaded /var/www/.../.env            # a .env was found from the working directory
config: loaded /etc/duerp/attendance.env (ENV_FILE)
config: no .env found searching up from /   # nothing found — fine ONLY if EnvironmentFile= is set
```

`dotenvy` searches upward from the **working directory**, so a unit without
`WorkingDirectory=` finds no `.env` at all. Both keys above are in the unit for
that reason — `WorkingDirectory=` for the search, `EnvironmentFile=` so it works
even without it. Check, in order:

```bash
systemctl show duerp-attendance -p WorkingDirectory -p EnvironmentFiles
sudo -u www-data test -r /var/www/face_recognition/duerp-face-recognition-backend/.env \
  && echo "readable" || echo "MISSING or unreadable by the service user"
```

**`.env` is gitignored** (`.gitignore:31`), so `git pull` does **not** carry it
onto a new host — a fresh deploy has no `.env` even though the repo looks
complete. Copy `.env.example` to `.env` there and fill it in.

To run from an arbitrary working directory, set `ENV_FILE` to an absolute path;
it skips the search entirely and is the quickest fix when this is already
failing in production:

```ini
Environment=ENV_FILE=/var/www/face_recognition/duerp-face-recognition-backend/.env
```

Note `EnvironmentFile=` and `ENV_FILE=` differ on quoting: systemd strips
surrounding quotes from a value, and so does dotenvy, but a plain
`export $(grep ... .env)` in a shell does **not** — `EXT_APP_PASSWORD` is
single-quoted in this file, which is why hand-rolled shell sourcing yields
`401 Invalid App ID or Password`.

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now duerp-attendance
journalctl -u duerp-attendance -f
```

## nginx

Attendance gets its **own vhost**. It serves the admin SPA from disk, proxies
the attendance API and face images to `:8083`, and forwards only `POST /login`
to duerp-api on `:8080`.

```nginx
# /etc/nginx/sites-available/attendance.du.ac.bd

server {
    listen 80;
    server_name attendance.du.ac.bd;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl;
    http2 on;
    server_name attendance.du.ac.bd;

    ssl_certificate     /etc/letsencrypt/live/attendance.du.ac.bd/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/attendance.du.ac.bd/privkey.pem;

    # TLS is not optional here: getUserMedia and navigator.geolocation are both
    # disabled outside a secure context, so an expired certificate takes
    # check-in offline rather than just dropping the padlock.

    root /var/www/face_recognition/duerp-face-recognition-frontend/dist;

    # Face captures are multi-megabyte and an enrollment sends several in one
    # request; must be at least WOW_MAX_UPLOAD_MB (default 25).
    client_max_body_size 30m;

    # ---- 1. deny directory indexes under /uploads/ -----------------------
    # The service registers its static route with file listings ENABLED, so a
    # bare directory URL returns a browsable index of every enrolled face.
    # Regex locations are matched before prefix ones, so this shadows the
    # proxy rules below without needing to sit above them.
    location ~ ^/uploads/(.*/)?$ { return 404; }

    # ---- 2. step logs: never served -------------------------------------
    # They carry client IPs, employee ids, GPS and image paths. The service
    # blocks this internally too; this is defence in depth. Keep it ABOVE the
    # /uploads/wow_attendance/ rule — nginx picks the longest matching prefix
    # and these two must not be reordered by accident.
    location /uploads/log { return 404; }

    # ---- 3. face images: served by the attendance service ----------------
    location /uploads/wow_attendance/ {
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # ---- 4. attendance API ----------------------------------------------
    location /ext-api/wow-attendance/ {
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host              $host;
        # Required: the ext-api IP allow-list reads this. Without it every
        # request looks like it came from the proxy.
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Image upload + AI round-trip. The service allows the AI platform 30s;
        # keep this comfortably above it so nginx is never the one to give up
        # on a request that actually succeeded.
        proxy_read_timeout  120s;
        proxy_send_timeout  120s;
        proxy_request_buffering off;
    }

    # This service serves THREE prefixes under /ext-api, not one. A prefix with
    # no block here falls through to `location /` and never reaches :8083 —
    # which surfaces as a 404 or an SPA page rather than an API error, and is
    # the first thing to check when a brand-new endpoint "does not exist".
    #
    # If the NFC endpoints already work in production, the live config has
    # something this document did not, and these are the blocks it is missing.
    location /ext-api/nfc-card/ {
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # save_card_info uploads two images and waits on the face-match
        # service (NFC_FACE_VERIFY_TIMEOUT_SECS, default 30s).
        proxy_read_timeout  120s;
        proxy_send_timeout  120s;
        proxy_request_buffering off;
    }

    # Exact match, not a prefix: /ext-api/me/access is the only path here, and
    # it is the one every client calls at login and on every app foreground.
    location = /ext-api/me/access {
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Small JSON, no uploads — the defaults are fine. Do NOT add a cache
        # here: the response is per-person, and the endpoint does its own
        # revalidation with an ETag.
        proxy_read_timeout  30s;
    }

    # ---- 5. login stays on duerp-api -------------------------------------
    # Both services mint an identical token, so /login was never split out.
    # The SPA posts it to its own origin, which on a dedicated vhost is this
    # one — so it has to be forwarded explicitly. Exact match: nothing else
    # under duerp-api is exposed here.
    location = /login {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # ---- 6. hashed build assets ------------------------------------------
    # Vite fingerprints these, so they are safe to cache forever.
    location /assets/ {
        try_files $uri =404;
        access_log off;
        expires 1y;
        add_header Cache-Control "public, immutable";
    }

    # ---- 7. the SPA ------------------------------------------------------
    # try_files is load-bearing: react-router owns /attendance/*, /face-setup
    # and /login client-side, so a deep link or a refresh must return
    # index.html rather than a filesystem 404.
    location / {
        try_files $uri $uri/ /index.html;
        add_header Cache-Control "no-cache";
    }
}
```

```bash
sudo ln -s /etc/nginx/sites-available/attendance.du.ac.bd /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
```

`attendance.du.ac.bd` is the confirmed production hostname; the certificate is
issued for it and `server_name` must match. If it ever moves, only this file
and the certificate change — the frontend needs no rebuild, because the bundle
uses relative URLs.

### Why this vhost and not a shared one

The SPA is built with an empty `VITE_ATTENDANCE_END_POINT`, so it calls its own
origin. That buys three things:

- **No CORS.** The service ships `Cors::permissive()`, which should not face a
  public network. Same-origin removes the need for it.
- **No rebuild on a domain change.** `VITE_*` values are inlined at build time.
- **One place to enforce the upload rules.** Body size, timeouts and the
  `/uploads` denials all live in one server block.

The cost is rule 5: `/login` must be forwarded by hand. Drop it and the SPA
gets a 404 from its own vhost on every sign-in attempt.

### A bare 404 on `/ext-api/wow-attendance/*` means the proxy rule is missing

![Postman: POST {{url}}/ext-api/wow-attendance/verify with an images file part
and a device_info part, coming back 404 Not Found with an empty body in
45 ms.](assets/verify_issue.png)

This is the signature failure of a half-finished cutover, and it is easy to
misread as a bug in the request. It is not: the body is **empty** and the status
is a plain `404`, so nothing in this service ever saw the call — a request that
did reach it fails with a JSON envelope (`{"status":"error", …}`), never a blank
404. The request above is well-formed; `verify` really does take `images` as a
file part plus a `device_info` text part.

What produces it: the request never reached duerp-attendance (:8083). On the
dedicated vhost it was most likely swallowed by rule 7 — the SPA catch-all
returns `index.html` for anything unmatched, and an API client reading that as
a failure reports a bare 404. Check, in order:

1. The `location /ext-api/wow-attendance/ { … }` block is present and nginx was
   reloaded (`nginx -t && systemctl reload nginx`).
2. The trailing slash matches. `location /ext-api/wow-attendance/` does **not**
   match a request for `/ext-api/wow-attendance` with no trailing segment.
3. duerp-attendance is actually up — `curl -s localhost:8083/health`.
4. You are hitting the vhost, not duerp-api. Postman's `{{url}}` variable
   pointing at the old host:port is the single most common cause; against the
   service directly, `localhost:8083/ext-api/wow-attendance/verify` must answer.

A **200 that returns HTML** instead of JSON is the same fault seen from the
other side: the SPA catch-all answered, so the API rule did not match.

A `403 IP address not allowed` instead means routing is fine and the ext-api
allow-list is what rejected you — see [Opening the IP allow-list](#opening-the-ip-allow-list).

## Health checks

```bash
curl -s localhost:8083/health
# {"status":"ok","service":"duerp-attendance","version":"0.1.0"}
```

`/health` is unauthenticated and touches neither the database nor the AI
platform — it answers "is the process up", not "is the system healthy". For the
latter, check that a recent `/verify` succeeded:

```sql
SELECT max(created_at) FROM ictcell.wow_attendance_records WHERE matched;
```

## Operating notes

**Connection pool.** Both services draw from one Postgres server. Keep
`DB_MAX_CONNECTIONS` here plus duerp-api's pool under the server's
`max_connections`.

**Disk.** Every enroll stores its images permanently and every verify stores a
live capture, so `uploads/` grows without bound. Re-enrollment does not delete
the old images — it retires the enrollment row and keeps the history. Budget
for growth and plan a retention policy for `wow_attendance/live/`.

**Step logs.** One file per enroll/verify/mapping-save call in `WOW_LOG_DIR`,
never rotated by the service. Rotate or prune them yourself. They are the first
thing to read when a check-in is disputed — they record every branch the
handler took, including which token type was used.

**Fail-closed dependencies.** If the AI platform is down, enroll and verify
return 502 and write nothing. That is intended: an enrollment the AI cannot
match is worse than no enrollment. Alert on a rising 502 rate for
`/ext-api/wow-attendance/*` in `ext_api_call_logs`.

**Impersonation attempts** land in
`ictcell.wow_attendance_token_mismatch_record`. A steady trickle usually means
a client sending a stale id; a spike is worth investigating:

```sql
SELECT action, requested_user_id, ai_recognized_id, ai_similarity, created_at
  FROM ictcell.wow_attendance_token_mismatch_record
 WHERE created_at > now() - interval '7 days'
 ORDER BY created_at DESC;
```