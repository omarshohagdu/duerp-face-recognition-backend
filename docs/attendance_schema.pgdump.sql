-- =====================================================================
-- attendance schema — GENERATED SNAPSHOT (pg_dump, schema only)
--
-- NOT THE SOURCE OF TRUTH. `docs/attendance_schema.sql` is the file to read
-- and to apply (see docs/DEPLOYMENT.md); it carries the comments explaining
-- WHY each object is shaped the way it is, which pg_dump strips.
--
-- This file is the opposite artifact: what the live database ACTUALLY contains
-- right now, machine-generated, for diffing against the hand-written file when
-- you suspect they have drifted.
--
-- Source database : lms_dev (server 17.6)
-- Generated       : 2026-09-09 10:43 UTC
-- Command         : /usr/lib/postgresql/17/bin/pg_dump --schema-only \
--                     --schema=attendance --no-owner --no-privileges
--
-- Contents: 8 tables, 1 view (attendance.employees, over ictcell.employees),
-- 13 functions, plus sequences, constraints and indexes.
--
-- Schema only — no rows. The identity-adjacent tables here hold employee ids
-- and face-image paths, so data is deliberately NOT dumped into the repo.
--
-- Regenerate:
--   /usr/lib/postgresql/17/bin/pg_dump --schema-only --schema=attendance \
--       --no-owner --no-privileges "$DATABASE_URL" > docs/attendance_schema.pgdump.sql
--   (the system pg_dump is 16.x and REFUSES a 17.x server — use the 17 path)
-- =====================================================================

--
-- PostgreSQL database dump
--

\restrict D2EIuEMrvhhVOppUV6GKb5TQt3Ks8JZEYWoLlyGHbyZxhtNZzj751Mr7hngiDHS

-- Dumped from database version 17.6
-- Dumped by pg_dump version 17.9 (Ubuntu 17.9-1.pgdg24.04+1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: attendance; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA attendance;


--
-- Name: nfc_card_get_info(character varying); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.nfc_card_get_info(p_card_number character varying) RETURNS jsonb
    LANGUAGE plpgsql STABLE
    AS $$
DECLARE
    v_card varchar := attendance.nfc_card_normalize(p_card_number);
    v_row  RECORD;
BEGIN
    IF v_card IS NULL THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'missing_card_number',
            'message', 'card_number is required');
    END IF;

    SELECT c.student_applicant_id, c.card_number, c.is_verified, c.registration_type
      INTO v_row
      FROM attendance.nfc_student_cards c
     WHERE c.card_number = v_card;

    IF NOT FOUND THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'card_not_found',
            'message', 'Card not recognized');
    END IF;

    RETURN jsonb_build_object(
        'success', true,
        'data', jsonb_build_object(
            'student_applicant_id', v_row.student_applicant_id,
            'card_number',          v_row.card_number,
            'is_verified',          v_row.is_verified,
            'registration_type',    v_row.registration_type
        )
    );
END;
$$;


--
-- Name: nfc_card_normalize(character varying); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.nfc_card_normalize(p_card_number character varying) RETURNS character varying
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$
    SELECT nullif(
        upper(regexp_replace(coalesce(p_card_number, ''), '[^0-9A-Za-z]', '', 'g')),
        ''
    )::varchar
$$;


--
-- Name: FUNCTION nfc_card_normalize(p_card_number character varying); Type: COMMENT; Schema: attendance; Owner: -
--

COMMENT ON FUNCTION attendance.nfc_card_normalize(p_card_number character varying) IS 'Canonical card number: separators stripped, upper-cased, empty -> NULL.';


--
-- Name: nfc_card_save_info(character varying, character varying, smallint, smallint, text, text, boolean, bigint, character varying); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.nfc_card_save_info(p_student_applicant_id character varying, p_card_number character varying, p_is_verified smallint, p_registration_type smallint, p_card_image text, p_student_selfie text, p_force_reassign boolean, p_performed_by bigint, p_client_ip character varying) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_applicant  varchar := btrim(coalesce(p_student_applicant_id, ''));
    v_card       varchar := attendance.nfc_card_normalize(p_card_number);
    v_verified   smallint;
    v_reg_type   smallint := p_registration_type;
    v_force      boolean  := coalesce(p_force_reassign, false);
    v_holder     RECORD;
    v_existing   RECORD;
    v_row        RECORD;
    v_reassigned varchar := NULL;
    v_changed    text[]  := '{}';
    v_created    boolean := false;
BEGIN
    -- ── Validation ───────────────────────────────────────────────────
    IF v_applicant = '' OR v_card IS NULL OR v_reg_type IS NULL THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'missing_fields',
            'message', 'student_applicant_id, card_number, and registration_type are required');
    END IF;

    IF v_reg_type NOT IN (1, 2)
       OR (p_is_verified IS NOT NULL AND p_is_verified NOT IN (1, 2)) THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'invalid_value',
            'message', 'is_verified/registration_type must be 1 or 2');
    END IF;

    -- Defaults to 2 (No) when omitted: a new mapping is unreviewed until
    -- someone says otherwise.
    v_verified := coalesce(p_is_verified, 2::smallint);

    -- Self-registration can never mark itself verified.
    IF v_reg_type = 2 THEN
        v_verified := 2;
    END IF;

    IF length(v_applicant) > 64 THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'invalid_value',
            'message', 'student_applicant_id must be at most 64 characters');
    END IF;

    -- Normalisation has already stripped everything that is not alphanumeric,
    -- so only the length still needs a bound. The floor rejects a truncated
    -- read (a one- or two-character "UID" is a failed scan, not a card).
    IF length(v_card) < 4 OR length(v_card) > 64 THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'invalid_card_number',
            'message', 'card_number must be 4-64 alphanumeric characters after normalization');
    END IF;

    -- ── Card ownership ───────────────────────────────────────────────
    -- Both rows are locked before anything is written, so two readers racing
    -- to claim one card serialise here instead of both passing the conflict
    -- check and one losing on the UNIQUE constraint.
    SELECT c.id, c.student_applicant_id, c.card_number
      INTO v_holder
      FROM attendance.nfc_student_cards c
     WHERE c.card_number = v_card
       FOR UPDATE;

    SELECT c.*
      INTO v_existing
      FROM attendance.nfc_student_cards c
     WHERE c.student_applicant_id = v_applicant
       FOR UPDATE;

    IF v_holder.id IS NOT NULL AND v_holder.student_applicant_id <> v_applicant THEN
        IF NOT v_force THEN
            RETURN jsonb_build_object(
                'success', false,
                'code',    'card_conflict',
                'message', 'Card already assigned to another student',
                'data', jsonb_build_object(
                    'assigned_to', v_holder.student_applicant_id
                ));
        END IF;

        -- Re-issue: take the card off its current holder first, so the write
        -- below cannot trip the UNIQUE constraint. Their record and images
        -- stay; only the card number is cleared.
        UPDATE attendance.nfc_student_cards
           SET card_number = NULL,
               updated_at  = now()
         WHERE id = v_holder.id;

        INSERT INTO attendance.nfc_card_audit
               (card_id, student_applicant_id, action, card_number,
                changed_fields, performed_by, client_ip)
        VALUES (v_holder.id, v_holder.student_applicant_id, 'unassigned', v_card,
                ARRAY['card_number'], p_performed_by, p_client_ip);

        v_reassigned := v_holder.student_applicant_id;
    END IF;

    -- ── Write ────────────────────────────────────────────────────────
    IF v_existing.id IS NULL THEN
        INSERT INTO attendance.nfc_student_cards
               (student_applicant_id, card_number, card_image, student_selfie,
                is_verified, registration_type)
        VALUES (v_applicant, v_card, p_card_image, p_student_selfie,
                v_verified, v_reg_type)
        RETURNING * INTO v_row;

        v_created := true;
        v_changed := ARRAY['card_number', 'is_verified', 'registration_type']
                     || CASE WHEN p_card_image    IS NOT NULL THEN ARRAY['card_image']    ELSE '{}'::text[] END
                     || CASE WHEN p_student_selfie IS NOT NULL THEN ARRAY['student_selfie'] ELSE '{}'::text[] END;
    ELSE
        -- Only what actually differs is reported as changed, so the audit row
        -- for a re-save that altered nothing is empty rather than misleading.
        v_changed := (CASE WHEN v_existing.card_number IS DISTINCT FROM v_card
                           THEN ARRAY['card_number'] ELSE '{}'::text[] END)
                  || (CASE WHEN v_existing.is_verified IS DISTINCT FROM v_verified
                           THEN ARRAY['is_verified'] ELSE '{}'::text[] END)
                  || (CASE WHEN v_existing.registration_type IS DISTINCT FROM v_reg_type
                           THEN ARRAY['registration_type'] ELSE '{}'::text[] END)
                  || (CASE WHEN p_card_image IS NOT NULL
                            AND v_existing.card_image IS DISTINCT FROM p_card_image
                           THEN ARRAY['card_image'] ELSE '{}'::text[] END)
                  || (CASE WHEN p_student_selfie IS NOT NULL
                            AND v_existing.student_selfie IS DISTINCT FROM p_student_selfie
                           THEN ARRAY['student_selfie'] ELSE '{}'::text[] END);

        UPDATE attendance.nfc_student_cards
           SET card_number       = v_card,
               -- COALESCE: an omitted image keeps the stored one.
               card_image        = coalesce(p_card_image, card_image),
               student_selfie    = coalesce(p_student_selfie, student_selfie),
               is_verified       = v_verified,
               registration_type = v_reg_type,
               updated_at        = now()
         WHERE id = v_existing.id
        RETURNING * INTO v_row;
    END IF;

    INSERT INTO attendance.nfc_card_audit
           (card_id, student_applicant_id, action, card_number, is_verified,
            registration_type, changed_fields, performed_by, client_ip)
    VALUES (v_row.id, v_row.student_applicant_id,
            CASE WHEN v_created THEN 'created' ELSE 'updated' END,
            v_row.card_number, v_row.is_verified, v_row.registration_type,
            v_changed, p_performed_by, p_client_ip);

    RETURN jsonb_build_object(
        'success', true,
        'message', 'Card info saved successfully',
        'data', jsonb_build_object(
            'student_applicant_id', v_row.student_applicant_id,
            'card_number',          v_row.card_number,
            'is_verified',          v_row.is_verified,
            'registration_type',    v_row.registration_type,
            -- Stored filesystem paths. The handler replaces both with public
            -- URLs before answering; a direct SQL caller gets the paths.
            'card_image',           v_row.card_image,
            'student_selfie',       v_row.student_selfie,
            'created',              v_created,
            'reassigned_from',      v_reassigned,
            'changed_fields',       to_jsonb(v_changed)
        ),
        'warnings', (
            SELECT COALESCE(jsonb_agg(w), '[]'::jsonb) FROM (
                SELECT 'is_verified forced to 2 — a self-registration (registration_type=2) '
                    || 'cannot mark itself verified' AS w
                 WHERE v_reg_type = 2 AND p_is_verified = 1
                UNION ALL
                SELECT 'Card taken from ' || v_reassigned || ' — that student now has no card'
                 WHERE v_reassigned IS NOT NULL
            ) warn
        )
    );

EXCEPTION
    -- Backstop for a race the FOR UPDATE locks above should already have
    -- serialised. Reported as the same conflict a caller would have seen on
    -- the check, not as a 500.
    WHEN unique_violation THEN
        RETURN jsonb_build_object(
            'success', false,
            'code',    'card_conflict',
            'message', 'Card already assigned to another student');
END;
$$;


--
-- Name: wow_attendance_body_building_mapping_save(character varying, integer, character varying, double precision, double precision, double precision, boolean); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_body_building_mapping_save(p_body_code character varying, p_building_id integer, p_building_name character varying, p_lat double precision, p_long double precision, p_radius double precision, p_is_active boolean) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_building_id   int;
    v_building_name varchar;
    v_radius        numeric := round(COALESCE(p_radius, 50)::numeric, 2);
    v_is_active     boolean := COALESCE(p_is_active, true);
    v_emp_count     int;
    v_row           RECORD;
    v_created       boolean := false;
BEGIN
    IF p_body_code IS NULL OR btrim(p_body_code) = '' THEN
        RETURN jsonb_build_object(
            'success', false, 'message', '`body_code` is required');
    END IF;

    IF p_lat IS NULL OR p_long IS NULL
       OR p_lat  NOT BETWEEN  -90 AND  90
       OR p_long NOT BETWEEN -180 AND 180 THEN
        RETURN jsonb_build_object(
            'success', false, 'message', 'Invalid building coordinates');
    END IF;

    IF v_radius <= 0 THEN
        RETURN jsonb_build_object(
            'success', false, 'message', '`radius` must be greater than 0');
    END IF;

    -- Resolve the building: explicit id wins, otherwise find-or-create by name.
    IF p_building_id IS NOT NULL THEN
        SELECT b.id, b.name INTO v_building_id, v_building_name
          FROM attendance.buildings b WHERE b.id = p_building_id;
        IF NOT FOUND THEN
            RETURN jsonb_build_object(
                'success', false,
                'message', format('Building %s not found', p_building_id));
        END IF;

    ELSIF p_building_name IS NOT NULL AND btrim(p_building_name) <> '' THEN
        SELECT b.id, b.name INTO v_building_id, v_building_name
          FROM attendance.buildings b
         WHERE lower(btrim(b.name)) = lower(btrim(p_building_name))
         LIMIT 1;
        IF NOT FOUND THEN
            INSERT INTO attendance.buildings (name, status)
                 VALUES (btrim(p_building_name), 'Active')
              RETURNING id, name INTO v_building_id, v_building_name;
            v_created := true;
        END IF;

    ELSE
        RETURN jsonb_build_object(
            'success', false,
            'message', 'Either `building_id` or `building_name` is required');
    END IF;

    -- Upsert the mapping.
    INSERT INTO attendance.body_building_mapping
                (body_code, building_id, lat, "long", radius, is_active)
         VALUES (btrim(p_body_code), v_building_id, p_lat, p_long, v_radius, v_is_active)
    ON CONFLICT (body_code, building_id) DO UPDATE
            SET lat        = EXCLUDED.lat,
                "long"     = EXCLUDED."long",
                radius     = EXCLUDED.radius,
                is_active  = EXCLUDED.is_active,
                updated_at = now()
      RETURNING id, body_code, building_id, lat, "long", radius, is_active,
                (xmax = 0) AS inserted
           INTO v_row;

    -- A body_code matching no employee is almost always a typo: the mapping
    -- saves fine but can never verify anyone. Reported, not rejected, so a
    -- mapping can still be staged before staff are assigned to the office.
    SELECT count(*) INTO v_emp_count
      FROM attendance.employees e WHERE e.office = btrim(p_body_code);

    RETURN jsonb_build_object(
        'success', true,
        'message', CASE WHEN v_row.inserted THEN 'Mapping created'
                        ELSE 'Mapping updated' END,
        'data', jsonb_build_object(
            'mapping_id',       v_row.id,
            'body_code',        v_row.body_code,
            'building_id',      v_row.building_id,
            'building_name',    v_building_name,
            'building_created', v_created,
            'lat',              v_row.lat,
            'long',             v_row."long",
            'radius',           v_row.radius,
            'is_active',        v_row.is_active,
            'employee_count',   v_emp_count
        ),
        'warnings', (
            SELECT COALESCE(jsonb_agg(w), '[]'::jsonb) FROM (
                SELECT 'No employee has office=' || btrim(p_body_code) ||
                       ' — this mapping will never verify anyone' AS w
                 WHERE v_emp_count = 0
                UNION ALL
                SELECT 'radius ' || v_radius || 'm is below 20m; GPS drift ' ||
                       'alone is 3-50m and will reject valid check-ins'
                 WHERE v_radius < 20
            ) warn
        )
    );
END;
$$;


--
-- Name: wow_attendance_check_enrolled(character varying); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_check_enrolled(p_person_id character varying) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_row RECORD;
BEGIN
  SELECT
    e.id                     AS enrollment_id,
    e.id_type                AS id_type,
    e.enrolled_at            AS enrolled_at,
    e.is_active              AS is_active,
    e.version                AS version,
    e.previous_enrollment_id AS previous_enrollment_id,
    (SELECT count(*) FROM attendance.wow_attendance_images i
      WHERE i.enrollment_id = e.id) AS image_count
    INTO v_row
    FROM attendance.wow_attendance_enrollments e
   WHERE e.person_id = p_person_id
     AND e.is_active = true
   ORDER BY e.enrolled_at DESC
   LIMIT 1;

  IF v_row.enrollment_id IS NULL THEN
    RETURN jsonb_build_object(
      'success', false,
      'enrolled', false,
      'message', 'Person is not enrolled',
      'data', jsonb_build_object('id', p_person_id)
    );
  END IF;

  RETURN jsonb_build_object(
    'success', true,
    'enrolled', true,
    'message', 'Person is enrolled',
    'data', jsonb_build_object(
      'id',                     p_person_id,
      'id_type',                v_row.id_type,
      'enrollment_id',          v_row.enrollment_id,
      'enrolled_at',            v_row.enrolled_at,
      'image_count',            v_row.image_count,
      'is_active',              v_row.is_active,
      'version',                v_row.version,
      'is_reenrollment',        (v_row.version > 1),
      'previous_enrollment_id', v_row.previous_enrollment_id
    )
  );
END;
$$;


--
-- Name: wow_attendance_enroll(character varying, character varying, text, jsonb, text[]); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_enroll(p_person_id character varying, p_id_type character varying, p_token text, p_device_info jsonb, p_image_paths text[]) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_enrollment_id UUID;
  v_path          TEXT;
  v_count         INT := 0;
  v_prev_id       UUID;
  v_prev_version  INT;
  v_version       INT := 1;
  v_is_reenroll   BOOLEAN := false;
BEGIN
  IF p_image_paths IS NULL OR array_length(p_image_paths, 1) IS NULL THEN
    RETURN jsonb_build_object(
      'success', false,
      'message', 'At least one face image is required'
    );
  END IF;

  -- Re-enroll = versioning: the same /enroll URL handles first enrollment and
  -- re-enrollment. Look up the current active enrollment (if any) being
  -- superseded so we can record the re-enroll lineage on the new row.
  SELECT id, version
    INTO v_prev_id, v_prev_version
    FROM attendance.wow_attendance_enrollments
   WHERE person_id = p_person_id
     AND id_type   = p_id_type
     AND is_active = true
   ORDER BY enrolled_at DESC, version DESC
   LIMIT 1;

  IF v_prev_id IS NOT NULL THEN
    v_is_reenroll := true;
    v_version     := COALESCE(v_prev_version, 1) + 1;
  END IF;

  -- Retire any current active enrollment(s) for this person (kept for history,
  -- along with their images and attendance records), then create a fresh active
  -- enrollment so the active row reflects the latest enrollment with only the
  -- newly supplied images. version / previous_enrollment_id capture the re-enroll.
  UPDATE attendance.wow_attendance_enrollments
     SET is_active = false
   WHERE person_id = p_person_id
     AND id_type   = p_id_type
     AND is_active = true;

  INSERT INTO attendance.wow_attendance_enrollments
    (person_id, id_type, device_info, version, previous_enrollment_id)
  VALUES
    (p_person_id, p_id_type, p_device_info, v_version, v_prev_id)
  RETURNING id INTO v_enrollment_id;

  FOREACH v_path IN ARRAY p_image_paths LOOP
    INSERT INTO attendance.wow_attendance_images (enrollment_id, image_path)
    VALUES (v_enrollment_id, v_path);
    v_count := v_count + 1;
  END LOOP;

  RETURN jsonb_build_object(
    'success', true,
    'message', CASE WHEN v_is_reenroll THEN 'Re-enrolled successfully'
                    ELSE 'Enrolled successfully' END,
    'data', jsonb_build_object(
      'id',                     p_person_id,
      'id_type',                p_id_type,
      'enrolled_image_count',   v_count,
      'enrollment_id',          v_enrollment_id,
      'version',                v_version,
      'is_reenrollment',        v_is_reenroll,
      'previous_enrollment_id', v_prev_id
    )
  );
END;
$$;


--
-- Name: wow_attendance_enrolled_image_paths(character varying, character varying); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_enrolled_image_paths(p_person_id character varying, p_id_type character varying) RETURNS text[]
    LANGUAGE sql
    AS $$
  SELECT COALESCE(array_agg(i.image_path), ARRAY[]::TEXT[])
  FROM attendance.wow_attendance_enrollments e
  JOIN attendance.wow_attendance_images i ON i.enrollment_id = e.id
  WHERE e.person_id = p_person_id
    AND e.id_type   = p_id_type
    AND e.is_active = true;
$$;


--
-- Name: wow_attendance_enrolled_list(character varying, text, integer, integer); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_enrolled_list(p_id_type character varying, p_token text, p_page integer, p_limit integer) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_page   INT := GREATEST(COALESCE(p_page, 1), 1);
  v_limit  INT := GREATEST(COALESCE(p_limit, 20), 1);
  v_offset INT;
  v_total  INT;
  v_list   JSONB;
BEGIN
  v_offset := (v_page - 1) * v_limit;

  SELECT count(*)
    INTO v_total
    FROM attendance.wow_attendance_enrollments e
   WHERE e.id_type = p_id_type
     AND e.is_active = true;

  SELECT COALESCE(jsonb_agg(row_to_json(t)::jsonb ORDER BY t.enrolled_at DESC), '[]'::jsonb)
    INTO v_list
  FROM (
    SELECT
      e.person_id                              AS id,
      CASE
        WHEN p_id_type = 'Student'  THEN s.name
        WHEN p_id_type = 'Employee' THEN f.name
        ELSE NULL
      END                                      AS name,
      e.id                                     AS enrollment_id,
      e.enrolled_at                            AS enrolled_at,
      (SELECT count(*) FROM attendance.wow_attendance_images i
        WHERE i.enrollment_id = e.id)          AS image_count,
      e.is_active                              AS is_active
    FROM attendance.wow_attendance_enrollments e
    LEFT JOIN ictcell.lms_student s
           ON p_id_type = 'Student'  AND s.id::VARCHAR = e.person_id
    LEFT JOIN ictcell.lms_faculty f
           ON p_id_type = 'Employee' AND f.emp_id = e.person_id
    WHERE e.id_type = p_id_type
      AND e.is_active = true
    ORDER BY e.enrolled_at DESC
    OFFSET v_offset
    LIMIT  v_limit
  ) t;

  RETURN jsonb_build_object(
    'success', true,
    'data', jsonb_build_object(
      'id_type', p_id_type,
      'total',   v_total,
      'page',    v_page,
      'limit',   v_limit,
      'list',    v_list
    )
  );
END;
$$;


--
-- Name: wow_attendance_enrolled_map(character varying); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_enrolled_map(p_id_type character varying) RETURNS jsonb
    LANGUAGE sql
    AS $$
  SELECT COALESCE(
    jsonb_agg(jsonb_build_object(
      'person_id',   t.person_id,
      'image_paths', t.image_paths
    )),
    '[]'::jsonb
  )
  FROM (
    SELECT e.person_id,
           array_agg(i.image_path) AS image_paths
    FROM attendance.wow_attendance_enrollments e
    JOIN attendance.wow_attendance_images i ON i.enrollment_id = e.id
    WHERE e.id_type   = p_id_type
      AND e.is_active = true
    GROUP BY e.person_id
  ) t;
$$;


--
-- Name: wow_attendance_location_verify(character varying, double precision, double precision); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_location_verify(p_emp_id character varying, p_device_lat double precision, p_device_long double precision) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_body_code varchar;
    v_emp_name varchar;
    v_row      RECORD;
BEGIN
    -- Reject impossible coordinates up front; without this a bad client
    -- silently gets "outside every building" instead of a usable error.
    IF p_device_lat IS NULL OR p_device_long IS NULL
       OR p_device_lat  NOT BETWEEN  -90 AND  90
       OR p_device_long NOT BETWEEN -180 AND 180 THEN
        RETURN jsonb_build_object(
            'success',  false,
            'verified', false,
            'message',  'Invalid device coordinates'
        );
    END IF;

    -- Step 1: emp_id -> office (body code)
    SELECT e.office, e.name_en
      INTO v_body_code, v_emp_name
      FROM attendance.employees e
     WHERE e.emp_id = p_emp_id
     LIMIT 1;

    IF NOT FOUND THEN
        RETURN jsonb_build_object(
            'success',  false,
            'verified', false,
            'message',  'Employee not found'
        );
    END IF;

    -- An employee with no office cannot be mapped to a building. Kept
    -- distinct from "not found" so the two are not confused in support.
    IF v_body_code IS NULL OR btrim(v_body_code) = '' THEN
        RETURN jsonb_build_object(
            'success',  false,
            'verified', false,
            'message',  'Employee has no office assigned'
        );
    END IF;

    -- Steps 2-4: Haversine against every active mapping, closest first.
    -- `least(1, ...)` guards asin()'s domain: floating-point error can push
    -- the argument a hair above 1 for a device sitting on the exact
    -- coordinates of a building, which would raise a math error.
    SELECT m.building_id,
           b.name AS building_name,
           COALESCE(m.radius, 50)::double precision AS radius_m,
           round((
               2 * 6371000 * asin(least(1, sqrt(
                   power(sin(radians(m.lat - p_device_lat) / 2), 2) +
                   cos(radians(p_device_lat)) *
                   cos(radians(m.lat))        *
                   power(sin(radians(m."long" - p_device_long) / 2), 2)
               )))
           )::numeric, 2)::double precision AS distance_m
      INTO v_row
      FROM attendance.body_building_mapping m
      JOIN attendance.buildings b ON b.id = m.building_id
     WHERE m.body_code   = v_body_code
       AND m.is_active
       AND b.status    = 'Active'
       AND m.lat    IS NOT NULL
       AND m."long" IS NOT NULL
     ORDER BY distance_m ASC
     LIMIT 1;

    IF NOT FOUND THEN
        RETURN jsonb_build_object(
            'success',  false,
            'verified', false,
            'message',  'No building mapping found for this employee office',
            'data',     jsonb_build_object('body_code', v_body_code)
        );
    END IF;

    -- Step 5: inside the building's own radius?
    RETURN jsonb_build_object(
        'success',  true,
        'verified', (v_row.distance_m <= v_row.radius_m),
        'message',  CASE WHEN v_row.distance_m <= v_row.radius_m
                         THEN 'Location verified'
                         ELSE 'Device location does not match any mapped building'
                    END,
        'data', jsonb_build_object(
            'emp_id',        p_emp_id,
            'emp_name',      v_emp_name,
            'body_code',     v_body_code,
            'building_id',   v_row.building_id,
            'building_name', v_row.building_name,
            'distance_m',    v_row.distance_m,
            'radius_m',      v_row.radius_m
        )
    );
END;
$$;


--
-- Name: wow_attendance_records_by_date(date, date, character varying, integer, integer); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_records_by_date(p_from_date date, p_to_date date, p_id_type character varying, p_page integer, p_limit integer) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_page   INT := GREATEST(COALESCE(p_page, 1), 1);
  v_limit  INT := GREATEST(COALESCE(p_limit, 20), 1);
  v_offset INT;
  v_total  INT;
  v_list   JSONB;
BEGIN
  v_offset := (v_page - 1) * v_limit;

  SELECT count(*)
    INTO v_total
    FROM attendance.wow_attendance_records r
   WHERE r.created_at::date >= p_from_date
     AND r.created_at::date <= p_to_date
     AND (p_id_type IS NULL OR r.id_type = p_id_type);

  SELECT COALESCE(jsonb_agg(row_to_json(t)::jsonb), '[]'::jsonb)
    INTO v_list
  FROM (
    SELECT
      r.id            AS record_id,
      r.person_id     AS id,
      r.id_type       AS id_type,
      CASE
        WHEN r.id_type = 'Student'  THEN s.name
        WHEN r.id_type = 'Employee' THEN f.name
        ELSE NULL
      END             AS name,
      r.matched       AS matched,
      r.confidence    AS confidence,
      r.live_image    AS live_image,
      r.device_info   AS device_info,
      r.enrollment_id AS enrollment_id,
      r.created_at    AS created_at
    FROM attendance.wow_attendance_records r
    LEFT JOIN ictcell.lms_student s
           ON r.id_type = 'Student'  AND s.id::VARCHAR = r.person_id
    LEFT JOIN ictcell.lms_faculty f
           ON r.id_type = 'Employee' AND f.emp_id = r.person_id
    WHERE r.created_at::date >= p_from_date
      AND r.created_at::date <= p_to_date
      AND (p_id_type IS NULL OR r.id_type = p_id_type)
    ORDER BY r.created_at DESC
    OFFSET v_offset
    LIMIT  v_limit
  ) t;

  RETURN jsonb_build_object(
    'success', true,
    'data', jsonb_build_object(
      'from_date', p_from_date,
      'to_date',   p_to_date,
      'id_type',   p_id_type,
      'total',     v_total,
      'page',      v_page,
      'limit',     v_limit,
      'list',      v_list
    )
  );
END;
$$;


--
-- Name: wow_attendance_records_by_person(character varying, date, date, integer, integer); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_records_by_person(p_person_id character varying, p_from_date date, p_to_date date, p_page integer, p_limit integer) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_page   INT := GREATEST(COALESCE(p_page, 1), 1);
  v_limit  INT := GREATEST(COALESCE(p_limit, 20), 1);
  v_offset INT;
  v_total  INT;
  v_name   TEXT;
  v_list   JSONB;
BEGIN
  v_offset := (v_page - 1) * v_limit;

  SELECT count(*)
    INTO v_total
    FROM attendance.wow_attendance_records r
   WHERE r.person_id = p_person_id
     AND r.created_at::date >= p_from_date
     AND r.created_at::date <= p_to_date;

  -- Resolve the person's display name once (from the most recent record's type).
  SELECT CASE
           WHEN r.id_type = 'Student'  THEN s.name
           WHEN r.id_type = 'Employee' THEN f.name
           ELSE NULL
         END
    INTO v_name
    FROM attendance.wow_attendance_records r
    LEFT JOIN ictcell.lms_student s
           ON r.id_type = 'Student'  AND s.id::VARCHAR = r.person_id
    LEFT JOIN ictcell.lms_faculty f
           ON r.id_type = 'Employee' AND f.emp_id = r.person_id
   WHERE r.person_id = p_person_id
   ORDER BY r.created_at DESC
   LIMIT 1;

  SELECT COALESCE(jsonb_agg(row_to_json(t)::jsonb), '[]'::jsonb)
    INTO v_list
  FROM (
    SELECT
      r.id            AS record_id,
      r.id_type       AS id_type,
      r.matched       AS matched,
      r.confidence    AS confidence,
      r.live_image    AS live_image,
      r.device_info   AS device_info,
      r.enrollment_id AS enrollment_id,
      r.created_at    AS created_at
    FROM attendance.wow_attendance_records r
    WHERE r.person_id = p_person_id
      AND r.created_at::date >= p_from_date
      AND r.created_at::date <= p_to_date
    ORDER BY r.created_at DESC
    OFFSET v_offset
    LIMIT  v_limit
  ) t;

  RETURN jsonb_build_object(
    'success', true,
    'data', jsonb_build_object(
      'id',        p_person_id,
      'name',      v_name,
      'from_date', p_from_date,
      'to_date',   p_to_date,
      'total',     v_total,
      'page',      v_page,
      'limit',     v_limit,
      'list',      v_list
    )
  );
END;
$$;


--
-- Name: wow_attendance_verify(character varying, character varying, text, jsonb, text, boolean, double precision); Type: FUNCTION; Schema: attendance; Owner: -
--

CREATE FUNCTION attendance.wow_attendance_verify(p_person_id character varying, p_id_type character varying, p_token text, p_device_info jsonb, p_live_image text, p_matched boolean, p_confidence double precision) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_enrollment_id UUID;
  v_record_id     UUID;
  v_matched_at    TIMESTAMPTZ;
BEGIN
  SELECT id
    INTO v_enrollment_id
    FROM attendance.wow_attendance_enrollments
   WHERE person_id = p_person_id
     AND id_type   = p_id_type
     AND is_active = true
   ORDER BY enrolled_at DESC
   LIMIT 1;

  IF v_enrollment_id IS NULL THEN
    RETURN jsonb_build_object(
      'success', false,
      'matched', false,
      'message', 'Person is not enrolled',
      'data', jsonb_build_object('id', p_person_id, 'id_type', p_id_type)
    );
  END IF;

  INSERT INTO attendance.wow_attendance_records
    (enrollment_id, person_id, id_type, matched, confidence, live_image, device_info)
  VALUES
    (v_enrollment_id, p_person_id, p_id_type, p_matched, p_confidence, p_live_image, p_device_info)
  RETURNING id, created_at INTO v_record_id, v_matched_at;

  IF p_matched THEN
    RETURN jsonb_build_object(
      'success', true,
      'matched', true,
      'message', 'Attendance marked',
      'data', jsonb_build_object(
        'id',            p_person_id,
        'id_type',       p_id_type,
        'attendance_id', v_record_id,
        'matched_at',    v_matched_at,
        'confidence',    p_confidence
      )
    );
  ELSE
    RETURN jsonb_build_object(
      'success', true,
      'matched', false,
      'message', 'Face did not match enrolled images',
      'data', jsonb_build_object(
        'id',         p_person_id,
        'id_type',    p_id_type,
        'confidence', p_confidence
      )
    );
  END IF;
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: body_building_mapping; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.body_building_mapping (
    id integer NOT NULL,
    body_code character varying(50) NOT NULL,
    building_id integer NOT NULL,
    lat double precision,
    long double precision,
    radius numeric(10,2) DEFAULT 50 NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT body_building_mapping_lat_ck CHECK (((lat IS NULL) OR ((lat >= ('-90'::integer)::double precision) AND (lat <= (90)::double precision)))),
    CONSTRAINT body_building_mapping_long_ck CHECK (((long IS NULL) OR ((long >= ('-180'::integer)::double precision) AND (long <= (180)::double precision)))),
    CONSTRAINT body_building_mapping_radius_ck CHECK ((radius > (0)::numeric))
);


--
-- Name: body_building_mapping_id_seq; Type: SEQUENCE; Schema: attendance; Owner: -
--

CREATE SEQUENCE attendance.body_building_mapping_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: body_building_mapping_id_seq; Type: SEQUENCE OWNED BY; Schema: attendance; Owner: -
--

ALTER SEQUENCE attendance.body_building_mapping_id_seq OWNED BY attendance.body_building_mapping.id;


--
-- Name: buildings; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.buildings (
    id integer NOT NULL,
    name character varying(256) NOT NULL,
    status character varying(32) DEFAULT 'Active'::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: buildings_id_seq; Type: SEQUENCE; Schema: attendance; Owner: -
--

CREATE SEQUENCE attendance.buildings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: buildings_id_seq; Type: SEQUENCE OWNED BY; Schema: attendance; Owner: -
--

ALTER SEQUENCE attendance.buildings_id_seq OWNED BY attendance.buildings.id;


--
-- Name: employees; Type: VIEW; Schema: attendance; Owner: -
--

CREATE VIEW attendance.employees AS
 SELECT id,
    emp_id,
    emp_category,
    name_en,
    name_bn,
    designation_en,
    designation_bn,
    office
   FROM ictcell.employees e;


--
-- Name: VIEW employees; Type: COMMENT; Schema: attendance; Owner: -
--

COMMENT ON VIEW attendance.employees IS 'Read-only projection of ictcell.employees. Owned by duerp-api; never written here.';


--
-- Name: nfc_card_audit; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.nfc_card_audit (
    id bigint NOT NULL,
    card_id bigint,
    student_applicant_id character varying(64) NOT NULL,
    action character varying(24) NOT NULL,
    card_number character varying(64),
    is_verified smallint,
    registration_type smallint,
    changed_fields text[] DEFAULT '{}'::text[] NOT NULL,
    performed_by bigint,
    client_ip character varying(45),
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: nfc_card_audit_id_seq; Type: SEQUENCE; Schema: attendance; Owner: -
--

CREATE SEQUENCE attendance.nfc_card_audit_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: nfc_card_audit_id_seq; Type: SEQUENCE OWNED BY; Schema: attendance; Owner: -
--

ALTER SEQUENCE attendance.nfc_card_audit_id_seq OWNED BY attendance.nfc_card_audit.id;


--
-- Name: nfc_student_cards; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.nfc_student_cards (
    id bigint NOT NULL,
    student_applicant_id character varying(64) NOT NULL,
    card_number character varying(64),
    card_image text,
    student_selfie text,
    is_verified smallint DEFAULT 2 NOT NULL,
    registration_type smallint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT nfc_student_cards_is_verified_ck CHECK ((is_verified = ANY (ARRAY[1, 2]))),
    CONSTRAINT nfc_student_cards_registration_type_ck CHECK ((registration_type = ANY (ARRAY[1, 2])))
);


--
-- Name: TABLE nfc_student_cards; Type: COMMENT; Schema: attendance; Owner: -
--

COMMENT ON TABLE attendance.nfc_student_cards IS 'NFC card <-> student mapping. student_applicant_id is an opaque external key: no applicant registry exists to reference.';


--
-- Name: nfc_student_cards_id_seq; Type: SEQUENCE; Schema: attendance; Owner: -
--

CREATE SEQUENCE attendance.nfc_student_cards_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: nfc_student_cards_id_seq; Type: SEQUENCE OWNED BY; Schema: attendance; Owner: -
--

ALTER SEQUENCE attendance.nfc_student_cards_id_seq OWNED BY attendance.nfc_student_cards.id;


--
-- Name: wow_attendance_enrollments; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.wow_attendance_enrollments (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    person_id character varying NOT NULL,
    id_type character varying NOT NULL,
    device_info jsonb,
    enrolled_at timestamp with time zone DEFAULT now(),
    is_active boolean DEFAULT true,
    version integer DEFAULT 1 NOT NULL,
    previous_enrollment_id uuid
);


--
-- Name: wow_attendance_images; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.wow_attendance_images (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    enrollment_id uuid,
    image_path text NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: wow_attendance_records; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.wow_attendance_records (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    enrollment_id uuid,
    person_id character varying NOT NULL,
    id_type character varying NOT NULL,
    matched boolean NOT NULL,
    confidence numeric(5,4),
    live_image text,
    device_info jsonb,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: wow_attendance_token_mismatch_record; Type: TABLE; Schema: attendance; Owner: -
--

CREATE TABLE attendance.wow_attendance_token_mismatch_record (
    id integer NOT NULL,
    action character varying(16) NOT NULL,
    ai_recognized_id character varying(50),
    requested_user_id character varying(50) NOT NULL,
    ai_requested_id character varying(100),
    ai_similarity double precision,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: wow_attendance_token_mismatch_record_id_seq; Type: SEQUENCE; Schema: attendance; Owner: -
--

CREATE SEQUENCE attendance.wow_attendance_token_mismatch_record_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: wow_attendance_token_mismatch_record_id_seq; Type: SEQUENCE OWNED BY; Schema: attendance; Owner: -
--

ALTER SEQUENCE attendance.wow_attendance_token_mismatch_record_id_seq OWNED BY attendance.wow_attendance_token_mismatch_record.id;


--
-- Name: body_building_mapping id; Type: DEFAULT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.body_building_mapping ALTER COLUMN id SET DEFAULT nextval('attendance.body_building_mapping_id_seq'::regclass);


--
-- Name: buildings id; Type: DEFAULT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.buildings ALTER COLUMN id SET DEFAULT nextval('attendance.buildings_id_seq'::regclass);


--
-- Name: nfc_card_audit id; Type: DEFAULT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_card_audit ALTER COLUMN id SET DEFAULT nextval('attendance.nfc_card_audit_id_seq'::regclass);


--
-- Name: nfc_student_cards id; Type: DEFAULT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_student_cards ALTER COLUMN id SET DEFAULT nextval('attendance.nfc_student_cards_id_seq'::regclass);


--
-- Name: wow_attendance_token_mismatch_record id; Type: DEFAULT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_token_mismatch_record ALTER COLUMN id SET DEFAULT nextval('attendance.wow_attendance_token_mismatch_record_id_seq'::regclass);


--
-- Name: body_building_mapping body_building_mapping_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.body_building_mapping
    ADD CONSTRAINT body_building_mapping_pkey PRIMARY KEY (id);


--
-- Name: body_building_mapping body_building_mapping_uniq; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.body_building_mapping
    ADD CONSTRAINT body_building_mapping_uniq UNIQUE (body_code, building_id);


--
-- Name: buildings buildings_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.buildings
    ADD CONSTRAINT buildings_pkey PRIMARY KEY (id);


--
-- Name: nfc_card_audit nfc_card_audit_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_card_audit
    ADD CONSTRAINT nfc_card_audit_pkey PRIMARY KEY (id);


--
-- Name: nfc_student_cards nfc_student_cards_applicant_uq; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_student_cards
    ADD CONSTRAINT nfc_student_cards_applicant_uq UNIQUE (student_applicant_id);


--
-- Name: nfc_student_cards nfc_student_cards_card_number_uq; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_student_cards
    ADD CONSTRAINT nfc_student_cards_card_number_uq UNIQUE (card_number);


--
-- Name: nfc_student_cards nfc_student_cards_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_student_cards
    ADD CONSTRAINT nfc_student_cards_pkey PRIMARY KEY (id);


--
-- Name: wow_attendance_enrollments wow_attendance_enrollments_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_enrollments
    ADD CONSTRAINT wow_attendance_enrollments_pkey PRIMARY KEY (id);


--
-- Name: wow_attendance_images wow_attendance_images_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_images
    ADD CONSTRAINT wow_attendance_images_pkey PRIMARY KEY (id);


--
-- Name: wow_attendance_records wow_attendance_records_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_records
    ADD CONSTRAINT wow_attendance_records_pkey PRIMARY KEY (id);


--
-- Name: wow_attendance_token_mismatch_record wow_attendance_token_mismatch_record_pkey; Type: CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_token_mismatch_record
    ADD CONSTRAINT wow_attendance_token_mismatch_record_pkey PRIMARY KEY (id);


--
-- Name: body_building_mapping_body_active_idx; Type: INDEX; Schema: attendance; Owner: -
--

CREATE INDEX body_building_mapping_body_active_idx ON attendance.body_building_mapping USING btree (body_code) WHERE is_active;


--
-- Name: nfc_card_audit_applicant_idx; Type: INDEX; Schema: attendance; Owner: -
--

CREATE INDEX nfc_card_audit_applicant_idx ON attendance.nfc_card_audit USING btree (student_applicant_id, created_at DESC);


--
-- Name: nfc_card_audit_card_number_idx; Type: INDEX; Schema: attendance; Owner: -
--

CREATE INDEX nfc_card_audit_card_number_idx ON attendance.nfc_card_audit USING btree (card_number, created_at DESC) WHERE (card_number IS NOT NULL);


--
-- Name: wow_attendance_token_mismatch_created_idx; Type: INDEX; Schema: attendance; Owner: -
--

CREATE INDEX wow_attendance_token_mismatch_created_idx ON attendance.wow_attendance_token_mismatch_record USING btree (created_at DESC);


--
-- Name: wow_attendance_token_mismatch_user_idx; Type: INDEX; Schema: attendance; Owner: -
--

CREATE INDEX wow_attendance_token_mismatch_user_idx ON attendance.wow_attendance_token_mismatch_record USING btree (requested_user_id);


--
-- Name: body_building_mapping body_building_mapping_building_id_fkey; Type: FK CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.body_building_mapping
    ADD CONSTRAINT body_building_mapping_building_id_fkey FOREIGN KEY (building_id) REFERENCES attendance.buildings(id);


--
-- Name: nfc_card_audit nfc_card_audit_card_id_fkey; Type: FK CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.nfc_card_audit
    ADD CONSTRAINT nfc_card_audit_card_id_fkey FOREIGN KEY (card_id) REFERENCES attendance.nfc_student_cards(id) ON DELETE SET NULL;


--
-- Name: wow_attendance_enrollments wow_attendance_enrollments_previous_enrollment_id_fkey; Type: FK CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_enrollments
    ADD CONSTRAINT wow_attendance_enrollments_previous_enrollment_id_fkey FOREIGN KEY (previous_enrollment_id) REFERENCES attendance.wow_attendance_enrollments(id);


--
-- Name: wow_attendance_images wow_attendance_images_enrollment_id_fkey; Type: FK CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_images
    ADD CONSTRAINT wow_attendance_images_enrollment_id_fkey FOREIGN KEY (enrollment_id) REFERENCES attendance.wow_attendance_enrollments(id);


--
-- Name: wow_attendance_records wow_attendance_records_enrollment_id_fkey; Type: FK CONSTRAINT; Schema: attendance; Owner: -
--

ALTER TABLE ONLY attendance.wow_attendance_records
    ADD CONSTRAINT wow_attendance_records_enrollment_id_fkey FOREIGN KEY (enrollment_id) REFERENCES attendance.wow_attendance_enrollments(id);


--
-- PostgreSQL database dump complete
--

\unrestrict D2EIuEMrvhhVOppUV6GKb5TQt3Ks8JZEYWoLlyGHbyZxhtNZzj751Mr7hngiDHS

