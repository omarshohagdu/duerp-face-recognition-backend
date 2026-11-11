-- =====================================================================
-- Office Location (Others): request offices for an employee you enrolled
--
-- Routes: src/routes/geo_location.rs
--   POST /ext-api/geo-location/others/my       json: employee_id, enrolled_by
--   POST /ext-api/geo-location/others/select   json: employee_id, enrolled_by, geo_location_office_ids[]
--   POST /ext-api/geo-location/others/remove   json: employee_id, enrolled_by, id
--   (the office list to choose from is the existing /geo-location/available)
--
-- WHY: an employee with no phone (enrolled by a colleague through
-- /face-attendance/others/enroll, sql/012) also cannot choose an office. The
-- colleague who enrolled them requests it for them; an administrator still
-- approves every request, exactly as for self-service.
--
-- WHO MAY ACT: only the colleague named in the employee's ACTIVE enrollment
-- (`wow_attendance_enrollments.enrolled_by`). Once the employee enrolls
-- themselves, the colleague loses access. The audit trail names the
-- colleague as the actor, with a note.
--
-- Idempotent. Requires 010 and 012.
--     psql "$DATABASE_URL" -f sql/013_geo_location_others.sql
-- =====================================================================

-- NULL when p_enrolled_by may act for p_employee_id, else the failure envelope.
CREATE OR REPLACE FUNCTION attendance.geo_others_guard(p_employee_id varchar, p_enrolled_by varchar)
RETURNS jsonb
LANGUAGE sql STABLE
AS $$
    SELECT CASE
        WHEN btrim(COALESCE(p_employee_id, '')) = '' OR btrim(COALESCE(p_enrolled_by, '')) = ''
            THEN attendance.geo_fail('INVALID_INPUT', '`employee_id` and `enrolled_by` are required')
        WHEN btrim(p_employee_id) = btrim(p_enrolled_by)
            THEN attendance.geo_fail('USE_SELF_OFFICE', 'That is your own employee ID. Use Office Location (Self).')
        WHEN NOT EXISTS (SELECT 1 FROM attendance.employees WHERE emp_id = btrim(p_employee_id))
            THEN attendance.geo_fail('EMPLOYEE_NOT_FOUND', 'No staff record was found for this employee ID.',
                     jsonb_build_object('employee_id', p_employee_id))
        WHEN NOT EXISTS (SELECT 1 FROM attendance.wow_attendance_enrollments
                          WHERE person_id = btrim(p_employee_id) AND id_type = 'Employee' AND is_active)
            THEN attendance.geo_fail('NOT_ENROLLED', 'Enroll this employee''s selfie first, then choose their office.',
                     jsonb_build_object('employee_id', p_employee_id))
        WHEN NOT EXISTS (SELECT 1 FROM attendance.wow_attendance_enrollments
                          WHERE person_id = btrim(p_employee_id) AND id_type = 'Employee' AND is_active
                            AND enrolled_by = btrim(p_enrolled_by))
            THEN attendance.geo_fail('NOT_ENROLLED_BY_YOU',
                     'You can only manage offices for employees you enrolled.',
                     jsonb_build_object('employee_id', p_employee_id))
    END;
$$;

-- The self functions write the employee as the audit actor. Re-attribute the
-- rows this transaction just wrote to the colleague who actually acted.
CREATE OR REPLACE FUNCTION attendance.geo_others_reattribute(
    p_row_ids bigint[], p_action varchar, p_employee_id varchar, p_enrolled_by varchar
) RETURNS void
LANGUAGE sql
AS $$
    UPDATE attendance.employee_geo_location_audit
       SET actor_id = btrim(p_enrolled_by),
           note     = left(format('On behalf of %s (Office Location: Others)', btrim(p_employee_id)), 255)
     WHERE employee_geo_location_id = ANY (p_row_ids)
       AND action     = p_action
       AND actor_id   = btrim(p_employee_id)
       AND created_at = now();
$$;

CREATE OR REPLACE FUNCTION attendance.employee_geo_others_my(p_employee_id varchar, p_enrolled_by varchar)
RETURNS jsonb
LANGUAGE sql STABLE
AS $$
    SELECT COALESCE(attendance.geo_others_guard(p_employee_id, p_enrolled_by),
                    attendance.employee_geo_my(btrim(p_employee_id)));
$$;

CREATE OR REPLACE FUNCTION attendance.employee_geo_others_select(
    p_employee_id varchar, p_enrolled_by varchar, p_ids bigint[]
) RETURNS jsonb
LANGUAGE plpgsql
AS $$
DECLARE
    v_err jsonb := attendance.geo_others_guard(p_employee_id, p_enrolled_by);
    v_res jsonb;
BEGIN
    IF v_err IS NOT NULL THEN RETURN v_err; END IF;
    v_res := attendance.employee_geo_select(btrim(p_employee_id), p_ids);
    IF v_res->>'status' = 'success' THEN
        PERFORM attendance.geo_others_reattribute(
            ARRAY(SELECT (e->>'id')::bigint FROM jsonb_array_elements(v_res->'data') e),
            'REQUEST', p_employee_id, p_enrolled_by);
        v_res := jsonb_set(v_res, '{message}', to_jsonb('Office request sent for admin approval'::text));
    END IF;
    RETURN v_res;
END;
$$;

CREATE OR REPLACE FUNCTION attendance.employee_geo_others_remove(
    p_employee_id varchar, p_enrolled_by varchar, p_id bigint
) RETURNS jsonb
LANGUAGE plpgsql
AS $$
DECLARE
    v_err jsonb := attendance.geo_others_guard(p_employee_id, p_enrolled_by);
    v_res jsonb;
BEGIN
    IF v_err IS NOT NULL THEN RETURN v_err; END IF;
    v_res := attendance.employee_geo_remove(btrim(p_employee_id), p_id);
    IF v_res->>'status' = 'success' THEN
        PERFORM attendance.geo_others_reattribute(ARRAY[p_id], 'REMOVE', p_employee_id, p_enrolled_by);
    END IF;
    RETURN v_res;
END;
$$;

-- Reachability + permission rules (audit, enforce = false), like the self paths.
INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, 'face.enroll_others', false, v.note
  FROM (VALUES
    ('/ext-api/geo-location/others/my',     'Offices of an employee the token holder enrolled.'),
    ('/ext-api/geo-location/others/select', 'Request offices for an employee the token holder enrolled.'),
    ('/ext-api/geo-location/others/remove', 'Cancel/remove an office of an employee the token holder enrolled.')
  ) AS v(endpoint, note)
 WHERE NOT EXISTS (SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint);

INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES ('/ext-api/geo-location/others/my'),
               ('/ext-api/geo-location/others/select'),
               ('/ext-api/geo-location/others/remove')) AS v(endpoint)
 WHERE NOT EXISTS (SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint);
