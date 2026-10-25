-- =====================================================================
-- Admin-managed runtime settings — the "Match with EIS" toggle
--
-- Backs GET|PUT /admin-api/settings/eis-match (src/routes/settings.rs), the
-- second card on the admin panel's settings/face-verification screen.
--
--     EIS_MATCH      'ON' | 'OFF'  — compare the card-save selfie with the EIS
--                                    studentship photo
--     EIS_MATCH_URL  the BASE URL  — the NFC card number is the student's EIS
--                                    token and is appended to it (or replaces
--                                    `{card_number}`), see src/routes/eis_match.rs
--
-- Same table, same audit trail and same validation rules as the NFC face
-- toggle in sql/008_system_settings.sql — this file only adds the two keys and
-- their get/save pair. Unlike that toggle there is NO environment fallback:
-- nothing read EIS settings from `.env` before this, so the stored row is the
-- whole truth from the start, and the seed is OFF.
--
-- Idempotent. Requires sql/008_system_settings.sql.
--
--     psql "$DATABASE_URL" -f sql/009_eis_match_settings.sql
-- =====================================================================

-- ---------------------------------------------------------------------
-- 1 · Seed — only if absent, never overwriting a configured value
-- ---------------------------------------------------------------------

INSERT INTO attendance.system_settings (key, value, label)
SELECT v.key, v.value, v.label
  FROM (VALUES
    ('EIS_MATCH',     'OFF', 'Match with EIS — compare the selfie with the EIS studentship photo'),
    ('EIS_MATCH_URL', 'https://academic.eis.du.ac.bd/en/studentship/',
                           'EIS studentship base URL — the card number is appended')
  ) AS v(key, value, label)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.system_settings s WHERE s.key = v.key
 );

-- The first version of this file seeded ONE student's full page as the URL.
-- The card number is appended now, so that value would build
-- `…/Qvuec1c774mKd4DNt/<card>`. Rewritten only while it is still exactly the
-- old seed — a URL an admin chose is left alone.
UPDATE attendance.system_settings
   SET value = 'https://academic.eis.du.ac.bd/en/studentship/',
       label = 'EIS studentship base URL — the card number is appended'
 WHERE key = 'EIS_MATCH_URL'
   AND value = 'https://academic.eis.du.ac.bd/en/studentship/Qvuec1c774mKd4DNt';
UPDATE attendance.system_settings
   SET label = 'Match with EIS — compare the selfie with the EIS studentship photo'
 WHERE key = 'EIS_MATCH';

-- ---------------------------------------------------------------------
-- 2 · Read
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.eis_match_get()
RETURNS jsonb
LANGUAGE sql STABLE
AS $function$
    SELECT jsonb_build_object(
        'status',  'success',
        'message', 'Settings',
        'data', jsonb_build_object(
            'eis_match',     coalesce((SELECT upper(btrim(value)) FROM attendance.system_settings
                                        WHERE key = 'EIS_MATCH'), 'OFF'),
            'eis_match_url', coalesce((SELECT btrim(value) FROM attendance.system_settings
                                        WHERE key = 'EIS_MATCH_URL'), ''),
            'updated_by',    (SELECT updated_by FROM attendance.system_settings
                               WHERE key = 'EIS_MATCH'),
            'updated_at',    (SELECT max(updated_at) FROM attendance.system_settings
                               WHERE key IN ('EIS_MATCH', 'EIS_MATCH_URL'))));
$function$;

-- ---------------------------------------------------------------------
-- 3 · Write
--
-- `p_url` semantics, identical to nfc_face_verify_save:
--     NULL  — leave the stored URL alone (the "just flip the switch" case)
--     ''    — clear it
--     value — replace it, if it validates
--
-- URL rule is the shared one from 008 section 4b: https anywhere, http only
-- to a private address. EIS is public, so in practice that means https.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.eis_match_save(
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
            'message','eis_match must be exactly "ON" or "OFF"',
            'data','{}'::jsonb);
    END IF;

    IF v_url_given AND v_url <> '' THEN
        IF v_url !~ '^https?://[A-Za-z0-9][A-Za-z0-9._-]*(:[0-9]{1,5})?(/[^[:space:]]*)?$' THEN
            RETURN jsonb_build_object('status','error','code','invalid_url',
                'message','eis_match_url must be a valid https:// URL (or http:// to a private address)',
                'data','{}'::jsonb);
        END IF;

        IF v_url ~* '^http://' AND NOT attendance.is_private_host(attendance.url_host(v_url)) THEN
            RETURN jsonb_build_object('status','error','code','invalid_url',
                'message','http:// is only allowed for a private address (10.x, 172.16-31.x, 192.168.x, 127.x or localhost) — use https:// for anything else',
                'data', jsonb_build_object('host', attendance.url_host(v_url)));
        END IF;
    END IF;

    SELECT upper(btrim(value)) INTO v_old_on  FROM attendance.system_settings WHERE key = 'EIS_MATCH';
    SELECT btrim(value)        INTO v_old_url FROM attendance.system_settings WHERE key = 'EIS_MATCH_URL';

    v_new_url := CASE WHEN v_url_given THEN v_url ELSE coalesce(v_old_url, '') END;

    IF v_enabled = 'ON' AND coalesce(v_new_url, '') = '' THEN
        RETURN jsonb_build_object('status','error','code','url_required',
            'message','Cannot enable EIS matching without EIS_MATCH_URL',
            'data','{}'::jsonb);
    END IF;

    INSERT INTO attendance.system_settings (key, value, updated_by, updated_at)
    VALUES ('EIS_MATCH', v_enabled, p_actor, now())
    ON CONFLICT (key) DO UPDATE
       SET value = EXCLUDED.value, updated_by = EXCLUDED.updated_by, updated_at = now();

    IF v_url_given THEN
        INSERT INTO attendance.system_settings (key, value, updated_by, updated_at)
        VALUES ('EIS_MATCH_URL', v_url, p_actor, now())
        ON CONFLICT (key) DO UPDATE
           SET value = EXCLUDED.value, updated_by = EXCLUDED.updated_by, updated_at = now();
    END IF;

    IF coalesce(v_old_on, '') IS DISTINCT FROM v_enabled THEN
        INSERT INTO attendance.system_settings_audit (setting_key, old_value, new_value, updated_by, client_ip)
        VALUES ('EIS_MATCH', v_old_on, v_enabled, p_actor, p_client_ip);
    END IF;

    IF v_url_given AND coalesce(v_old_url, '') IS DISTINCT FROM v_url THEN
        INSERT INTO attendance.system_settings_audit (setting_key, old_value, new_value, updated_by, client_ip)
        VALUES ('EIS_MATCH_URL', v_old_url, v_url, p_actor, p_client_ip);
    END IF;

    RETURN jsonb_build_object('status','success','message','Settings updated',
        'data', jsonb_build_object(
            'eis_match',     v_enabled,
            'eis_match_url', v_new_url));
END;
$function$;

COMMENT ON FUNCTION attendance.eis_match_save(varchar, varchar, bigint, varchar) IS
    'Set the Match-with-EIS toggle and endpoint. Refuses ON without a URL (url_required), a non-ON/OFF value (invalid_value), or a non-https URL (invalid_url).';

-- ---------------------------------------------------------------------
-- 4 · Who may change it — the same permission as the face toggle
--
-- Two paths, one endpoint, for the reason given in sql/008 section 6: the
-- gateway proxies /ext-api today, /admin-api is the documented contract.
-- ---------------------------------------------------------------------

INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, 'admin.settings.manage', true, v.note
  FROM (VALUES
    ('/admin-api/settings/eis-match',
     'GET reads the Match-with-EIS toggle; PUT changes it.'),
    ('/ext-api/settings/eis-match',
     'The same endpoint under the prefix the gateway proxies. Remove this row only once /admin-api has a proxy rule AND every client has moved.')
  ) AS v(endpoint, note)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint
 );

INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES
    ('/admin-api/settings/eis-match'),
    ('/ext-api/settings/eis-match')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint
 );
