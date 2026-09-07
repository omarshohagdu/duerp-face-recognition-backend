-- =====================================================================
-- attendance — dedicated schema for the DU face-attendance service
--
-- Everything this service OWNS, in one schema of its own instead of mixed
-- into `ictcell` alongside duerp-api's tables. Applying this file creates the
-- schema, its six tables and its ten functions from nothing.
--
-- WHAT LIVES HERE
--   attendance.wow_attendance_enrollments            one row per (re-)enrollment
--   attendance.wow_attendance_images                 enrolled face image paths
--   attendance.wow_attendance_records                recorded check-ins
--   attendance.wow_attendance_token_mismatch_record  impersonation audit
--   attendance.buildings                             geo-fence anchor points
--   attendance.body_building_mapping                 office -> building + radius
--   attendance.employees                             VIEW over ictcell.employees
--   + the ten attendance.wow_attendance_* functions the service calls
--
-- WHAT DELIBERATELY STAYS IN ictcell
--   ictcell.employees, ictcell.body, ictcell.lms_student, ictcell.lms_faculty
--       Identity data owned by duerp-api. This service only reads it. Section 4
--       exposes employees as `attendance.employees`, a VIEW — the rows are not
--       copied, so there is exactly one employee record in the database and it
--       stays duerp-api's. body / lms_student / lms_faculty are still read
--       fully qualified; give them the same treatment if you ever need it.
--   ictcell.ext_api_allowed_ips, ictcell.ext_api_call_logs
--       Shared ext-api infrastructure. duerp-api writes to both, so they are
--       not this service's to move. `sql/000_ext_api_infra.sql` still owns
--       them and must be applied separately.
--
-- APPLY
--   psql "$DATABASE_URL" -f docs/attendance_schema.sql
--
--   Idempotent: every statement is CREATE ... IF NOT EXISTS or CREATE OR
--   REPLACE, so re-running it is safe and changes nothing.
--
-- THE CODE MUST CHANGE WITH IT
--   src/routes/wow_attendance.rs still names `ictcell.wow_attendance_*` in
--   every query. Applying this file alone changes nothing at runtime — the
--   service keeps reading the old tables. The two have to ship together.
--
-- Generated from sql/001_wow_attendance.sql, sql/002_location_verify.sql and
-- sql/003_token_mismatch.sql, with the two backward-compatibility migrations
-- dropped: they only make sense against a legacy `ictcell` database.
-- =====================================================================

CREATE SCHEMA IF NOT EXISTS attendance;

-- The UUID primary keys below default to gen_random_uuid(), which is core
-- from PostgreSQL 13 onward — no extension needed. On an older server, install
-- pgcrypto first; it is not available on this cluster (PG 17), so requiring it
-- here would break the file.

-- ---------------------------------------------------------------------
-- 1 · employees — the identity table this schema reads
--
-- The geo-fence needs two things from it: emp_id -> office (the body code the
-- building mapping joins on) and name_en (returned with a verified check-in).
--
-- A VIEW, not a copy. `ictcell.employees` is duerp-api's table and the single
-- source of truth for 6,932 staff records. Duplicating it here would mean two
-- employee lists drifting apart, and a check-in resolving against a stale
-- office would put someone at the wrong building. The view costs nothing, is
-- always current, and is read-only by construction.
--
-- Columns are listed explicitly rather than SELECT *, so a column added
-- upstream cannot silently change what this schema exposes.
--
-- This is also the ONLY place the attendance schema reaches into ictcell for
-- employee data. Moving to a database that has no `ictcell` means replacing
-- this one object — see the standalone table below.
-- ---------------------------------------------------------------------

CREATE OR REPLACE VIEW attendance.employees AS
SELECT e.id,
       e.emp_id,          -- 10-digit employee id; joins wow_attendance_enrollments.person_id
       e.emp_category,
       e.name_en,
       e.name_bn,
       e.designation_en,
       e.designation_bn,
       e.office           -- body code, e.g. '440000'; joins body_building_mapping.body_code
  FROM ictcell.employees e;

COMMENT ON VIEW attendance.employees IS
    'Read-only projection of ictcell.employees. Owned by duerp-api; never written here.';

-- Worth knowing: ictcell.employees carries no indexes, so the emp_id lookup in
-- wow_attendance_location_verify sequentially scans all 6,932 rows on every
-- check-in. Harmless at this size, and not this schema''s table to alter — but
-- if the staff list grows, an index on (emp_id) and (office) belongs upstream.

-- ── Standalone alternative ───────────────────────────────────────────
-- Use this INSTEAD of the view only when attendance runs in its own database
-- with no `ictcell` schema present. It buys independence at the cost of a
-- second employee list that something else must keep in sync — decide who
-- owns that sync before choosing it.
--
-- DROP VIEW IF EXISTS attendance.employees;
--
-- CREATE TABLE IF NOT EXISTS attendance.employees (
--     id             varchar(36),
--     emp_id         varchar(10),
--     emp_category   varchar(5),
--     name_en        varchar(255),
--     name_bn        varchar(255) NOT NULL,
--     designation_en varchar(100),
--     designation_bn varchar(150) NOT NULL,
--     office         varchar(20)
-- );
--
-- -- The two lookups the geo-fence performs. Unlike the ictcell original,
-- -- a standalone copy should carry them from the start.
-- CREATE INDEX IF NOT EXISTS employees_emp_id_idx ON attendance.employees (emp_id);
-- CREATE INDEX IF NOT EXISTS employees_office_idx ON attendance.employees (office);
--
-- -- Seed from the shared database while both are still reachable:
-- -- INSERT INTO attendance.employees SELECT * FROM ictcell.employees;


-- ---------------------------------------------------------------------
-- 2 · Enrollment, images and attendance records
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.wow_attendance_enrollments (
  id                     UUID         DEFAULT gen_random_uuid() PRIMARY KEY,
  person_id              VARCHAR      NOT NULL,   -- student_id or employee_id
  id_type                VARCHAR      NOT NULL,   -- 'Student' | 'Employee'
  device_info            JSONB,
  enrolled_at            TIMESTAMPTZ  DEFAULT now(),
  is_active              BOOLEAN      DEFAULT true,
  version                INT          NOT NULL DEFAULT 1,  -- 1 = first enroll, 2+ = re-enroll
  previous_enrollment_id UUID         REFERENCES attendance.wow_attendance_enrollments(id)  -- retired row this re-enroll superseded
);

CREATE TABLE IF NOT EXISTS attendance.wow_attendance_images (
  id             UUID         DEFAULT gen_random_uuid() PRIMARY KEY,
  enrollment_id  UUID         REFERENCES attendance.wow_attendance_enrollments(id),
  image_path     TEXT         NOT NULL,
  created_at     TIMESTAMPTZ  DEFAULT now()
);

CREATE TABLE IF NOT EXISTS attendance.wow_attendance_records (
  id             UUID         DEFAULT gen_random_uuid() PRIMARY KEY,
  enrollment_id  UUID         REFERENCES attendance.wow_attendance_enrollments(id),
  person_id      VARCHAR      NOT NULL,
  id_type        VARCHAR      NOT NULL,
  matched        BOOLEAN      NOT NULL,
  confidence     NUMERIC(5,4),
  live_image     TEXT,
  device_info    JSONB,
  created_at     TIMESTAMPTZ  DEFAULT now()
);

-- ---------------------------------------------------------------------
-- Function: enroll a person (id + id_type come from query params)
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_enroll(
  p_person_id   VARCHAR,
  p_id_type     VARCHAR,
  p_token       TEXT,
  p_device_info JSONB,
  p_image_paths TEXT[]
)
RETURNS JSONB
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

-- ---------------------------------------------------------------------
-- Function: check whether a person is enrolled (by person_id)
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_check_enrolled(
  p_person_id VARCHAR
)
RETURNS JSONB
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

-- ---------------------------------------------------------------------
-- Function: paginated enrolled list for an id_type
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_enrolled_list(
  p_id_type  VARCHAR,
  p_token    TEXT,
  p_page     INT,
  p_limit    INT
)
RETURNS JSONB
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

-- ---------------------------------------------------------------------
-- Function: record a verification attempt / mark attendance
-- (matching is done in the application layer; matched + confidence
--  are passed in here)
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_verify(
  p_person_id   VARCHAR,
  p_id_type     VARCHAR,
  p_token       TEXT,
  p_device_info JSONB,
  p_live_image  TEXT,
  p_matched     BOOLEAN,
  p_confidence  DOUBLE PRECISION
)
RETURNS JSONB
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

-- ---------------------------------------------------------------------
-- Report: attendance records within a date range (filtered by created_at)
-- Optional id_type filter (NULL = both). Paginated, newest first.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_records_by_date(
  p_from_date DATE,
  p_to_date   DATE,
  p_id_type   VARCHAR,
  p_page      INT,
  p_limit     INT
)
RETURNS JSONB
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

-- ---------------------------------------------------------------------
-- Report: attendance records for one person within a date range
-- (person-wise, filtered by created_at). Paginated, newest first.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_records_by_person(
  p_person_id VARCHAR,
  p_from_date DATE,
  p_to_date   DATE,
  p_page      INT,
  p_limit     INT
)
RETURNS JSONB
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

-- ---------------------------------------------------------------------
-- Helper: enrolled image paths for a person (used by the app before
-- calling the face-match service)
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_enrolled_image_paths(
  p_person_id VARCHAR,
  p_id_type   VARCHAR
)
RETURNS TEXT[]
LANGUAGE sql
AS $$
  SELECT COALESCE(array_agg(i.image_path), ARRAY[]::TEXT[])
  FROM attendance.wow_attendance_enrollments e
  JOIN attendance.wow_attendance_images i ON i.enrollment_id = e.id
  WHERE e.person_id = p_person_id
    AND e.id_type   = p_id_type
    AND e.is_active = true;
$$;

-- ---------------------------------------------------------------------
-- Function: all active enrolled people for an id_type, each with their
-- enrolled image paths. Used by the verify endpoint for 1:N face
-- identification when no person id is supplied.
-- Returns: [ { "person_id": "...", "image_paths": ["...", ...] }, ... ]
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.wow_attendance_enrolled_map(
  p_id_type VARCHAR
)
RETURNS JSONB
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


-- ---------------------------------------------------------------------
-- 3 · Geo-fence: buildings, office mapping, and location verification
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.buildings (
    id         serial PRIMARY KEY,
    name       varchar(256) NOT NULL,
    status     varchar(32)  NOT NULL DEFAULT 'Active',
    created_at timestamptz  NOT NULL DEFAULT now()
);

-- `body_code`, not `body_id`: this column joins `ictcell.employees.office`,
-- which holds `ictcell.body.body_code` ("490010"), NOT `body.body_id` ("OES").
-- Naming it body_id would contradict what body_id means in ictcell.body.
--
-- It is varchar, not int: the codes are zero-paddable, so an int column would
-- break the join on any padded code.
CREATE TABLE IF NOT EXISTS attendance.body_building_mapping (
    id          serial PRIMARY KEY,
    body_code   varchar(50) NOT NULL,
    building_id int         NOT NULL REFERENCES attendance.buildings(id),
    lat         double precision,
    "long"      double precision,
    -- Metres. GPS hardware alone drifts 3-50m, so a radius below ~20m
    -- will reject legitimate check-ins; 50m is the working default.
    radius      numeric(10,2) NOT NULL DEFAULT 50,
    is_active   boolean       NOT NULL DEFAULT true,
    created_at  timestamptz   NOT NULL DEFAULT now(),
    updated_at  timestamptz   NOT NULL DEFAULT now(),
    CONSTRAINT body_building_mapping_uniq UNIQUE (body_code, building_id),
    CONSTRAINT body_building_mapping_lat_ck
        CHECK (lat  IS NULL OR lat  BETWEEN  -90 AND  90),
    CONSTRAINT body_building_mapping_long_ck
        CHECK ("long" IS NULL OR "long" BETWEEN -180 AND 180),
    CONSTRAINT body_building_mapping_radius_ck CHECK (radius > 0)
);

-- The verification query filters on body_code + is_active on every call.
CREATE INDEX IF NOT EXISTS body_building_mapping_body_active_idx
    ON attendance.body_building_mapping (body_code)
    WHERE is_active;

-- ── Function ─────────────────────────────────────────────────

CREATE OR REPLACE FUNCTION attendance.wow_attendance_location_verify(
    p_emp_id      varchar,
    p_device_lat  double precision,
    p_device_long double precision
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
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
$function$;
-- ============================================================
-- Admin: create/update one body -> building mapping
--
-- Endpoint : POST /ext-api/wow-attendance/mapping-save
-- Function : attendance.wow_attendance_body_building_mapping_save
--
-- Upserts on (body_code, building_id), so calling it twice with the same
-- pair edits the existing mapping rather than duplicating it.
--
-- Building selection: pass p_building_id to target an existing building,
-- or leave it NULL and pass p_building_name to find-or-create one by name.
-- The buildings table starts empty, so the name path is what bootstraps it.
-- ============================================================

-- Dropped rather than replaced: CREATE OR REPLACE cannot rename an input
-- parameter, and the first cut of this file called the first argument
-- p_body_id. Nothing else references this function, so dropping is safe.
DROP FUNCTION IF EXISTS attendance.wow_attendance_body_building_mapping_save(
    varchar, int, varchar, double precision, double precision, double precision, boolean);

CREATE OR REPLACE FUNCTION attendance.wow_attendance_body_building_mapping_save(
    p_body_code       varchar,
    p_building_id   int,
    p_building_name varchar,
    p_lat           double precision,
    p_long          double precision,
    -- double precision, not numeric: the handler binds an f64, and Postgres
    -- has only an assignment cast float8 -> numeric, so a numeric parameter
    -- here makes function resolution fail at runtime ("function does not
    -- exist"). Narrowed to the column's numeric(10,2) on insert below.
    p_radius        double precision,
    p_is_active     boolean
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
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
$function$;


-- ---------------------------------------------------------------------
-- 4 · Audit: token / face-ownership mismatches
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.wow_attendance_token_mismatch_record (
    id                serial PRIMARY KEY,
    -- 'Enroll' | 'Verify'
    action            varchar(16)  NOT NULL,
    -- Verify: the identifier the AI recognized the live face as.
    -- Enroll:  the id the caller tried to enroll.
    ai_recognized_id  varchar(50),
    -- The logged-in / token holder the request was authenticated as.
    requested_user_id varchar(50)  NOT NULL,
    -- AI platform's own request_id (correlation id) from its response. NULL until
    -- the platform returns it / for flows with no AI call.
    ai_requested_id   varchar(100),
    -- AI recognition similarity score (recognition only). NULL otherwise.
    ai_similarity     double precision,
    created_at        timestamptz  NOT NULL DEFAULT now()
);

-- Common lookups: "who tried to act as someone else", and recent-first review.
CREATE INDEX IF NOT EXISTS wow_attendance_token_mismatch_user_idx
    ON attendance.wow_attendance_token_mismatch_record (requested_user_id);
CREATE INDEX IF NOT EXISTS wow_attendance_token_mismatch_created_idx
    ON attendance.wow_attendance_token_mismatch_record (created_at DESC);

-- ---------------------------------------------------------------------
-- 5 · Grants
--
-- Override the role when applying:
--   psql "$DATABASE_URL" -v app_role=duerp_attendance -f docs/attendance_schema.sql
-- ---------------------------------------------------------------------

\if :{?app_role}
\else
\set app_role dev_team
\endif

GRANT USAGE ON SCHEMA attendance TO :"app_role";
GRANT SELECT, INSERT, UPDATE, DELETE
   ON ALL TABLES IN SCHEMA attendance TO :"app_role";
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA attendance TO :"app_role";
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA attendance TO :"app_role";

-- Objects added by a later migration inherit the same grants.
ALTER DEFAULT PRIVILEGES IN SCHEMA attendance
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO :"app_role";
ALTER DEFAULT PRIVILEGES IN SCHEMA attendance
    GRANT USAGE, SELECT ON SEQUENCES TO :"app_role";
ALTER DEFAULT PRIVILEGES IN SCHEMA attendance
    GRANT EXECUTE ON FUNCTIONS TO :"app_role";

-- The functions still read the identity tables, so the role needs these too.
-- Granted here for completeness; on the shared database they already exist.
GRANT USAGE ON SCHEMA ictcell TO :"app_role";
GRANT SELECT ON ictcell.employees, ictcell.body,
                ictcell.lms_student, ictcell.lms_faculty TO :"app_role";


-- =====================================================================
-- 6 · Copying existing data across  (OPTIONAL — commented out)
--
-- Only needed when cutting a database that already ran the `ictcell` version.
-- Row counts on lms_dev at the time of writing: buildings 2,
-- body_building_mapping 2, enrollments 17, images 39, records 35,
-- token mismatches 50.
--
-- The column layouts are identical, so `SELECT *` is exact — verified against
-- information_schema before this file was written. Run it with the service
-- STOPPED so nothing writes to the old tables mid-copy.
--
-- Order matters: parents before children. `wow_attendance_enrollments` has a
-- self-referencing FK (previous_enrollment_id), which resolves inside a single
-- INSERT ... SELECT because PostgreSQL checks FK triggers at end of statement.
-- =====================================================================

-- BEGIN;
--
-- INSERT INTO attendance.buildings                            SELECT * FROM ictcell.buildings;
-- INSERT INTO attendance.body_building_mapping                SELECT * FROM ictcell.body_building_mapping;
-- INSERT INTO attendance.wow_attendance_enrollments           SELECT * FROM ictcell.wow_attendance_enrollments;
-- INSERT INTO attendance.wow_attendance_images                SELECT * FROM ictcell.wow_attendance_images;
-- INSERT INTO attendance.wow_attendance_records               SELECT * FROM ictcell.wow_attendance_records;
-- INSERT INTO attendance.wow_attendance_token_mismatch_record SELECT * FROM ictcell.wow_attendance_token_mismatch_record;
--
-- -- serial columns: move each sequence past the copied ids, or the next
-- -- INSERT collides with an existing primary key.
-- SELECT setval(pg_get_serial_sequence('attendance.buildings', 'id'),
--               COALESCE((SELECT max(id) FROM attendance.buildings), 1));
-- SELECT setval(pg_get_serial_sequence('attendance.body_building_mapping', 'id'),
--               COALESCE((SELECT max(id) FROM attendance.body_building_mapping), 1));
-- SELECT setval(pg_get_serial_sequence('attendance.wow_attendance_token_mismatch_record', 'id'),
--               COALESCE((SELECT max(id) FROM attendance.wow_attendance_token_mismatch_record), 1));
--
-- -- Confirm before committing: every count must match its ictcell source.
-- SELECT 'enrollments' t, count(*) FROM attendance.wow_attendance_enrollments
-- UNION ALL SELECT 'images',  count(*) FROM attendance.wow_attendance_images
-- UNION ALL SELECT 'records', count(*) FROM attendance.wow_attendance_records;
--
-- COMMIT;

-- The old ictcell tables are deliberately NOT dropped here. Leave them in
-- place until the service has run against this schema long enough to trust,
-- then drop them in a separate, deliberate migration.
