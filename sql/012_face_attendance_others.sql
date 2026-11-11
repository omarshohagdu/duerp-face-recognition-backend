-- =====================================================================
-- Others Selfie Attendance: one employee enrolls / marks for another
--
-- Routes: src/routes/face_attendance.rs
--   POST /ext-api/face-attendance/others/enroll   employee_id + selfie + enrolled_by
--   POST /ext-api/face-attendance/others/mark     employee_id + selfie + enrolled_by + GPS
--   POST /ext-api/face-attendance/others/list     the people enrolled_by has enrolled
--
-- WHY: some employees have no mobile phone, so they cannot enroll or check in
-- themselves. A colleague (`enrolled_by`, always the token holder) captures
-- their selfie on their own device instead. Everything else is the
-- selfie path from sql/011: 1:1 face-verify against the enrolled selfie, and
-- GPS inside an approved, active office.
--
-- WHAT THIS FILE DOES
--   1. `enrolled_by` on enrollments, `marked_by` on attendance records.
--      NULL = the employee did it themselves (every row before this file).
--   2. attendance.face_enroll_for_other(): the enrollment, its conflict rules
--      and the `enrolled_by` stamp in ONE statement, so they cannot drift.
--   3. attendance.face_enrolled_by(): the "people I enrolled" list.
--   4. Permission keys, endpoint rules (audit, enforce = false) and the IP
--      allow-list rows for the three new paths.
--
-- Idempotent. Requires 011.
--     psql "$DATABASE_URL" -f sql/012_face_attendance_others.sql
-- =====================================================================

-- 1 · Who did it on someone else's behalf.
ALTER TABLE attendance.wow_attendance_enrollments
    ADD COLUMN IF NOT EXISTS enrolled_by varchar(50);

ALTER TABLE attendance.wow_attendance_records
    ADD COLUMN IF NOT EXISTS marked_by varchar(50);

COMMENT ON COLUMN attendance.wow_attendance_enrollments.enrolled_by IS
    'emp_id of the colleague who enrolled this person (face-attendance/others/enroll). NULL = self-enrolled.';
COMMENT ON COLUMN attendance.wow_attendance_records.marked_by IS
    'emp_id of the colleague who marked this attendance (face-attendance/others/mark). NULL = self-marked.';

CREATE INDEX IF NOT EXISTS wow_attendance_enrollments_enrolled_by_idx
    ON attendance.wow_attendance_enrollments (enrolled_by)
 WHERE enrolled_by IS NOT NULL AND is_active;

-- 2 · Enroll someone else.
--
-- Conflict rules, checked against the employee's CURRENT active enrollment:
--   * none                         → enroll
--   * self-enrolled (enrolled_by NULL) → ALREADY_SELF_ENROLLED. A colleague
--     must never replace the photo an employee registered for themselves;
--     that is exactly how someone else's face would get onto their record.
--   * enrolled by this enroller    → re-enroll (replace the photo)
--   * enrolled by another colleague → ENROLLED_BY_OTHER
-- The employee can always take over by enrolling themselves on the normal
-- /face-attendance/enroll, which writes enrolled_by = NULL.
CREATE OR REPLACE FUNCTION attendance.face_enroll_for_other(
    p_employee_id varchar,
    p_enrolled_by varchar,
    p_token       text,
    p_device_info jsonb,
    p_image_paths text[]
) RETURNS jsonb
LANGUAGE plpgsql
AS $$
DECLARE
    v_cur     RECORD;
    v_result  jsonb;
    v_id      uuid;
BEGIN
    IF p_employee_id IS NULL OR btrim(p_employee_id) = ''
       OR p_enrolled_by IS NULL OR btrim(p_enrolled_by) = '' THEN
        RETURN jsonb_build_object('success', false, 'code', 'INVALID_INPUT',
            'message', 'employee_id and enrolled_by are required');
    END IF;
    IF btrim(p_employee_id) = btrim(p_enrolled_by) THEN
        RETURN jsonb_build_object('success', false, 'code', 'USE_SELF_ENROLL',
            'message', 'To enroll yourself, use Enroll selfie.');
    END IF;

    -- Lock the current enrollment so two enrollers cannot race past the check.
    SELECT id, enrolled_by
      INTO v_cur
      FROM attendance.wow_attendance_enrollments
     WHERE person_id = p_employee_id AND id_type = 'Employee' AND is_active
     ORDER BY enrolled_at DESC, version DESC
     LIMIT 1
       FOR UPDATE;

    IF FOUND THEN
        IF v_cur.enrolled_by IS NULL THEN
            RETURN jsonb_build_object('success', false, 'code', 'ALREADY_SELF_ENROLLED',
                'message', 'This employee enrolled their own selfie. Only they can replace it.',
                'data', jsonb_build_object('employee_id', p_employee_id));
        ELSIF v_cur.enrolled_by <> p_enrolled_by THEN
            RETURN jsonb_build_object('success', false, 'code', 'ENROLLED_BY_OTHER',
                'message', 'Another colleague enrolled this employee. Only they, or the employee, can replace it.',
                'data', jsonb_build_object('employee_id', p_employee_id, 'enrolled_by', v_cur.enrolled_by));
        END IF;
    END IF;

    v_result := attendance.wow_attendance_enroll(
        p_employee_id, 'Employee', p_token,
        COALESCE(p_device_info, '{}'::jsonb) || jsonb_build_object('enrolled_by', p_enrolled_by),
        p_image_paths);

    IF (v_result->>'success')::boolean IS NOT TRUE THEN
        RETURN v_result || jsonb_build_object('code', 'ENROLL_FAILED');
    END IF;

    v_id := (v_result->'data'->>'enrollment_id')::uuid;
    UPDATE attendance.wow_attendance_enrollments SET enrolled_by = p_enrolled_by WHERE id = v_id;

    RETURN v_result
        || jsonb_build_object('code', 'OK')
        || jsonb_build_object('data', (v_result->'data') || jsonb_build_object('enrolled_by', p_enrolled_by));
END;
$$;

-- 3 · The people an enroller currently holds an active enrollment for, newest
--     first, with today's check-in so the screen can show who is still to mark.
CREATE OR REPLACE FUNCTION attendance.face_enrolled_by(p_enrolled_by varchar)
RETURNS jsonb
LANGUAGE sql
STABLE
AS $$
    SELECT COALESCE(jsonb_agg(row_to_json(x)::jsonb ORDER BY x.enrolled_at DESC), '[]'::jsonb)
      FROM (
        SELECT e.person_id           AS employee_id,
               emp.name_en           AS employee_name,
               emp.designation_en    AS designation,
               emp.office            AS office_id,
               e.id                  AS enrollment_id,
               e.version,
               e.enrolled_at,
               img.image_path,
               last.created_at       AS last_marked_at,
               last.marked_by        AS last_marked_by,
               COALESCE(last.created_at >= date_trunc('day', now()), false) AS marked_today
          FROM attendance.wow_attendance_enrollments e
          LEFT JOIN LATERAL (SELECT name_en, designation_en, office
                               FROM attendance.employees WHERE emp_id = e.person_id LIMIT 1) emp ON true
          LEFT JOIN LATERAL (SELECT image_path FROM attendance.wow_attendance_images
                              WHERE enrollment_id = e.id ORDER BY created_at DESC LIMIT 1) img ON true
          LEFT JOIN LATERAL (SELECT r.created_at, r.marked_by FROM attendance.wow_attendance_records r
                              WHERE r.person_id = e.person_id AND r.id_type = 'Employee' AND r.matched
                              ORDER BY r.created_at DESC LIMIT 1) last ON true
         WHERE e.enrolled_by = p_enrolled_by
           AND e.id_type = 'Employee'
           AND e.is_active
      ) x;
$$;

-- 4 · Permissions and reachability.
--     Two new keys so enrolling/marking for others can be granted separately
--     from doing it for yourself. Rules start in audit (enforce = false) like
--     every other face endpoint, so any signed-in employee may use them today;
--     turning enforce on restricts them to roles holding the key.
INSERT INTO attendance.resources (key, name, category) VALUES
    ('face.enroll_others', 'Face · Enroll others (selfie)',       'Attendance Devices'),
    ('face.mark_others',   'Face · Mark attendance for others',   'Attendance Devices')
ON CONFLICT (key) DO NOTHING;

INSERT INTO attendance.role_permissions (role_id, resource_id)
SELECT r.id, res.id
  FROM attendance.roles r, attendance.resources res
 WHERE r.key IN ('admin', 'superadmin')
   AND res.key IN ('face.enroll_others', 'face.mark_others')
ON CONFLICT DO NOTHING;

INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, v.resource_key, false, v.note
  FROM (VALUES
    ('/ext-api/face-attendance/others/enroll', 'face.enroll_others',
        'Enroll a colleague''s selfie (no phone). enrolled_by must be the token holder.'),
    ('/ext-api/face-attendance/others/mark',   'face.mark_others',
        'Mark a colleague''s selfie attendance: 1:1 face-verify + approved-office GPS. enrolled_by must be the token holder.'),
    ('/ext-api/face-attendance/others/list',   'face.enroll_others',
        'Colleagues the token holder has enrolled.')
  ) AS v(endpoint, resource_key, note)
 WHERE NOT EXISTS (SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint);

INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES ('/ext-api/face-attendance/others/enroll'),
               ('/ext-api/face-attendance/others/mark'),
               ('/ext-api/face-attendance/others/list')) AS v(endpoint)
 WHERE NOT EXISTS (SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint);
