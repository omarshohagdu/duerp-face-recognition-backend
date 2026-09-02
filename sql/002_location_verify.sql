-- ============================================================
-- Location verification for WOW attendance (PostgreSQL)
--
-- Called by : the location gate inside POST /ext-api/wow-attendance/verify,
--             after face recognition and before attendance is recorded
-- Function  : ictcell.wow_attendance_location_verify
--
-- Flow:
--   1. ictcell.employees.emp_id -> employees.office (body code)
--   2. body_building_mapping WHERE body_code = office
--      -> (lat, long, radius) per mapped building
--   3. Haversine distance: device GPS vs each building GPS
--   4. Closest building wins
--   5. distance <= radius -> verified true / false
-- ============================================================

-- ── Tables ───────────────────────────────────────────────────
-- Neither table existed in `ictcell` before this change; both are
-- created here rather than assumed.

CREATE TABLE IF NOT EXISTS ictcell.buildings (
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
CREATE TABLE IF NOT EXISTS ictcell.body_building_mapping (
    id          serial PRIMARY KEY,
    body_code   varchar(50) NOT NULL,
    building_id int         NOT NULL REFERENCES ictcell.buildings(id),
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

-- Migration for any database that already got the first cut of this file,
-- where the column was called body_id. Renaming keeps the (empty) table and
-- its constraints rather than dropping them; a no-op on a fresh database.
DO $migrate$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'ictcell'
           AND table_name   = 'body_building_mapping'
           AND column_name  = 'body_id'
    ) THEN
        ALTER TABLE ictcell.body_building_mapping RENAME COLUMN body_id TO body_code;
    END IF;
END
$migrate$;

-- The verification query filters on body_code + is_active on every call.
CREATE INDEX IF NOT EXISTS body_building_mapping_body_active_idx
    ON ictcell.body_building_mapping (body_code)
    WHERE is_active;

-- ── Function ─────────────────────────────────────────────────

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_location_verify(
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
      FROM ictcell.employees e
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
      FROM ictcell.body_building_mapping m
      JOIN ictcell.buildings b ON b.id = m.building_id
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
-- Function : ictcell.wow_attendance_body_building_mapping_save
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
DROP FUNCTION IF EXISTS ictcell.wow_attendance_body_building_mapping_save(
    varchar, int, varchar, double precision, double precision, double precision, boolean);

CREATE OR REPLACE FUNCTION ictcell.wow_attendance_body_building_mapping_save(
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
          FROM ictcell.buildings b WHERE b.id = p_building_id;
        IF NOT FOUND THEN
            RETURN jsonb_build_object(
                'success', false,
                'message', format('Building %s not found', p_building_id));
        END IF;

    ELSIF p_building_name IS NOT NULL AND btrim(p_building_name) <> '' THEN
        SELECT b.id, b.name INTO v_building_id, v_building_name
          FROM ictcell.buildings b
         WHERE lower(btrim(b.name)) = lower(btrim(p_building_name))
         LIMIT 1;
        IF NOT FOUND THEN
            INSERT INTO ictcell.buildings (name, status)
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
    INSERT INTO ictcell.body_building_mapping
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
      FROM ictcell.employees e WHERE e.office = btrim(p_body_code);

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
