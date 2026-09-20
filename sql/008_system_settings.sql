-- =====================================================================
-- Admin-managed runtime settings — the NFC face-verification toggle
--
-- Backs GET|PUT /admin-api/settings/nfc-face-verify (src/routes/settings.rs).
--
-- WHAT THIS CHANGES ABOUT THE FACE GATE
--   Until now the gate was configured by ENVIRONMENT VARIABLE:
--   `NFC_FACE_VERIFY_URL` set meant "verify every save", unset meant "refuse
--   every save" (fail closed), and changing either needed an edit to `.env`
--   plus a restart. There was no ON/OFF switch at all — "off" was spelled
--   "unset the URL", which also happens to be how a misconfiguration looks.
--
--   These two rows make it an operator decision instead:
--     NFC_FACE_VERIFY      'ON' | 'OFF'   — explicit, and separate from "is it
--                                           configured", which is the whole
--                                           point of having a toggle
--     NFC_FACE_VERIFY_URL  the endpoint   — '' until somebody sets one
--
--   `.env` remains the FALLBACK: a row that has never been set reads through
--   to the environment variable of the same name, so applying this file
--   changes no behaviour until somebody uses the API. See
--   `src/routes/nfc_card.rs`.
--
-- OFF MEANS SAVES PROCEED UNVERIFIED. That is what a toggle is for, and it is
-- worth being blunt about: with `NFC_FACE_VERIFY = 'OFF'`, `save_card_info`
-- writes a card mapping without comparing the card photo to the selfie. It is
-- the same thing as an outage of the face service, except deliberate and
-- attributable — which is why every flip is audited with the admin who made
-- it.
--
-- Idempotent. Requires sql/006_access_control.sql (the permission this API is
-- gated on lives in `attendance.resources`).
--
--     psql "$DATABASE_URL" -f sql/008_system_settings.sql
-- =====================================================================

CREATE SCHEMA IF NOT EXISTS attendance;

-- ---------------------------------------------------------------------
-- 1 · The settings themselves
--
-- Key/value, deliberately: these are operator switches read by name, not a
-- typed configuration object. A column per setting would mean a migration
-- every time somebody wants one more.
--
-- `updated_by` is the token's `sub` — `app_users.person_id`, which is UNIQUE
-- and is the id every other audit trail in this schema records. ON DELETE SET
-- NULL: losing the account must not lose the setting.
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.system_settings (
    key        varchar(64)  PRIMARY KEY,
    value      text         NOT NULL DEFAULT '',
    -- Free text for the admin screen, so a switch nobody recognises is not a
    -- mystery to the next operator.
    label      text,
    updated_by bigint       REFERENCES attendance.app_users(person_id) ON DELETE SET NULL,
    updated_at timestamptz  NOT NULL DEFAULT now()
);

COMMENT ON TABLE attendance.system_settings IS
    'Admin-managed runtime switches. A missing or empty row falls back to the environment variable of the same name.';

-- ---------------------------------------------------------------------
-- 2 · Every change, with both sides
--
-- One row PER KEY CHANGED, not per request: "when did face verification get
-- turned off, and by whom" is the question this exists for, and it should not
-- require unpicking a combined record to answer.
--
-- A no-op write leaves no row. An audit trail that records changes which did
-- not happen trains people to ignore it.
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.system_settings_audit (
    id          bigserial   PRIMARY KEY,
    setting_key varchar(64) NOT NULL,
    old_value   text,
    new_value   text,
    updated_by  bigint,
    client_ip   varchar(45),
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS system_settings_audit_key_idx
    ON attendance.system_settings_audit (setting_key, created_at DESC);

-- ---------------------------------------------------------------------
-- 3 · Seed — only if absent, never overwriting a configured value
--
-- `OFF` and `''` are the safe starting point: the gate stays exactly as it is
-- (env-configured) until an operator touches it, and an accidental re-run of
-- this file cannot silently disable face verification.
-- ---------------------------------------------------------------------

INSERT INTO attendance.system_settings (key, value, label)
SELECT v.key, v.value, v.label
  FROM (VALUES
    ('NFC_FACE_VERIFY',     'OFF', 'Compare the card photo with the selfie before saving a card'),
    ('NFC_FACE_VERIFY_URL', '',    'Face-match service endpoint, e.g. https://face.du.ac.bd/verify')
  ) AS v(key, value, label)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.system_settings s WHERE s.key = v.key
 );

-- ---------------------------------------------------------------------
-- 4 · Read
--
-- Returns the pair the API and the face gate both want, plus who last touched
-- them. An absent row reads as the seeded default rather than NULL, so a
-- caller never has to decide what a missing setting means.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.nfc_face_verify_get()
RETURNS jsonb
LANGUAGE sql STABLE
AS $function$
    SELECT jsonb_build_object(
        'status',  'success',
        'message', 'Settings',
        'data', jsonb_build_object(
            'nfc_face_verify',     coalesce((SELECT upper(btrim(value)) FROM attendance.system_settings
                                              WHERE key = 'NFC_FACE_VERIFY'), 'OFF'),
            'nfc_face_verify_url', coalesce((SELECT btrim(value) FROM attendance.system_settings
                                              WHERE key = 'NFC_FACE_VERIFY_URL'), ''),
            'updated_by',          (SELECT updated_by FROM attendance.system_settings
                                     WHERE key = 'NFC_FACE_VERIFY'),
            'updated_at',          (SELECT max(updated_at) FROM attendance.system_settings
                                     WHERE key IN ('NFC_FACE_VERIFY', 'NFC_FACE_VERIFY_URL'))));
$function$;

-- ---------------------------------------------------------------------
-- 4b · What counts as an acceptable endpoint
--
-- **https anywhere, or http to a private address.**
--
-- The strict https rule was right for an endpoint on the public internet and
-- wrong for this deployment: the face-match service runs at
-- `http://10.224.224.101:8089/verify`, on the campus network, with no
-- certificate — so https-only meant the switch could never be turned on for
-- the service it exists to control.
--
-- What is allowed over plaintext is therefore RFC1918, loopback, and
-- `localhost` — addresses that are not routable from outside the campus. That
-- is a smaller hole than it looks, but it IS a hole: the images crossing that
-- link are photographs of students' faces, and anyone on the same LAN can read
-- them. Prefer https the day the face service can offer it; this exists so an
-- internal service is usable, not because plaintext is fine.
--
-- Public hostnames and public IPs still require https, which is the case that
-- actually matters — a typo'd `http://face.du.ac.bd/verify` would otherwise
-- put those photographs on the open internet in clear.
-- ---------------------------------------------------------------------

-- Host part of a URL: everything after the scheme, before a port or a path.
CREATE OR REPLACE FUNCTION attendance.url_host(p_url text)
RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $function$
    SELECT lower(split_part(split_part(regexp_replace(coalesce(p_url, ''), '^[A-Za-z]+://', ''), '/', 1), ':', 1))
$function$;

-- Is this host on a network that cannot be reached from outside?
-- 
-- IPv4 literals only, plus `localhost`. A NAME that happens to resolve to a
-- private address does not count: this function cannot resolve anything, and
-- a name whose DNS an attacker controls is exactly how "internal" stops being
-- internal.
CREATE OR REPLACE FUNCTION attendance.is_private_host(p_host text)
RETURNS boolean
LANGUAGE plpgsql IMMUTABLE PARALLEL SAFE
AS $function$
DECLARE
    v_host text := lower(btrim(coalesce(p_host, '')));
    v_oct  text := '(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])';
BEGIN
    IF v_host = 'localhost' THEN
        RETURN true;
    END IF;

    RETURN v_host ~ ('^10\.' || v_oct || '\.' || v_oct || '\.' || v_oct || '$')
        OR v_host ~ ('^172\.(1[6-9]|2[0-9]|3[01])\.' || v_oct || '\.' || v_oct || '$')
        OR v_host ~ ('^192\.168\.' || v_oct || '\.' || v_oct || '$')
        OR v_host ~ ('^127\.' || v_oct || '\.' || v_oct || '\.' || v_oct || '$');
END;
$function$;

COMMENT ON FUNCTION attendance.is_private_host(text) IS
    'RFC1918, loopback or localhost — the hosts allowed to be reached over plaintext http.';

-- ---------------------------------------------------------------------
-- 5 · Write
--
-- One call for both keys, because they are one decision: turning the gate on
-- without an endpoint to call is not a state worth being able to reach, and
-- the 422 below exists to say so.
--
-- `p_url` semantics, which the API documents to its callers:
--     NULL  — leave the stored URL alone (the "just flip the switch" case)
--     ''    — clear it
--     value — replace it, if it validates
--
-- VALIDATION ORDER MATTERS. The enum is checked before the URL, and both
-- before the ON-without-a-URL rule, so a request with two things wrong reports
-- the one the caller can act on first.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.nfc_face_verify_save(
    p_enabled   varchar,
    p_url       varchar DEFAULT NULL,
    p_actor     bigint  DEFAULT NULL,
    p_client_ip varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_enabled   varchar := upper(btrim(coalesce(p_enabled, '')));
    v_url       varchar := btrim(coalesce(p_url, ''));
    v_url_given boolean := p_url IS NOT NULL;
    v_old_on    text;
    v_old_url   text;
    v_new_url   text;
BEGIN
    IF v_enabled NOT IN ('ON', 'OFF') THEN
        RETURN jsonb_build_object('status','error','code','invalid_value',
            'message','nfc_face_verify must be exactly "ON" or "OFF"',
            'data','{}'::jsonb);
    END IF;

    -- A deliberately strict shape: scheme, a host, an optional port and path,
    -- and no whitespace. Anything looser and a typo like "https:/host" is
    -- stored and only discovered when a save fails at the desk.
    --
    -- http is accepted ONLY to a private address (section 4b) — that is the
    -- campus face service, which has no certificate. Everything else is https.
    IF v_url_given AND v_url <> '' THEN
        IF v_url !~ '^https?://[A-Za-z0-9][A-Za-z0-9._-]*(:[0-9]{1,5})?(/[^[:space:]]*)?$' THEN
            RETURN jsonb_build_object('status','error','code','invalid_url',
                'message','nfc_face_verify_url must be a valid https:// URL (or http:// to a private address)',
                'data','{}'::jsonb);
        END IF;

        IF v_url ~* '^http://' AND NOT attendance.is_private_host(attendance.url_host(v_url)) THEN
            RETURN jsonb_build_object('status','error','code','invalid_url',
                'message','http:// is only allowed for a private address (10.x, 172.16-31.x, 192.168.x, 127.x or localhost) — use https:// for anything else',
                'data', jsonb_build_object('host', attendance.url_host(v_url)));
        END IF;
    END IF;

    SELECT upper(btrim(value)) INTO v_old_on  FROM attendance.system_settings WHERE key = 'NFC_FACE_VERIFY';
    SELECT btrim(value)        INTO v_old_url FROM attendance.system_settings WHERE key = 'NFC_FACE_VERIFY_URL';

    -- What the URL will be after this request: the new one if one was sent,
    -- otherwise whatever is already stored.
    v_new_url := CASE WHEN v_url_given THEN v_url ELSE coalesce(v_old_url, '') END;

    -- The 422. Turning the gate ON with nothing to call would mean every save
    -- failing at the card desk with a 503, which reads as "the system is
    -- broken" rather than "somebody enabled a feature that is not configured".
    IF v_enabled = 'ON' AND coalesce(v_new_url, '') = '' THEN
        RETURN jsonb_build_object('status','error','code','url_required',
            'message','Cannot enable face verification without NFC_FACE_VERIFY_URL',
            'data','{}'::jsonb);
    END IF;

    -- Both rows, one statement each, inside the function's own transaction:
    -- a caller can never observe the toggle ON with the old URL, or vice versa.
    INSERT INTO attendance.system_settings (key, value, updated_by, updated_at)
    VALUES ('NFC_FACE_VERIFY', v_enabled, p_actor, now())
    ON CONFLICT (key) DO UPDATE
       SET value = EXCLUDED.value, updated_by = EXCLUDED.updated_by, updated_at = now();

    IF v_url_given THEN
        INSERT INTO attendance.system_settings (key, value, updated_by, updated_at)
        VALUES ('NFC_FACE_VERIFY_URL', v_url, p_actor, now())
        ON CONFLICT (key) DO UPDATE
           SET value = EXCLUDED.value, updated_by = EXCLUDED.updated_by, updated_at = now();
    END IF;

    -- One audit row per key that actually moved.
    IF coalesce(v_old_on, '') IS DISTINCT FROM v_enabled THEN
        INSERT INTO attendance.system_settings_audit (setting_key, old_value, new_value, updated_by, client_ip)
        VALUES ('NFC_FACE_VERIFY', v_old_on, v_enabled, p_actor, p_client_ip);
    END IF;

    IF v_url_given AND coalesce(v_old_url, '') IS DISTINCT FROM v_url THEN
        INSERT INTO attendance.system_settings_audit (setting_key, old_value, new_value, updated_by, client_ip)
        VALUES ('NFC_FACE_VERIFY_URL', v_old_url, v_url, p_actor, p_client_ip);
    END IF;

    RETURN jsonb_build_object('status','success','message','Settings updated',
        'data', jsonb_build_object(
            'nfc_face_verify',     v_enabled,
            'nfc_face_verify_url', v_new_url));
END;
$function$;

COMMENT ON FUNCTION attendance.nfc_face_verify_save(varchar, varchar, bigint, varchar) IS
    'Set the face-verification toggle and endpoint. Refuses ON without a URL (url_required), a non-ON/OFF value (invalid_value), or a non-https URL (invalid_url).';

-- ---------------------------------------------------------------------
-- 6 · Who may change it
--
-- Its own permission rather than reusing `admin.roles.manage`: turning face
-- verification off is an operations decision, and the person who administers
-- roles is not necessarily the person who should be able to make it.
-- ---------------------------------------------------------------------

INSERT INTO attendance.resources (key, name, category) VALUES
    ('admin.settings.manage', 'Admin · System settings', 'Administration')
ON CONFLICT (key) DO NOTHING;

INSERT INTO attendance.role_permissions (role_id, resource_id)
SELECT r.id, res.id
  FROM attendance.roles r, attendance.resources res
 WHERE r.key IN ('admin', 'superadmin')
   AND res.key = 'admin.settings.manage'
ON CONFLICT DO NOTHING;

-- Seeded `enforce = true`, like the other admin API (sql/007): these endpoints
-- have no existing callers to break, and an unenforced switch for disabling
-- face verification is worse than no switch.
-- TWO PATHS, one endpoint. The handlers are mounted under both `/ext-api` and
-- `/admin-api` (src/main.rs): the second is the documented contract, the first
-- is the one the production gateway actually proxies today. The middleware
-- matches on the exact path, so each needs its own row, and they carry the
-- same permission because they are the same capability.
INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, 'admin.settings.manage', true, v.note
  FROM (VALUES
    ('/admin-api/settings/nfc-face-verify',
     'GET reads the face-verification toggle; PUT changes it. Both methods, one row — the allow-list and this map key on path, not method.'),
    ('/ext-api/settings/nfc-face-verify',
     'The same endpoint under the prefix the gateway proxies. Remove this row only once /admin-api has a proxy rule AND every client has moved.')
  ) AS v(endpoint, note)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint
 );

-- `ExtAuthMiddleware` guards `/admin-api` exactly as it guards `/ext-api`
-- (see src/main.rs), so this path needs its allow-list row or it is a 403
-- before the handler runs.
INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES
    ('/admin-api/settings/nfc-face-verify'),
    ('/ext-api/settings/nfc-face-verify')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint
 );
