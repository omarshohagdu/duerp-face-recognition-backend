# Deployment — duerp-attendance

Runs as an ordinary Rust binary on its own port, behind the same reverse proxy
as duerp-api. See [`MIGRATION.md`](MIGRATION.md) for the cutover order — this
document covers the steady state.

---

## Build

```bash
cargo build --release        # -> target/release/duerp-attendance
```

## Database

Apply in order. Against the existing shared database, `000`–`002` are no-ops
and only `003` creates anything new.

```bash
for f in sql/000_ext_api_infra.sql sql/001_wow_attendance.sql \
         sql/002_location_verify.sql sql/003_token_mismatch.sql; do
  psql "$DATABASE_URL" -f "$f" || break
done
```

The service needs `SELECT` on the identity tables (`employees`, `lms_student`,
`lms_faculty`, `body`) and full DML on the `wow_attendance_*`, `buildings`,
`body_building_mapping` and `ext_api_*` tables. It creates no tables at
runtime — schema changes are always an explicit `psql` run.

### Opening the IP allow-list

`sql/000` seeds every endpoint with localhost only. Every real client IP must be
added per endpoint, because the check matches the **full path**, not a prefix:

```sql
UPDATE ictcell.ext_api_allowed_ips
   SET ip_address = ip_address || '{203.0.113.10}'
 WHERE endpoint IN ('/ext-api/wow-attendance/verify',
                    '/ext-api/wow-attendance/enroll');
```

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
| `WOW_UPLOAD_DIR` | `duerp-attendance/uploads/wow_attendance` | Face captures are this service's data; duerp-api never reads them. |
| `WOW_UPLOADS_SERVE_DIR` | `duerp-attendance/uploads` | Parent of the above — the `/uploads` URL prefix supplies the rest. |
| `WOW_PUBLIC_BASE_URL` | *(unset)* | Public origin used to build openable image URLs in the step logs. Leave unset when the service is reached directly, or when your proxy sets `X-Forwarded-Proto`/`-Host`. Set it when the proxy does neither, or the logs will carry `http://127.0.0.1:8083/...` links no admin can open. |
| `WOW_LOG_DIR` | `duerp-attendance/uploads/log` | This service's own step logs, alongside its images. duerp-api's admin viewer still shows them: it reads this folder too, via `WOW_ATTENDANCE_LOG_DIR` in **duerp-api's** `.env`, which must name this same path. |

The first two moved out of duerp-api on 2026-08-19; `WOW_LOG_DIR` did not.

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
WorkingDirectory=/var/www/Rust/duerp/duerp-attendance
EnvironmentFile=/var/www/Rust/duerp/duerp-attendance/.env
ExecStart=/var/www/Rust/duerp/duerp-attendance/target/release/duerp-attendance
Restart=on-failure
RestartSec=5

# One writable tree: this service's own uploads, holding both the face captures
# and its step logs. ProtectSystem=full makes everything else read-only.
#
# duerp-api's unit needs READ access here so its /api/logs viewer can list these
# files — reads are allowed by default under ProtectSystem, so nothing extra is
# required there, but the two units must run as users that can traverse it.
ReadWritePaths=/var/www/Rust/duerp/duerp-attendance/uploads
ProtectSystem=full
PrivateTmp=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now duerp-attendance
journalctl -u duerp-attendance -f
```

## nginx

Attendance paths go to `:8083`, everything else stays on duerp-api at `:8080`.

```nginx
server {
    listen 80;
    server_name erp.example.edu;

    # Face captures are multi-megabyte; must be at least WOW_MAX_UPLOAD_MB.
    client_max_body_size 30m;

    # --- attendance service ---
    location /ext-api/wow-attendance/ {
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host              $host;
        # Required: the ext-api IP allow-list checks this.
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Image upload + AI round-trip. The service's own AI timeout is 30s;
        # keep this comfortably above it so nginx is never the one to give up.
        proxy_read_timeout  120s;
        proxy_send_timeout  120s;
        proxy_request_buffering off;
    }

    # Never expose the step logs — they carry tokens, client IPs and employee
    # ids. Both services also block this internally; this is defence in depth.
    # Keep this BEFORE the /uploads/wow_attendance/ rule below: nginx picks the
    # longest matching prefix, and these two must not be reordered by accident.
    location /uploads/log { return 404; }

    # --- face captures live with the attendance service ---
    # Since 2026-08-19 `wow_attendance/` sits under duerp-attendance/uploads, so
    # this prefix is served by :8083 while the rest of /uploads (lectures,
    # course materials, notice-board attachments) stays on duerp-api. Without
    # this rule every enrolled/live image 404s: duerp-api serves /uploads from a
    # folder that no longer contains them.
    location /uploads/wow_attendance/ {
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # --- everything else: duerp-api ---
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

`POST /login` is intentionally **not** split out: both services mint an
identical token, so leaving it on duerp-api is correct and needs no rule.

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

What produces it: the request landed on **duerp-api (:8080)**, which no longer
registers these routes, instead of on duerp-attendance (:8083). Check, in order:

1. The `location /ext-api/wow-attendance/ { … }` block above is present and
   nginx was reloaded (`nginx -t && systemctl reload nginx`).
2. The trailing slash matches. `location /ext-api/wow-attendance/` does **not**
   match a request for `/ext-api/wow-attendance` with no trailing segment.
3. duerp-attendance is actually up — `curl -s localhost:8083/health`.
4. You are hitting the proxy, not `:8080` directly. Postman's `{{url}}` variable
   pointing at the old host:port is the single most common cause; against the
   service directly, `localhost:8083/ext-api/wow-attendance/verify` must answer.

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