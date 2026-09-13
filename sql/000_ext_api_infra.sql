-- =====================================================================
-- ext-api gate: per-endpoint IP allow-list + call log
--
-- These two tables back `src/middleware/ext_auth_middleware.rs` (IP check)
-- and `src/middleware/api_logger.rs` (request/response log).
--
-- THEY LIVE IN `attendance`, NOT `ictcell`. They used to be read out of the
-- shared `ictcell` schema that duerp-api owns; sql/005_ext_api_attendance_schema.sql
-- moved this service onto its own copies and explains why. `ictcell` still has
-- its own pair and duerp-api still uses them — the two are now independent, so
-- an IP added on the `ictcell` side has no effect here.
--
-- On a database that already carries the `ictcell` tables, apply 005 as well:
-- this file only seeds localhost, and 005 is what brings the real client IPs
-- and the call history across.
--
-- The column shapes match the live `ictcell` tables this service ran against
-- for a year, so a row copied from one side is insertable on the other. That
-- matters for 005, which copies between them. The middleware's requirements:
--   * ext_api_allowed_ips.ip_address is an ARRAY — the check is
--     `$2 = ANY(ip_address)`, one row per endpoint holding many IPs.
--     A literal `'*'` in that array means "any IP" and skips the check for
--     that endpoint; sql/004_nfc_card.sql uses it for the two NFC paths.
--   * status_code is smallint — api_logger binds `status_code as i16`.
--
-- Apply first: this file, then 001 -> 002 -> 003 -> 004 -> 005.
-- =====================================================================

CREATE SCHEMA IF NOT EXISTS attendance;

-- ── Per-endpoint IP allow-list ───────────────────────────────────────
-- `endpoint` is matched against the FULL request path, e.g.
-- '/ext-api/wow-attendance/verify' — not a prefix. Every endpoint the
-- service exposes needs its own row, or every call to it returns 403.
CREATE TABLE IF NOT EXISTS attendance.ext_api_allowed_ips (
    id         serial                 PRIMARY KEY,
    endpoint   character varying(255) NOT NULL,
    ip_address text[]                 NOT NULL DEFAULT '{}',
    is_active  boolean                NOT NULL DEFAULT true
);

-- The middleware filters on endpoint + is_active on every single request.
CREATE INDEX IF NOT EXISTS ext_api_allowed_ips_endpoint_active_idx
    ON attendance.ext_api_allowed_ips (endpoint)
    WHERE is_active;

-- ── Request / response log ───────────────────────────────────────────
-- Written fire-and-forget by ApiLogger, so a failed insert never affects the
-- response. request_body/response_body are jsonb; non-JSON bodies (multipart
-- uploads) land as a JSON string rather than failing the insert.
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

-- Recent-first review, and "what has been failing" triage.
CREATE INDEX IF NOT EXISTS ext_api_call_logs_created_idx
    ON attendance.ext_api_call_logs (created_at DESC);
CREATE INDEX IF NOT EXISTS ext_api_call_logs_endpoint_status_idx
    ON attendance.ext_api_call_logs (endpoint, status_code);

-- ── Seed the allow-list for this service's endpoints ─────────────────
-- Localhost only. Add real client IPs per endpoint before going live (or `'*'`
-- for any IP, as sql/004_nfc_card.sql does for the two NFC paths):
--   UPDATE attendance.ext_api_allowed_ips
--      SET ip_address = ip_address || '{203.0.113.10}'
--    WHERE endpoint = '/ext-api/wow-attendance/verify';
--
-- On a database migrated off `ictcell`, sql/005 unions the real IPs into these
-- rows, so applying this file first and 005 second loses nothing.
--
-- NOT `ON CONFLICT`: the `ictcell` table these shapes came from has its unique
-- constraint on (endpoint, ip_address), not on `endpoint` alone, so an ON
-- CONFLICT target naming `endpoint` aborts the script there. The NOT EXISTS
-- guard is portable and leaves already-configured rows (with their real IPs)
-- untouched.
INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{127.0.0.1,::1}'::text[]
  FROM (VALUES
    ('/ext-api/wow-attendance/enroll'),
    ('/ext-api/wow-attendance/verify'),
    ('/ext-api/wow-attendance/check'),
    ('/ext-api/wow-attendance/enrolled'),
    ('/ext-api/wow-attendance/reports/by-date'),
    ('/ext-api/wow-attendance/reports/by-person'),
    ('/ext-api/wow-attendance/ssl_image_verfiy'),
    ('/ext-api/wow-attendance/mapping-save'),
    -- Step-log readers. They take a bearer token on top of this list, but the
    -- middleware runs FIRST: without a row here every call to them is a 403
    -- before the token is ever looked at.
    ('/ext-api/wow-attendance/logs/login'),
    ('/ext-api/wow-attendance/logs/attendance'),
    -- NFC card <-> student mapping. Gated by the app credentials + this list
    -- plus a bearer token. See docs/nfc_card.md.
    ('/ext-api/nfc-card/get_card_info'),
    ('/ext-api/nfc-card/save_card_info')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_allowed_ips a
     WHERE a.endpoint = v.endpoint
 );
