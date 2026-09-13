-- =====================================================================
-- Move this service's ext-api gate into the `attendance` schema
--
-- WHAT CHANGES: `src/middleware/api_logger.rs` now INSERTs into
-- `attendance.ext_api_call_logs` and `src/middleware/ext_auth_middleware.rs`
-- now SELECTs from `attendance.ext_api_allowed_ips`. This file creates those
-- two tables and copies THIS SERVICE's rows across from `ictcell`.
--
-- WHY A COPY AND NOT A MOVE — READ THIS BEFORE "TIDYING UP" THE OLD TABLES:
-- `ictcell.ext_api_allowed_ips` and `ictcell.ext_api_call_logs` are SHARED with
-- duerp-api, which is still live on the same database. The allow-list holds 15
-- duerp-api endpoints (`/ext-api/getNoticeStream`, `/ext-api/submitAssignment`,
-- …) next to this service's 12, and roughly 3,500 of the call-log rows are
-- duerp-api's. Renaming, dropping or `ALTER ... SET SCHEMA`-ing either table
-- takes duerp-api's whole ext-api surface down with it: every one of its
-- endpoints 403s the moment its allow-list disappears. So this file only ever
-- reads `ictcell` — nothing here deletes or alters a row over there.
--
-- CONSEQUENCE TO KNOW ABOUT: the two schemas stop tracking each other the
-- instant the new code is deployed. Adding an IP to the `ictcell` row for
-- `/ext-api/wow-attendance/verify` will have NO effect on this service any
-- more; the row that matters is the `attendance` one. docs/DEPLOYMENT.md and
-- docs/nfc_card.md have been repointed accordingly.
--
-- TABLE SHAPES are copied from the LIVE `ictcell` tables, not from this repo's
-- own `sql/000_ext_api_infra.sql` — that file was a looser reconstruction for
-- fresh databases (everything nullable `text`, plus a `created_at` on the
-- allow-list that production does not have). Matching production keeps a row
-- copied from one side insertable on the other, and keeps the middleware's
-- binds (`status_code as i16`) honest. `sql/000` has been brought into line.
--
-- ORDER: apply after 000; safe in any order relative to 001-004. Idempotent —
-- every statement is `IF NOT EXISTS`, a `NOT EXISTS` guard, or a no-op on the
-- second run, so re-running this file changes nothing.
-- =====================================================================

CREATE SCHEMA IF NOT EXISTS attendance;

-- ── Per-endpoint IP allow-list ───────────────────────────────────────
-- `endpoint` is matched against the FULL request path, e.g.
-- '/ext-api/wow-attendance/verify' — not a prefix. Every endpoint the service
-- exposes needs its own row here, or every call to it returns 403.
--
-- A literal `'*'` in `ip_address` means "any IP" and skips the check for that
-- endpoint; the two NFC paths use it (see section 6 of sql/004_nfc_card.sql).
CREATE TABLE IF NOT EXISTS attendance.ext_api_allowed_ips (
    id         serial                 PRIMARY KEY,
    endpoint   character varying(255) NOT NULL,
    ip_address text[]                 NOT NULL DEFAULT '{}',
    is_active  boolean                NOT NULL DEFAULT true
);

COMMENT ON TABLE attendance.ext_api_allowed_ips IS
    'Per-endpoint IP allow-list read by ExtAuthMiddleware on every /ext-api call. This service''s copy; duerp-api keeps its own in ictcell.';

-- The middleware filters on endpoint + is_active on every single request.
CREATE INDEX IF NOT EXISTS ext_api_allowed_ips_endpoint_active_idx
    ON attendance.ext_api_allowed_ips (endpoint)
    WHERE is_active;

-- ── Request / response log ───────────────────────────────────────────
-- Written fire-and-forget by ApiLogger, so a failed insert never affects the
-- response. request_body/response_body are jsonb; a non-JSON body (multipart
-- upload) lands as a JSON string rather than failing the insert.
CREATE TABLE IF NOT EXISTS attendance.ext_api_call_logs (
    id            bigserial              PRIMARY KEY,
    endpoint      character varying(255) NOT NULL,
    method        character varying(10)  NOT NULL,
    request_body  jsonb,
    response_body jsonb,
    status_code   smallint               NOT NULL,
    duration_ms   integer                NOT NULL,
    client_ip     character varying(45),
    user_agent    text,
    error_message text,
    created_at    timestamptz            NOT NULL DEFAULT now()
);

COMMENT ON TABLE attendance.ext_api_call_logs IS
    'Request/response log appended by ApiLogger for /ext-api calls. This service''s copy; duerp-api keeps its own in ictcell.';

-- Recent-first review, and "what has been failing" triage.
CREATE INDEX IF NOT EXISTS ext_api_call_logs_created_idx
    ON attendance.ext_api_call_logs (created_at DESC);
CREATE INDEX IF NOT EXISTS ext_api_call_logs_endpoint_status_idx
    ON attendance.ext_api_call_logs (endpoint, status_code);

-- =====================================================================
-- Backfill from ictcell — only this service's own rows
--
-- Wrapped in a DO block guarded on `to_regclass`: on a fresh database there is
-- no `ictcell` schema to read from, and the whole section must then be skipped
-- rather than abort the script. `sql/000_ext_api_infra.sql` seeds such a
-- database instead.
--
-- WHICH ROWS ARE "THIS SERVICE'S": the endpoints main.rs actually registers,
-- i.e. everything under `/ext-api/wow-attendance/` and `/ext-api/nfc-card/`.
-- Matched by prefix rather than listed, so an endpoint added between writing
-- this file and running it still comes across.
-- =====================================================================

DO $$
DECLARE
    moved_ips  integer := 0;
    merged_ips integer := 0;
    moved_logs bigint  := 0;
BEGIN
    IF to_regclass('ictcell.ext_api_allowed_ips') IS NULL THEN
        RAISE NOTICE 'ictcell.ext_api_allowed_ips absent — fresh database, nothing to back-fill';
    ELSE
        -- 1 · Endpoints with no row in `attendance` yet: copy the ictcell row
        --     verbatim, real IPs and `is_active` included.
        --
        --     NOT `ON CONFLICT`: the production table's unique constraint is on
        --     (endpoint, ip_address), not on `endpoint` alone, so an ON CONFLICT
        --     target naming `endpoint` aborts the script there. The NOT EXISTS
        --     guard is portable and leaves an already-configured row alone.
        INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address, is_active)
        SELECT s.endpoint, s.ip_address, s.is_active
          FROM ictcell.ext_api_allowed_ips s
         WHERE (s.endpoint LIKE '/ext-api/wow-attendance/%'
             OR s.endpoint LIKE '/ext-api/nfc-card/%')
           AND NOT EXISTS (
               SELECT 1 FROM attendance.ext_api_allowed_ips t
                WHERE t.endpoint = s.endpoint
           );
        GET DIAGNOSTICS moved_ips = ROW_COUNT;

        -- 2 · Endpoints that DO already have a row — which is what `sql/000`
        --     leaves behind if it ran first, seeded with localhost only. Union
        --     the real IPs in rather than skipping, or the production clients
        --     (160.202.144.124, the campus ranges, the NFC `'*'`) are silently
        --     dropped on the switchover and every real call starts 403ing.
        --
        --     `s.ip_address <@ t.ip_address` is "the source adds nothing new",
        --     so a second run updates no rows. `is_active` is deliberately not
        --     touched here: an endpoint switched off in `attendance` was switched
        --     off on purpose.
        UPDATE attendance.ext_api_allowed_ips t
           SET ip_address = ARRAY(SELECT DISTINCT unnest(t.ip_address || s.ip_address))
          FROM ictcell.ext_api_allowed_ips s
         WHERE s.endpoint = t.endpoint
           AND (s.endpoint LIKE '/ext-api/wow-attendance/%'
             OR s.endpoint LIKE '/ext-api/nfc-card/%')
           AND NOT (s.ip_address <@ t.ip_address);
        GET DIAGNOSTICS merged_ips = ROW_COUNT;

        RAISE NOTICE 'allow-list: % row(s) copied, % row(s) had IPs merged in', moved_ips, merged_ips;
    END IF;

    IF to_regclass('ictcell.ext_api_call_logs') IS NULL THEN
        RAISE NOTICE 'ictcell.ext_api_call_logs absent — fresh database, no history to back-fill';
    ELSE
        -- 3 · One-shot history backfill, and it runs ONLY while the target is
        --     still empty (`NOT EXISTS (SELECT 1 FROM ...)`).
        --
        --     That guard is what makes re-running this file safe: the rows carry
        --     no natural key, so there is nothing to dedupe on, and a second
        --     unguarded pass would simply double the history. The cost is the
        --     ordering requirement below.
        --
        --     RUN THIS BEFORE DEPLOYING THE NEW BINARY. Once the new code is
        --     live it starts appending here, the table stops being empty, and
        --     this backfill quietly becomes a no-op — leaving the pre-switchover
        --     history reachable only in `ictcell`. If that has already happened
        --     and you want the history anyway, the same INSERT with the last
        --     line dropped and `AND s.created_at < '<switchover timestamp>'`
        --     added in its place does the job.
        --
        --     `id` is NOT carried over: it is a bigserial with no meaning
        --     outside its own table, and preserving it would collide with rows
        --     the service has already written. Ordering the copy by `created_at`
        --     keeps the new ids chronological, which is the only property
        --     anything actually reads them for.
        INSERT INTO attendance.ext_api_call_logs
            (endpoint, method, request_body, response_body,
             status_code, duration_ms, client_ip, user_agent, error_message, created_at)
        SELECT s.endpoint, s.method, s.request_body, s.response_body,
               s.status_code, s.duration_ms, s.client_ip, s.user_agent, s.error_message, s.created_at
          FROM ictcell.ext_api_call_logs s
         WHERE (s.endpoint LIKE '/ext-api/wow-attendance/%'
             OR s.endpoint LIKE '/ext-api/nfc-card/%')
           AND NOT EXISTS (SELECT 1 FROM attendance.ext_api_call_logs)
         ORDER BY s.created_at, s.id;
        GET DIAGNOSTICS moved_logs = ROW_COUNT;

        IF moved_logs = 0 THEN
            RAISE NOTICE 'call log: nothing copied — attendance.ext_api_call_logs was not empty (backfill already done, or the service is already writing here)';
        ELSE
            RAISE NOTICE 'call log: % row(s) copied', moved_logs;
        END IF;
    END IF;
END $$;

-- =====================================================================
-- Verify — run these by hand after applying
--
--   -- 12 rows, with the SAME ip_address arrays as the ictcell originals:
--   SELECT endpoint, is_active, ip_address
--     FROM attendance.ext_api_allowed_ips
--    ORDER BY endpoint;
--
--   -- any endpoint whose IPs did not come across in full (expect 0 rows):
--   SELECT s.endpoint, s.ip_address AS ictcell, t.ip_address AS attendance
--     FROM ictcell.ext_api_allowed_ips s
--     LEFT JOIN attendance.ext_api_allowed_ips t USING (endpoint)
--    WHERE (s.endpoint LIKE '/ext-api/wow-attendance/%'
--        OR s.endpoint LIKE '/ext-api/nfc-card/%')
--      AND (t.endpoint IS NULL OR NOT (s.ip_address <@ t.ip_address));
--
--   -- history landed, and nothing of duerp-api's came with it (expect 0):
--   SELECT count(*) FROM attendance.ext_api_call_logs;
--   SELECT count(*) FROM attendance.ext_api_call_logs
--    WHERE endpoint NOT LIKE '/ext-api/wow-attendance/%'
--      AND endpoint NOT LIKE '/ext-api/nfc-card/%';
--
--   -- after a smoke-test call, the newest row must be in `attendance`:
--   SELECT id, endpoint, status_code, created_at
--     FROM attendance.ext_api_call_logs ORDER BY id DESC LIMIT 5;
-- =====================================================================
