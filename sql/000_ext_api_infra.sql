-- =====================================================================
-- ext-api gate: per-endpoint IP allow-list + call log
--
-- These two tables back `src/middleware/ext_auth_middleware.rs` (IP check)
-- and `src/middleware/api_logger.rs` (request/response log). They already
-- exist in the shared `ictcell` schema that duerp-api uses, so applying this
-- file against the production database is a no-op — it exists so a FRESH
-- database (a dev box, a test instance) can stand this service up alone.
--
-- The column shapes are the ones the middleware queries require:
--   * ext_api_allowed_ips.ip_address is an ARRAY — the check is
--     `$2 = ANY(ip_address)`, one row per endpoint holding many IPs.
--   * status_code is smallint — api_logger binds `status_code as i16`.
--
-- Apply first: this file, then 001 -> 002 -> 003.
-- =====================================================================

CREATE SCHEMA IF NOT EXISTS ictcell;

-- ── Per-endpoint IP allow-list ───────────────────────────────────────
-- `endpoint` is matched against the FULL request path, e.g.
-- '/ext-api/wow-attendance/verify' — not a prefix. Every endpoint the
-- service exposes needs its own row, or every call to it returns 403.
CREATE TABLE IF NOT EXISTS ictcell.ext_api_allowed_ips (
    id         serial PRIMARY KEY,
    endpoint   text    NOT NULL,
    ip_address text[]  NOT NULL DEFAULT '{}',
    is_active  boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- The middleware filters on endpoint + is_active on every single request.
CREATE INDEX IF NOT EXISTS ext_api_allowed_ips_endpoint_active_idx
    ON ictcell.ext_api_allowed_ips (endpoint)
    WHERE is_active;

-- ── Request / response log ───────────────────────────────────────────
-- Written fire-and-forget by ApiLogger, so a failed insert never affects the
-- response. request_body/response_body are jsonb; non-JSON bodies (multipart
-- uploads) land as a JSON string or null rather than failing the insert.
CREATE TABLE IF NOT EXISTS ictcell.ext_api_call_logs (
    id            bigserial PRIMARY KEY,
    endpoint      text,
    method        text,
    request_body  jsonb,
    response_body jsonb,
    status_code   smallint,
    duration_ms   integer,
    client_ip     text,
    user_agent    text,
    error_message text,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- Recent-first review, and "what has been failing" triage.
CREATE INDEX IF NOT EXISTS ext_api_call_logs_created_idx
    ON ictcell.ext_api_call_logs (created_at DESC);
CREATE INDEX IF NOT EXISTS ext_api_call_logs_endpoint_status_idx
    ON ictcell.ext_api_call_logs (endpoint, status_code);

-- ── Seed the allow-list for this service's endpoints ─────────────────
-- Localhost only. Add real client IPs per endpoint before going live:
--   UPDATE ictcell.ext_api_allowed_ips
--      SET ip_address = ip_address || '{203.0.113.10}'
--    WHERE endpoint = '/ext-api/wow-attendance/verify';
--
-- NOT `ON CONFLICT`: on the shared production database this table predates this
-- file and has no unique constraint on `endpoint`, so an ON CONFLICT target
-- would abort the whole script there. The NOT EXISTS guard is portable and
-- leaves already-configured rows (with their real IPs) untouched.
INSERT INTO ictcell.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{127.0.0.1,::1}'::text[]
  FROM (VALUES
    ('/ext-api/wow-attendance/enroll'),
    ('/ext-api/wow-attendance/verify'),
    ('/ext-api/wow-attendance/check'),
    ('/ext-api/wow-attendance/enrolled'),
    ('/ext-api/wow-attendance/reports/by-date'),
    ('/ext-api/wow-attendance/reports/by-person'),
    ('/ext-api/wow-attendance/ssl_image_verfiy'),
    ('/ext-api/wow-attendance/mapping-save')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM ictcell.ext_api_allowed_ips a
     WHERE a.endpoint = v.endpoint
 );