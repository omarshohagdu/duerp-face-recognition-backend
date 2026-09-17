-- =====================================================================
-- attendance — dedicated schema for the DU face-attendance service
--
-- Everything this service OWNS, in one schema of its own instead of mixed
-- into `ictcell` alongside duerp-api's tables. Applying this file creates the
-- schema, its fifteen tables and its seventeen functions from nothing.
--
-- (Eight tables and fifteen of those functions are the service as it runs
-- today. The other seven tables and the last two functions are section 6, the
-- access-control layer, which nothing reads yet and which is created EMPTY —
-- see its header.)
--
-- WHAT LIVES HERE
--   attendance.wow_attendance_enrollments            one row per (re-)enrollment
--   attendance.wow_attendance_images                 enrolled face image paths
--   attendance.wow_attendance_records                recorded check-ins
--   attendance.wow_attendance_token_mismatch_record  impersonation audit
--   attendance.buildings                             geo-fence anchor points
--   attendance.body_building_mapping                 office -> building + radius
--   attendance.employees                             VIEW over ictcell.employees
--   attendance.nfc_student_cards                     NFC card <-> student mapping
--   attendance.nfc_card_audit                        card assignment history
--   + the ten attendance.wow_attendance_* functions the service calls
--   + attendance.nfc_card_normalize / _labels / _get_info / _save_info
--
-- WHAT DELIBERATELY STAYS IN ictcell
--   ictcell.employees, ictcell.body, ictcell.lms_student, ictcell.lms_faculty
--       Identity data owned by duerp-api. This service only reads it. Section 4
--       exposes employees as `attendance.employees`, a VIEW — the rows are not
--       copied, so there is exactly one employee record in the database and it
--       stays duerp-api's. body / lms_student / lms_faculty are still read
--       fully qualified; give them the same treatment if you ever need it.
--
-- WHAT MOVED OUT OF ictcell
--   attendance.ext_api_allowed_ips, attendance.ext_api_call_logs
--       This service's ext-api gate. It used to read the shared ictcell pair;
--       sql/005_ext_api_attendance_schema.sql gave this service its own copies
--       and copied the wow-attendance / nfc-card rows across. The ictcell pair
--       still exists and duerp-api still uses it — nothing was dropped there,
--       and the two no longer track each other. These tables are created by
--       `sql/000_ext_api_infra.sql`, not by this file, and must be applied
--       separately.
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
-- 5 · NFC card <-> student mapping
--
-- Generated from sql/004_nfc_card.sql. Backs the three `/ext-api/nfc-card/*`
-- endpoints in src/routes/nfc_card.rs; see docs/nfc_card.md.
--
-- These functions answer with `status` ("success" | "error") plus a
-- `message`, NOT the `success` boolean every wow_attendance function returns.
-- That is the NFC contract, requested deliberately; do not "harmonise" it.
--
-- Placed BEFORE the grants below on purpose: those use
-- `GRANT ... ON ALL TABLES IN SCHEMA attendance`, which only reaches tables
-- that already exist when it runs.
--
-- NOT copied from the migration: its `attendance.ext_api_allowed_ips` seed. That
-- table is created by sql/000_ext_api_infra.sql rather than by this file, even
-- though it now lives in this schema — but all three endpoints still need their
-- rows in it or every call is a 403 before the handler runs. Apply sql/000 as
-- well, or insert them by hand.
-- ---------------------------------------------------------------------

-- ---------------------------------------------------------------------
-- 5a · Card-number normalisation
--
-- The single definition of "the same card". A reader may present one UID as
-- `04:A1:B2:C3:D4:E5`, `04-a1-b2-c3-d4-e5` or `04A1B2C3D4E5`; all three are
-- one card and must find one row. Separators are dropped and the rest is
-- upper-cased, so the stored value is the canonical form and the plain UNIQUE
-- constraint below is enough to keep a card on one student.
--
-- IMMUTABLE so it can be used in an index expression if one is ever needed,
-- and so the planner can fold it into a constant on lookup. `nullif(...,'')`
-- means a value that normalises away to nothing is NULL, not an empty string —
-- "no card" has exactly one representation.
--
-- The Rust handler applies the same rules before it logs or validates the
-- value; this function is what actually decides what gets stored and matched,
-- so the two can never drift into disagreeing about a stored row.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.nfc_card_normalize(p_card_number varchar)
RETURNS varchar
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $function$
    SELECT nullif(
        upper(regexp_replace(coalesce(p_card_number, ''), '[^0-9A-Za-z]', '', 'g')),
        ''
    )::varchar
$function$;

COMMENT ON FUNCTION attendance.nfc_card_normalize(varchar) IS
    'Canonical card number: separators stripped, upper-cased, empty -> NULL.';

-- ---------------------------------------------------------------------
-- 5b · Human-readable labels for the two 1/2 enums
--
-- The API returns both the number and its meaning: the number is what a client
-- branches on, the label is what a screen shows. Defined ONCE, here, because
-- both endpoints return it and two inline CASE expressions would eventually
-- disagree about the wording.
--
-- An out-of-range value cannot reach a stored row (there are CHECK constraints
-- for that) but can be passed to this function directly, so it yields NULL
-- rather than silently labelling an unknown code as one of the two.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.nfc_card_labels(
    p_is_verified       smallint,
    p_registration_type smallint
) RETURNS jsonb
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $function$
    SELECT jsonb_build_object(
        'is_verified_label', CASE p_is_verified
                                 WHEN 1 THEN 'Verified'
                                 WHEN 2 THEN 'Not Verified'
                             END,
        'registration_type_label', CASE p_registration_type
                                       WHEN 1 THEN 'Admin'
                                       WHEN 2 THEN 'Self'
                                   END
    )
$function$;

COMMENT ON FUNCTION attendance.nfc_card_labels(smallint, smallint) IS
    'is_verified 1=Verified 2=Not Verified; registration_type 1=Admin 2=Self.';

-- ---------------------------------------------------------------------
-- 5c · The mapping table
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.nfc_student_cards (
    id                   bigserial   PRIMARY KEY,
    -- Student's registration/applicant identifier. One row per student, so
    -- this is the key a save upserts on.
    student_applicant_id varchar(64) NOT NULL,
    -- NFC card UID in canonical form, NULL until a card is assigned. Stays
    -- nullable after assignment too: a reassignment clears it on the student
    -- who lost the card rather than deleting their record.
    card_number          varchar(64),
    -- Filesystem paths, not URLs. The handler turns them into `/uploads/...`
    -- URLs per request, because the public origin is deployment config and
    -- baking it into rows would strand them behind a hostname change.
    card_image           text,
    student_selfie       text,
    -- 1 = Yes, 2 = No.
    is_verified          smallint    NOT NULL DEFAULT 2,
    -- 1 = Admin, 2 = Self.
    registration_type    smallint    NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT nfc_student_cards_applicant_uq
        UNIQUE (student_applicant_id),
    -- Postgres treats NULLs as distinct in a UNIQUE constraint, so every
    -- student without a card can hold NULL while an assigned card belongs to
    -- exactly one student. This is the constraint the 409 protects.
    CONSTRAINT nfc_student_cards_card_number_uq
        UNIQUE (card_number),
    CONSTRAINT nfc_student_cards_is_verified_ck
        CHECK (is_verified IN (1, 2)),
    CONSTRAINT nfc_student_cards_registration_type_ck
        CHECK (registration_type IN (1, 2))
);

COMMENT ON TABLE attendance.nfc_student_cards IS
    'NFC card <-> student mapping. student_applicant_id is an opaque external key: no applicant registry exists to reference.';

-- ---------------------------------------------------------------------
-- 5d · Audit trail
--
-- Business-logic step 6 of the spec ("log who saved it, when, which fields
-- changed"). The step logs and `attendance.ext_api_call_logs` already record the
-- calls, but neither answers the question this table exists for: who held this
-- card before, and when did it move. Reassignment makes that history the
-- point, so it gets a table rather than a grep.
--
-- `performed_by` is the bearer token's `sub`. That is a DU user id, not the
-- applicant id being written — the token carries no applicant identity, so it
-- says which login performed the write and nothing more.
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.nfc_card_audit (
    id                   bigserial   PRIMARY KEY,
    -- Nullable: an 'unassigned' row survives if its card record is ever
    -- deleted, and losing the history is worse than a dangling reference.
    card_id              bigint      REFERENCES attendance.nfc_student_cards(id)
                                     ON DELETE SET NULL,
    student_applicant_id varchar(64) NOT NULL,
    -- 'created' | 'updated' | 'unassigned'
    action               varchar(24) NOT NULL,
    card_number          varchar(64),
    is_verified          smallint,
    registration_type    smallint,
    -- Which columns this write actually changed; empty for a no-op save.
    changed_fields       text[]      NOT NULL DEFAULT '{}',
    performed_by         bigint,
    client_ip            varchar(45),
    created_at           timestamptz NOT NULL DEFAULT now()
);

-- "What happened to this student's card" and "who has held this card".
CREATE INDEX IF NOT EXISTS nfc_card_audit_applicant_idx
    ON attendance.nfc_card_audit (student_applicant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS nfc_card_audit_card_number_idx
    ON attendance.nfc_card_audit (card_number, created_at DESC)
    WHERE card_number IS NOT NULL;

-- ---------------------------------------------------------------------
-- 5e · Function: scan lookup
--
-- Returns the four fields the spec asks for and nothing else — no image
-- paths, no other PII. A turnstile calls this on every tap, so the payload
-- stays small and there is nothing in it worth intercepting.
--
-- Unverified students (is_verified = 2) are returned normally, with the flag
-- set; the caller decides what an unreviewed mapping is allowed to do. This
-- endpoint reports the mapping, it does not police it.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.nfc_card_get_info(p_card_number varchar)
RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_card varchar := attendance.nfc_card_normalize(p_card_number);
    v_row  RECORD;
BEGIN
    IF v_card IS NULL THEN
        RETURN jsonb_build_object(
            'status',  'error',
            'code',    'missing_card_number',
            'message', 'card_number is required');
    END IF;

    SELECT c.student_applicant_id, c.card_number, c.is_verified, c.registration_type
      INTO v_row
      FROM attendance.nfc_student_cards c
     WHERE c.card_number = v_card;

    IF NOT FOUND THEN
        RETURN jsonb_build_object(
            'status',  'error',
            'code',    'card_not_found',
            'message', 'No Data Found');
    END IF;

    RETURN jsonb_build_object(
        'status',  'success',
        'message', 'Data Found',
        'data', jsonb_build_object(
            'student_applicant_id', v_row.student_applicant_id,
            'card_number',          v_row.card_number,
            'is_verified',          v_row.is_verified,
            'registration_type',    v_row.registration_type
        ) || attendance.nfc_card_labels(v_row.is_verified, v_row.registration_type)
    );
END;
$function$;

-- ---------------------------------------------------------------------
-- 5f · Function: registration-status check
--
-- "Has this student's card already been registered?" — the desk-side question,
-- asked before a card is issued, and asked about the STUDENT, not the card.
-- The card-side question ("who does this UID belong to?") is what
-- `nfc_card_get_info` answers on every tap.
--
-- WHAT `p_registration_no` IS MATCHED AGAINST
-- `nfc_student_cards.student_applicant_id` — the same key `nfc_card_save_info`
-- upserts on, whatever the card desk put there. It is an opaque external
-- identifier with no foreign key (see the header of this file), so this
-- function cannot and does not validate it against a student registry: an
-- unknown number and a typo'd one are the same answer, "no registration".
-- Nothing is joined to `ictcell.lms_student` either — that table carries no
-- applicant identifier, so there is no link to follow.
--
-- Compared as `btrim(...)`, exactly what the save stores, and NOT through
-- `nfc_card_normalize`: that function is for card UIDs, which are matched
-- case- and separator-insensitively. Applying it here would let this endpoint
-- report a registration under a spelling the save would treat as a different
-- student. A lookup must never be looser than the write it reports on.
--
-- Same lookup as the other two, DELIBERATELY DIFFERENT PAYLOAD, and that
-- difference is why this is a third function rather than a flag on one of
-- them:
--
--   * `data` is ALWAYS an object — `{}` when there is no registration. The
--     requested contract for this endpoint says so, so a client can read
--     `data` without first checking `status`. `nfc_card_get_info` omits `data`
--     on failure and must keep doing so; its callers are already written
--     against that.
--   * The row is returned WHOLE — images and timestamps included — because
--     the caller is a registration desk deciding what to do next, not a
--     turnstile that must not carry PII it has no use for. The handler swaps
--     the stored paths for public URLs before answering; a direct SQL caller
--     sees the paths, exactly as with `nfc_card_save_info`.
--
-- `is_registered` is `true` whenever a row comes back. It is redundant with
-- `status` by construction and exists so a UI can bind one boolean rather
-- than branch on a string. There is no `is_registered: false` case: a student
-- with no registration has no data object to put it in.
-- ---------------------------------------------------------------------

-- The parameter was `p_card_number` in the first version of this endpoint, and
-- Postgres refuses to rename an input parameter through CREATE OR REPLACE
-- ("cannot change name of input parameter"). Dropping first is what makes a
-- re-run of this file work on a database that already has that version.
-- Harmless on a database that does not, and the function has no dependents —
-- nothing calls it but the handler.
DROP FUNCTION IF EXISTS attendance.nfc_card_reg_status(varchar);

CREATE OR REPLACE FUNCTION attendance.nfc_card_reg_status(p_registration_no varchar)
RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_reg_no varchar := nullif(btrim(coalesce(p_registration_no, '')), '');
    v_row    RECORD;
BEGIN
    IF v_reg_no IS NULL THEN
        RETURN jsonb_build_object(
            'status',  'error',
            'code',    'missing_registration_no',
            'message', 'registration_no is required',
            'data',    '{}'::jsonb);
    END IF;

    -- `card_number IS NOT NULL` is part of the question, not a detail: a
    -- student whose card was re-issued to someone else keeps their record —
    -- images and all — with a NULL card number. They hold no card, so "is
    -- their card registered?" is NO, and a desk told otherwise would refuse to
    -- issue them one.
    SELECT c.student_applicant_id, c.card_number, c.is_verified,
           c.registration_type, c.card_image, c.student_selfie,
           c.created_at, c.updated_at
      INTO v_row
      FROM attendance.nfc_student_cards c
     WHERE c.student_applicant_id = v_reg_no
       AND c.card_number IS NOT NULL;

    -- Not an error in the domain sense — "not registered" is a perfectly good
    -- answer to this question — but it is reported as one, because the agreed
    -- contract pairs `status: "error"` with `data: {}` here. Match on `code`.
    IF NOT FOUND THEN
        RETURN jsonb_build_object(
            'status',  'error',
            'code',    'card_not_found',
            'message', 'No Data Found',
            'data',    '{}'::jsonb);
    END IF;

    RETURN jsonb_build_object(
        'status',  'success',
        'message', 'Registration found for registration no ' || v_row.student_applicant_id,
        'data', jsonb_build_object(
            'is_registered',        true,
            'student_applicant_id', v_row.student_applicant_id,
            -- Echoed under both names: `registration_no` is what the caller
            -- asked with, `student_applicant_id` is what the other two
            -- endpoints call the same value, and a client should not have to
            -- know they are one column.
            'registration_no',      v_row.student_applicant_id,
            'card_number',          v_row.card_number,
            'is_verified',          v_row.is_verified,
            'registration_type',    v_row.registration_type,
            -- Stored filesystem paths; the handler replaces both with public
            -- URLs before answering.
            'card_image',           v_row.card_image,
            'student_selfie',       v_row.student_selfie,
            -- When the registration was first created, and when it last
            -- changed — what "already registered" is usually asked alongside.
            'registered_at',        v_row.created_at,
            'updated_at',           v_row.updated_at
        ) || attendance.nfc_card_labels(v_row.is_verified, v_row.registration_type)
    );
END;
$function$;

COMMENT ON FUNCTION attendance.nfc_card_reg_status(varchar) IS
    'Does this registration no hold a registered card? Matched on student_applicant_id; whole row + is_registered on success; data is always an object, {} when not.';

-- ---------------------------------------------------------------------
-- 5g · Function: save / update a card record
--
-- Upserts on `student_applicant_id` — one card record per student, so a
-- repeated save is an update, not a second row.
--
-- IMAGES ARE OPTIONAL AND NEVER CLEARED BY OMISSION. A save that sends no
-- `card_image` keeps the one already stored (COALESCE, not overwrite):
-- re-verifying a student would otherwise wipe the images the previous save
-- uploaded, and the caller has no way to send "unchanged".
--
-- registration_type = 2 (Self) FORCES is_verified = 2, whatever was passed in,
-- so a self-registration can never mark itself trusted. The spec recommends
-- it; it is enforced here rather than in the handler because this function is
-- the only writer and a second caller must not be able to bypass it.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.nfc_card_save_info(
    p_student_applicant_id varchar,
    p_card_number          varchar,
    p_is_verified          smallint,
    p_registration_type    smallint,
    p_card_image           text,
    p_student_selfie       text,
    p_force_reassign       boolean,
    p_performed_by         bigint,
    p_client_ip            varchar
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
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
            'status',  'error',
            'code',    'missing_fields',
            'message', 'student_applicant_id, card_number, and registration_type are required');
    END IF;

    IF v_reg_type NOT IN (1, 2)
       OR (p_is_verified IS NOT NULL AND p_is_verified NOT IN (1, 2)) THEN
        RETURN jsonb_build_object(
            'status',  'error',
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
            'status',  'error',
            'code',    'invalid_value',
            'message', 'student_applicant_id must be at most 64 characters');
    END IF;

    -- Normalisation has already stripped everything that is not alphanumeric,
    -- so only the length still needs a bound. The floor rejects a truncated
    -- read (a one- or two-character "UID" is a failed scan, not a card).
    IF length(v_card) < 4 OR length(v_card) > 64 THEN
        RETURN jsonb_build_object(
            'status',  'error',
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
                'status',  'error',
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
        'status',  'success',
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
        ) || attendance.nfc_card_labels(v_row.is_verified, v_row.registration_type),
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
            'status',  'error',
            'code',    'card_conflict',
            'message', 'Card already assigned to another student');
END;
$function$;

-- ---------------------------------------------------------------------
-- 6 · Access control — roles, permissions, menus
--
-- Generated from sql/006_access_control.sql. Design and rollout:
-- docs/access_control.md. Backs the per-person gate on /ext-api: which role or
-- per-person override a path requires, and what each client renders.
--
-- This service owns its access model outright — these are its tables, not a
-- view over duerp-api's. They began as a copy of the ERP's RBAC
-- (`ictcell.roles`, `.resources`, `.app_users`, …), the same move sql/005 made
-- for the ext-api gate, and for the same reason: a change made for the ERP
-- must not be able to take this service's gate down, or vice versa.
--
-- OPTIONAL, AND INERT ON ITS OWN. The service does not read these objects yet
-- (the middleware change is docs/access_control.md §6), and nothing refuses
-- anything until an endpoint rule says `enforce = true`.
--
-- WHAT IS NOT HERE — this file gives the SHAPE, sql/006 gives the CONTENT:
--
--   1. The rows. These tables are created EMPTY: no roles, no accounts, no
--      menu. sql/006_access_control.sql copies the ERP's current rows across,
--      ids preserved, and resets the sequences past them.
--
--   2. The endpoint rules — one row per /ext-api path, seeded in audit mode.
--      An empty rules table is safe today and becomes a total lockout the
--      moment the middleware ships, because a path with no rule is denied by
--      design. Apply sql/006 before enabling the check.
--
--   3. The `/ext-api/me/access` row in `attendance.ext_api_allowed_ips`, for
--      the same reason section 5 leaves the NFC rows out: that table is
--      created by sql/000_ext_api_infra.sql, not by this file.
--
-- Keep the two copies in step — a change to the objects below belongs in
-- sql/006_access_control.sql too.
--
-- Placed BEFORE the grants, like section 5, because those use
-- `GRANT ... ON ALL TABLES IN SCHEMA attendance`, which only reaches tables
-- that already exist when it runs.
-- ---------------------------------------------------------------------

-- ---------------------------------------------------------------------
-- 6a · The RBAC tables
--
-- Column-for-column what `ictcell` holds, so the rows copy across unchanged
-- and anyone who knows the ERP's model already knows this one. Every foreign
-- key points INSIDE `attendance`: a cross-schema FK would leave this service's
-- access model breaking whenever duerp-api deleted a row, which is exactly the
-- coupling this move exists to remove.
--
-- One addition, and it is the reason the move is convenient rather than
-- merely defensible: `menu_items.platforms`, which needed another team's
-- agreement while the table was theirs (docs/access_control.md §13) and is
-- simply a column here.
-- ---------------------------------------------------------------------

-- A named role: 'admin', 'faculty', 'card_desk'. `is_system` marks the ones
-- the ERP ships and nobody should rename.
CREATE TABLE IF NOT EXISTS attendance.roles (
    id         integer     PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    key        text        NOT NULL UNIQUE,
    name       text        NOT NULL,
    is_system  boolean     NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE attendance.roles IS
    'Named roles. Copied from ictcell.roles; this service reads THIS copy. See docs/access_control.md.';

-- The permission vocabulary. One key gates a desktop menu item, a mobile tab
-- and the endpoints behind them, so a role change moves all three together.
CREATE TABLE IF NOT EXISTS attendance.resources (
    id         integer     PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    key        text        NOT NULL UNIQUE,
    name       text        NOT NULL,
    category   text,
    created_at timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE attendance.resources IS
    'Permission keys ("nfc.card.register"). The shared vocabulary for menus, features and endpoint rules.';

-- Which permissions a role holds.
CREATE TABLE IF NOT EXISTS attendance.role_permissions (
    role_id     integer NOT NULL REFERENCES attendance.roles(id)     ON DELETE CASCADE,
    resource_id integer NOT NULL REFERENCES attendance.resources(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, resource_id)
);

-- One row per person who can sign in. `person_id` is the token's `sub`.
--
-- `du_base_role` is DU's own free-text role string, kept because it is what
-- the accounts currently carry and the only clue for a backfill; `role_id` is
-- the one this service decides with. See docs/access_control.md §7.4 — today
-- `role_id` is NULL for every copied row, which means everybody is denied
-- until somebody fills it in.
CREATE TABLE IF NOT EXISTS attendance.app_users (
    id            integer     PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    person_id     bigint      NOT NULL UNIQUE,
    username      text,
    du_base_role  text,
    role_id       integer     REFERENCES attendance.roles(id),
    -- Anything other than 'active' refuses every gated call immediately,
    -- whatever the person's token says. Tokens live ~730 hours with no
    -- revocation list, so this is the kill switch.
    status        text        NOT NULL DEFAULT 'active',
    last_login_at timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE attendance.app_users IS
    'Sign-in accounts. person_id = the bearer token''s `sub`. Copied from ictcell.app_users; carries DU email addresses.';

-- Per-person exceptions to the role. This is "allow one specific person" and
-- "everyone in this role except her", and the PK means a person has at most
-- one verdict per permission.
CREATE TABLE IF NOT EXISTS attendance.user_permission_overrides (
    user_id     integer NOT NULL REFERENCES attendance.app_users(id) ON DELETE CASCADE,
    resource_id integer NOT NULL REFERENCES attendance.resources(id) ON DELETE CASCADE,
    effect      text    NOT NULL CHECK (effect IN ('grant', 'deny')),
    PRIMARY KEY (user_id, resource_id)
);

-- The navigation tree each client renders, gated by `resource_id` and scoped
-- by `platforms`. A row with no `resource_id` is a container: it carries no
-- permission of its own and appears only if something under it survived.
CREATE TABLE IF NOT EXISTS attendance.menu_items (
    id          integer PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    parent_id   integer REFERENCES attendance.menu_items(id) ON DELETE CASCADE,
    label       text    NOT NULL,
    icon        text,
    route       text,
    resource_id integer REFERENCES attendance.resources(id),
    sort_order  integer NOT NULL DEFAULT 0,
    is_active   boolean NOT NULL DEFAULT true,
    -- DEFAULT '{desktop}' is what keeps a copied ERP menu behaving exactly as
    -- it does today: a mobile client gets only rows somebody deliberately
    -- marked for it, never a desktop tree squeezed onto a phone.
    platforms   text[]  NOT NULL DEFAULT '{desktop}'
);

COMMENT ON COLUMN attendance.menu_items.platforms IS
    'Which clients render this item: desktop | mobile | kiosk. Read by attendance.access_profile().';

-- ---------------------------------------------------------------------
-- 6b · Endpoint → permission map
--
-- The missing link: `resources` holds permission keys, `ext_api_allowed_ips`
-- holds paths, and until now nothing said which permission a path requires.
--
-- Matched on the FULL path, exactly, like `ext_api_allowed_ips` — not a
-- prefix. A new endpoint is unreachable until someone says who may reach it.
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.ext_api_endpoint_permissions (
    id            bigserial    PRIMARY KEY,
    -- e.g. '/ext-api/nfc-card/save_card_info'
    endpoint      varchar(255) NOT NULL,
    -- NULL = "any authenticated caller": today's behaviour, written down, for
    -- the paths that genuinely need no role (the access profile reports only
    -- the caller's own access; a turnstile may be deliberately open).
    resource_key  text,
    -- false = AUDIT MODE. The verdict is computed and logged; the request is
    -- served anyway. This is how the rollout happens without locking anyone
    -- out (docs §8), and how a mistake is undone with one UPDATE and no
    -- redeploy.
    enforce       boolean      NOT NULL DEFAULT false,
    is_active     boolean      NOT NULL DEFAULT true,
    -- Why this row is the way it is — especially why a resource_key is NULL,
    -- which the next reader will otherwise take for an oversight.
    note          text,
    created_at    timestamptz  NOT NULL DEFAULT now(),
    updated_at    timestamptz  NOT NULL DEFAULT now(),

    CONSTRAINT ext_api_endpoint_permissions_endpoint_uq UNIQUE (endpoint)
);

COMMENT ON TABLE attendance.ext_api_endpoint_permissions IS
    'Which attendance.resources key each /ext-api path requires. enforce=false means audit-only. No row = denied, once the middleware asks.';

CREATE INDEX IF NOT EXISTS ext_api_endpoint_permissions_active_idx
    ON attendance.ext_api_endpoint_permissions (endpoint)
    WHERE is_active;

-- ---------------------------------------------------------------------
-- 6b2 · The audit trail the rollout is read from
--
-- `ExtAuthMiddleware` writes one row here for every request it WOULD have
-- refused — in audit mode that is most of them, which is the point: this table
-- is how "who breaks if I enforce this endpoint?" becomes a query instead of a
-- log trawl (docs/access_control.md §8).
--
-- FOLDED, NOT APPENDED. One row per (person, endpoint, reason), with a counter,
-- because during the audit phase every single tap is a denial — nobody has a
-- role yet — and a row each would be a write per request for a report nobody
-- reads line by line. The unique index is `NULLS NOT DISTINCT` so the
-- no-token case (person_id IS NULL) folds into one row too, rather than one
-- per request.
--
-- The write is detached in the handler path: a failed INSERT here must never
-- delay or fail a request the verdict already allowed.
--
-- `token_source` is what makes docs §7.1 measurable: 'legacy' means the caller
-- arrived on a DU token whose signature CANNOT be verified, so the identity
-- this row records is unproven. Those have to reach zero before enforcement
-- means anything.
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.ext_api_access_audit (
    id            bigserial    PRIMARY KEY,
    -- NULL = no usable bearer token on the request.
    person_id     bigint,
    endpoint      varchar(255) NOT NULL,
    -- The verdict's `reason`: no_rule, not_in_role, no_account,
    -- account_inactive, denied_by_override, unknown_resource, check_failed.
    reason        text         NOT NULL,
    -- 'ours' | 'legacy' | 'none' — see the note above.
    token_source  text,
    -- What the endpoint's rule said at the time...
    rule_enforces boolean      NOT NULL DEFAULT false,
    -- ...and whether the request was actually refused. `false` while auditing.
    refused       boolean      NOT NULL DEFAULT false,
    hits          bigint       NOT NULL DEFAULT 1,
    first_seen    timestamptz  NOT NULL DEFAULT now(),
    last_seen     timestamptz  NOT NULL DEFAULT now(),

    CONSTRAINT ext_api_access_audit_uq
        UNIQUE NULLS NOT DISTINCT (person_id, endpoint, reason)
);

COMMENT ON TABLE attendance.ext_api_access_audit IS
    'Would-be and actual access refusals, folded by (person, endpoint, reason). Written by ExtAuthMiddleware; read during the enforcement rollout.';

CREATE INDEX IF NOT EXISTS ext_api_access_audit_endpoint_idx
    ON attendance.ext_api_access_audit (endpoint, last_seen DESC);

-- ---------------------------------------------------------------------
-- 6c · The decision: may this person call this endpoint?
--
-- One function, so the rule holds for anything that asks — the middleware, a
-- psql prompt, a future admin screen. It returns a VERDICT OBJECT rather than
-- a boolean because the caller needs three things: whether to allow, whether
-- this endpoint is enforcing yet, and WHY — "nobody configured this path" and
-- "you are not in the role" send an operator to completely different places.
--
-- It FAILS CLOSED at every unknown: no rule, no account, no such resource key.
-- The one exception is an explicit `resource_key IS NULL`, which is somebody
-- writing down "open to any authenticated caller" on purpose.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.ext_api_can_call(
    p_person_id bigint,
    p_endpoint  varchar
) RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_rule     RECORD;
    v_user     RECORD;
    v_resource integer;
    v_effect   text;
    v_allowed  boolean;
BEGIN
    SELECT * INTO v_rule
      FROM attendance.ext_api_endpoint_permissions
     WHERE endpoint = p_endpoint
       AND is_active;

    -- No rule, or a deactivated one. Denied — but `enforce` is reported as
    -- true so the middleware refuses rather than waving it through: an
    -- unconfigured path is exactly the case this must not be lenient about.
    IF NOT FOUND THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', true,
            'reason', 'no_rule', 'endpoint', p_endpoint);
    END IF;

    IF v_rule.resource_key IS NULL THEN
        RETURN jsonb_build_object('allowed', true, 'enforce', v_rule.enforce,
            'reason', 'open_to_authenticated');
    END IF;

    SELECT id INTO v_resource FROM attendance.resources WHERE key = v_rule.resource_key;

    -- A typo'd key must never read as "allowed". It is a config error, and the
    -- reason code says so rather than blaming the caller.
    IF v_resource IS NULL THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'unknown_resource', 'resource_key', v_rule.resource_key);
    END IF;

    SELECT au.id, au.role_id, au.status, r.key AS role_key
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    IF NOT FOUND THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'no_account', 'person_id', p_person_id);
    END IF;

    -- The kill switch. Tokens live ~730 hours and there is no revocation list,
    -- so this is what makes "stop that person now" possible at all: the next
    -- request is refused whatever their token says.
    IF coalesce(v_user.status, '') <> 'active' THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'account_inactive', 'status', v_user.status);
    END IF;

    -- A per-person override beats the role, either way. "Everyone in this role
    -- except her" has to be expressible, and it only is if a person-level deny
    -- outranks a role-level grant. At most one row can match: the table's PK is
    -- (user_id, resource_id), so there is no grant-vs-deny tie to break here.
    SELECT effect INTO v_effect
      FROM attendance.user_permission_overrides
     WHERE user_id = v_user.id AND resource_id = v_resource;

    IF v_effect = 'deny' THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'denied_by_override');
    ELSIF v_effect = 'grant' THEN
        RETURN jsonb_build_object('allowed', true, 'enforce', v_rule.enforce,
            'reason', 'granted_by_override');
    END IF;

    v_allowed := EXISTS (
        SELECT 1 FROM attendance.role_permissions rp
         WHERE rp.role_id = v_user.role_id
           AND rp.resource_id = v_resource);

    RETURN jsonb_build_object('allowed', v_allowed, 'enforce', v_rule.enforce,
        'reason', CASE WHEN v_allowed THEN 'granted_by_role' ELSE 'not_in_role' END,
        'role', v_user.role_key,
        'resource_key', v_rule.resource_key);
END;
$function$;

COMMENT ON FUNCTION attendance.ext_api_can_call(bigint, varchar) IS
    'May this person call this path? {allowed, enforce, reason, ...}. Fails closed on no rule / no account / unknown resource.';

-- ---------------------------------------------------------------------
-- 6d · The access profile: what one person may do, and what to render
--
-- Backs `POST /ext-api/me/access` (docs §14), which both the desktop SPA and
-- the mobile app call right after login. Returns the caller's permissions, the
-- menu already filtered for their platform AND their role, and a version hash
-- the client can send back to skip a re-render.
--
-- THE MENU IS A VIEW, NOT A GATE. Hiding an item is a courtesy to the user;
-- `ext_api_can_call` above is the thing that refuses. Both read the same
-- tables so they cannot disagree — but a client that trusts this payload for
-- anything but rendering is a client with no access control at all.
--
-- The permission set here and the verdict in 6c MUST stay in step: role
-- grants, plus per-person grants, minus per-person denies. A UI that offers a
-- button the API then refuses is a bug report every single time.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.access_profile(
    p_person_id bigint,
    p_platform  text DEFAULT 'desktop'
) RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_user  RECORD;
    v_perms text[];
    v_menu  jsonb;
BEGIN
    SELECT au.id, au.person_id, au.username, au.status, au.role_id,
           r.key AS role_key, r.name AS role_name
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    -- No account, or a suspended one: an EMPTY profile, not an error. The
    -- client renders a bare shell and every endpoint still refuses them; a
    -- failure here would only make the app look broken to someone who is
    -- merely unprivileged.
    IF NOT FOUND OR coalesce(v_user.status, '') <> 'active' THEN
        RETURN jsonb_build_object(
            'status',  'success',
            'message', 'No access profile for this user',
            'data', jsonb_build_object(
                'person_id',   p_person_id,
                'username',    NULL,
                'role',        NULL,
                'role_name',   NULL,
                'platform',    p_platform,
                'permissions', '[]'::jsonb,
                'menu',        '[]'::jsonb,
                'version',     md5('')));
    END IF;

    SELECT coalesce(array_agg(DISTINCT res.key ORDER BY res.key), '{}')
      INTO v_perms
      FROM attendance.resources res
     WHERE (EXISTS (SELECT 1 FROM attendance.role_permissions rp
                     WHERE rp.role_id = v_user.role_id
                       AND rp.resource_id = res.id)
            OR EXISTS (SELECT 1 FROM attendance.user_permission_overrides o
                        WHERE o.user_id = v_user.id
                          AND o.resource_id = res.id
                          AND o.effect = 'grant'))
       AND NOT EXISTS (SELECT 1 FROM attendance.user_permission_overrides o
                        WHERE o.user_id = v_user.id
                          AND o.resource_id = res.id
                          AND o.effect = 'deny');

    WITH visible AS (
        SELECT m.*, res.key AS resource_key
          FROM attendance.menu_items m
          LEFT JOIN attendance.resources res ON res.id = m.resource_id
         WHERE m.is_active
           AND p_platform = ANY(m.platforms)
           -- A row with no resource_id is a container ("User Management"). It
           -- carries no permission of its own and survives only if something
           -- under it did — see the final WHERE.
           AND (m.resource_id IS NULL OR res.key = ANY(v_perms))
    ),
    children AS (
        SELECT parent_id,
               jsonb_agg(jsonb_build_object(
                   'id', id, 'label', label, 'icon', icon,
                   'route', route, 'permission', resource_key)
                   ORDER BY sort_order) AS items
          FROM visible
         WHERE parent_id IS NOT NULL
         GROUP BY parent_id
    )
    SELECT coalesce(jsonb_agg(jsonb_build_object(
               'id', v.id, 'label', v.label, 'icon', v.icon, 'route', v.route,
               'permission', v.resource_key,
               'children', coalesce(c.items, '[]'::jsonb))
               ORDER BY v.sort_order), '[]'::jsonb)
      INTO v_menu
      FROM visible v
      LEFT JOIN children c ON c.parent_id = v.id
     WHERE v.parent_id IS NULL
       -- An empty dropdown is worse than no dropdown: it tells the user there
       -- is something there and then refuses to open.
       AND (v.resource_id IS NOT NULL OR c.items IS NOT NULL);

    RETURN jsonb_build_object(
        'status',  'success',
        'message', 'Access profile',
        'data', jsonb_build_object(
            'person_id',   v_user.person_id,
            'username',    v_user.username,
            'role',        v_user.role_key,
            'role_name',   v_user.role_name,
            'platform',    p_platform,
            'permissions', to_jsonb(v_perms),
            'menu',        v_menu,
            -- Cheap change-detection for the client, and the cheapest possible
            -- "your access changed" signal. Covers both halves of the payload,
            -- so a menu edit invalidates it as surely as a role change does.
            'version',     md5(v_perms::text || v_menu::text)));
END;
$function$;

COMMENT ON FUNCTION attendance.access_profile(bigint, text) IS
    'One person''s permissions + platform-filtered menu for POST /ext-api/me/access. Rendering hints only — attendance.ext_api_can_call() is the gate.';

-- ---------------------------------------------------------------------
-- 7 · Grants
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
-- 8 · Copying existing data across  (OPTIONAL — commented out)
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
