-- =====================================================================
-- Selfie enrollment + selfie attendance via the face-verify service
--
-- Routes: src/routes/face_attendance.rs
--   POST /ext-api/face-attendance/enroll   employee_id + one selfie
--   POST /ext-api/face-attendance/mark     employee_id + selfie + GPS
--
-- Instead of the AI platform's 1:N /recognize, these compare the selfie 1:1
-- with the employee's enrolled image through NFC_FACE_VERIFY_URL (the
-- `card_image` + `selfie_image` service the NFC card desk already uses), and
-- require the device to be inside one of the employee's APPROVED offices.
--
-- Enrollment reuses attendance.wow_attendance_enrollments / _images, so an
-- employee enrolled either way can select offices, and the mark endpoint can
-- compare against an image enrolled either way.
--
-- WHAT THIS FILE DOES
--   1. Records HOW each check-in was verified (verify_method, face_similarity).
--   2. Gives employee_geo_location_verify a mode argument, so this path can
--      always demand an approved office whatever GEO_LOCATION_MODE says.
--   3. Allow-list + permission rows for the two new paths.
--
-- Idempotent. Requires 010.
--     psql "$DATABASE_URL" -f sql/011_face_verify_attendance.sql
-- =====================================================================

-- 1 · How a check-in was verified. NULL on rows from before this file
--     (those were all the AI platform's /recognize).
ALTER TABLE attendance.wow_attendance_records
    ADD COLUMN IF NOT EXISTS verify_method   varchar(16),
    ADD COLUMN IF NOT EXISTS face_similarity numeric(6,4);

COMMENT ON COLUMN attendance.wow_attendance_records.verify_method IS
    'AI_RECOGNIZE (wow-attendance/verify) | FACE_VERIFY (face-attendance/mark). NULL = before sql/011, i.e. AI_RECOGNIZE.';

-- 2 · The location gate with an explicit mode.
--     p_mode NULL = use GEO_LOCATION_MODE (what /wow-attendance/verify does);
--     'APPROVED_OFFICE' = approved offices only (what /face-attendance/mark does).
--     The 4-argument form is kept as a wrapper so existing callers are unchanged.
CREATE OR REPLACE FUNCTION attendance.employee_geo_location_verify(
    p_emp_id      varchar,
    p_lat         double precision,
    p_long        double precision,
    p_accuracy    double precision,
    p_mode        varchar
) RETURNS jsonb
LANGUAGE plpgsql
AS $$
DECLARE
    v_mode      varchar := CASE upper(btrim(COALESCE(p_mode, '')))
                               WHEN 'BODY_MAPPING' THEN 'BODY_MAPPING'
                               WHEN 'APPROVED_OFFICE' THEN 'APPROVED_OFFICE'
                               WHEN 'APPROVED_OFFICE_OR_BODY_MAPPING' THEN 'APPROVED_OFFICE_OR_BODY_MAPPING'
                               ELSE attendance.geo_location_mode()
                           END;
    v_max_acc   integer := attendance.geo_setting_int('GEO_MAX_GPS_ACCURACY_METERS', 50);
    v_has_appr  boolean;
    v_legacy    jsonb;
    v_row       RECORD;
    v_code      varchar;
    v_msg       varchar;
BEGIN
    IF p_lat IS NULL OR p_long IS NULL
       OR p_lat NOT BETWEEN -90 AND 90 OR p_long NOT BETWEEN -180 AND 180 THEN
        RETURN jsonb_build_object('success', false, 'verified', false,
            'code', 'INVALID_COORDINATES', 'message', 'Invalid device coordinates');
    END IF;

    v_has_appr := EXISTS (SELECT 1 FROM attendance.employee_geo_location
                           WHERE employee_id = p_emp_id AND status = 'APPROVED');

    IF v_mode = 'BODY_MAPPING' OR (v_mode = 'APPROVED_OFFICE_OR_BODY_MAPPING' AND NOT v_has_appr) THEN
        v_legacy := attendance.wow_attendance_location_verify(p_emp_id, p_lat, p_long);
        RETURN v_legacy
            || jsonb_build_object('code', CASE WHEN (v_legacy->>'verified')::boolean THEN 'OK'
                                               WHEN (v_legacy->>'success')::boolean THEN 'OUT_OF_LOCATION'
                                               ELSE 'LOCATION_NOT_VERIFIED' END)
            || jsonb_build_object('data', COALESCE(v_legacy->'data', '{}'::jsonb)
                                          || jsonb_build_object('source', 'BODY_MAPPING', 'mode', v_mode));
    END IF;

    IF p_accuracy IS NOT NULL AND p_accuracy > v_max_acc THEN
        INSERT INTO attendance.geo_attendance_attempt_log
                    (employee_id, failure_code, latitude, longitude, accuracy_meters)
             VALUES (p_emp_id, 'GPS_ACCURACY_TOO_LOW', p_lat, p_long, p_accuracy);
        RETURN jsonb_build_object('success', false, 'verified', false, 'code', 'GPS_ACCURACY_TOO_LOW',
            'message', format('GPS accuracy is %sm; it must be %sm or better. Move outdoors or wait for a better fix.',
                              round(p_accuracy::numeric, 1), v_max_acc),
            'data', jsonb_build_object('accuracy_meters', p_accuracy, 'max_accuracy_meters', v_max_acc,
                                       'source', 'APPROVED_OFFICE', 'mode', v_mode));
    END IF;

    SELECT egl.id AS employee_geo_location_id,
           g.id   AS geo_location_office_id,
           g.office_id, b.name AS office_name, g.label,
           g.radius_meters,
           round(attendance.haversine_meters(p_lat, p_long,
                     g.latitude::double precision, g.longitude::double precision)::numeric, 2) AS distance
      INTO v_row
      FROM attendance.employee_geo_location egl
      JOIN attendance.geo_location_office g ON g.id = egl.geo_location_office_id
      LEFT JOIN LATERAL (SELECT name FROM ictcell.body WHERE body_code = g.office_id LIMIT 1) b ON true
     WHERE egl.employee_id = p_emp_id
       AND egl.status = 'APPROVED'
       AND g.status   = 'ACTIVE'
     ORDER BY distance ASC, g.id
     LIMIT 1;

    IF NOT FOUND THEN
        INSERT INTO attendance.geo_attendance_attempt_log
                    (employee_id, failure_code, latitude, longitude, accuracy_meters)
             VALUES (p_emp_id, 'NO_APPROVED_LOCATION', p_lat, p_long, p_accuracy);
        RETURN jsonb_build_object('success', false, 'verified', false, 'code', 'NO_APPROVED_LOCATION',
            'message', CASE WHEN v_has_appr
                            THEN 'None of your approved offices is currently active for attendance.'
                            ELSE 'You have no approved office. Select your office and wait for admin approval.' END,
            'data', jsonb_build_object('source', 'APPROVED_OFFICE', 'mode', v_mode));
    END IF;

    IF v_row.distance > v_row.radius_meters THEN
        v_code := 'OUT_OF_LOCATION';
        v_msg  := 'You are not inside any of your approved offices';
        INSERT INTO attendance.geo_attendance_attempt_log
                    (employee_id, failure_code, nearest_geo_location_office_id, distance_meters,
                     latitude, longitude, accuracy_meters)
             VALUES (p_emp_id, v_code, v_row.geo_location_office_id, v_row.distance,
                     p_lat, p_long, p_accuracy);
    ELSE
        v_code := 'OK';
        v_msg  := 'Location verified';
    END IF;

    RETURN jsonb_build_object(
        'success',  v_code = 'OK',
        'verified', v_code = 'OK',
        'code',     v_code,
        'message',  v_msg,
        'data', jsonb_build_object(
            'source',                   'APPROVED_OFFICE',
            'mode',                     v_mode,
            'emp_id',                   p_emp_id,
            'employee_geo_location_id', v_row.employee_geo_location_id,
            'geo_location_office_id',   v_row.geo_location_office_id,
            'office_id',                v_row.office_id,
            'office_name',              v_row.office_name,
            'label',                    v_row.label,
            'distance_meters',          v_row.distance::double precision,
            'radius_meters',            v_row.radius_meters));
END;
$$;

CREATE OR REPLACE FUNCTION attendance.employee_geo_location_verify(
    p_emp_id      varchar,
    p_lat         double precision,
    p_long        double precision,
    p_accuracy    double precision
) RETURNS jsonb
LANGUAGE sql
AS $$
    SELECT attendance.employee_geo_location_verify(p_emp_id, p_lat, p_long, p_accuracy, NULL::varchar);
$$;

-- 3 · Who may reach the two paths.
--     Open to any IP (phones, browsers), like the rest of the employee API; the
--     handlers only ever act on the token holder's own employee id. Rules reuse
--     the existing face permissions and start in audit (enforce = false), the
--     same as /wow-attendance/enroll and /verify.
INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, v.resource_key, false, v.note
  FROM (VALUES
    ('/ext-api/face-attendance/enroll', 'face.enroll',
        'Selfie enrollment (one image, checked by the face-verify service). Self only.'),
    ('/ext-api/face-attendance/mark',   'face.verify',
        'Selfie attendance: 1:1 face-verify against the enrolled image + approved-office GPS. Self only.')
  ) AS v(endpoint, resource_key, note)
 WHERE NOT EXISTS (SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint);

INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES ('/ext-api/face-attendance/enroll'), ('/ext-api/face-attendance/mark')) AS v(endpoint)
 WHERE NOT EXISTS (SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint);
