-- =====================================================================
-- wow_attendance — face-based attendance for Students & Employees
-- =====================================================================

-- ---------------------------------------------------------------------
-- Tables
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS ictcell.wow_attendance_enrollments (
  id                     UUID         DEFAULT gen_random_uuid() PRIMARY KEY,
  person_id              VARCHAR      NOT NULL,   -- student_id or employee_id
  id_type                VARCHAR      NOT NULL,   -- 'Student' | 'Employee'
  device_info            JSONB,
  enrolled_at            TIMESTAMPTZ  DEFAULT now(),
  is_active              BOOLEAN      DEFAULT true,
  version                INT          NOT NULL DEFAULT 1,  -- 1 = first enroll, 2+ = re-enroll
  previous_enrollment_id UUID         REFERENCES ictcell.wow_attendance_enrollments(id)  -- retired row this re-enroll superseded
);

-- Re-enroll tracking columns for databases created before they existed.
ALTER TABLE ictcell.wow_attendance_enrollments
  ADD COLUMN IF NOT EXISTS version INT NOT NULL DEFAULT 1;
ALTER TABLE ictcell.wow_attendance_enrollments
  ADD COLUMN IF NOT EXISTS previous_enrollment_id UUID
    REFERENCES ictcell.wow_attendance_enrollments(id);

CREATE TABLE IF NOT EXISTS ictcell.wow_attendance_images (
  id             UUID         DEFAULT gen_random_uuid() PRIMARY KEY,
  enrollment_id  UUID         REFERENCES ictcell.wow_attendance_enrollments(id),
  image_path     TEXT         NOT NULL,
  created_at     TIMESTAMPTZ  DEFAULT now()
);

CREATE TABLE IF NOT EXISTS ictcell.wow_attendance_records (
  id             UUID         DEFAULT gen_random_uuid() PRIMARY KEY,
  enrollment_id  UUID         REFERENCES ictcell.wow_attendance_enrollments(id),
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_enroll(
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
    FROM ictcell.wow_attendance_enrollments
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
  UPDATE ictcell.wow_attendance_enrollments
     SET is_active = false
   WHERE person_id = p_person_id
     AND id_type   = p_id_type
     AND is_active = true;

  INSERT INTO ictcell.wow_attendance_enrollments
    (person_id, id_type, device_info, version, previous_enrollment_id)
  VALUES
    (p_person_id, p_id_type, p_device_info, v_version, v_prev_id)
  RETURNING id INTO v_enrollment_id;

  FOREACH v_path IN ARRAY p_image_paths LOOP
    INSERT INTO ictcell.wow_attendance_images (enrollment_id, image_path)
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_check_enrolled(
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
    (SELECT count(*) FROM ictcell.wow_attendance_images i
      WHERE i.enrollment_id = e.id) AS image_count
    INTO v_row
    FROM ictcell.wow_attendance_enrollments e
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_enrolled_list(
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
    FROM ictcell.wow_attendance_enrollments e
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
      (SELECT count(*) FROM ictcell.wow_attendance_images i
        WHERE i.enrollment_id = e.id)          AS image_count,
      e.is_active                              AS is_active
    FROM ictcell.wow_attendance_enrollments e
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_verify(
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
    FROM ictcell.wow_attendance_enrollments
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

  INSERT INTO ictcell.wow_attendance_records
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_records_by_date(
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
    FROM ictcell.wow_attendance_records r
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
    FROM ictcell.wow_attendance_records r
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_records_by_person(
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
    FROM ictcell.wow_attendance_records r
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
    FROM ictcell.wow_attendance_records r
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
    FROM ictcell.wow_attendance_records r
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_enrolled_image_paths(
  p_person_id VARCHAR,
  p_id_type   VARCHAR
)
RETURNS TEXT[]
LANGUAGE sql
AS $$
  SELECT COALESCE(array_agg(i.image_path), ARRAY[]::TEXT[])
  FROM ictcell.wow_attendance_enrollments e
  JOIN ictcell.wow_attendance_images i ON i.enrollment_id = e.id
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

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_enrolled_map(
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
    FROM ictcell.wow_attendance_enrollments e
    JOIN ictcell.wow_attendance_images i ON i.enrollment_id = e.id
    WHERE e.id_type   = p_id_type
      AND e.is_active = true
    GROUP BY e.person_id
  ) t;
$$;