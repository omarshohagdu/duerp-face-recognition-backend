-- ============================================================
-- Procedure : wow_attendance_location_verification
-- Endpoint  : POST /ext-api/wow-attendance/verify
--
-- Flow:
--   1. employees.emp_id → employees.office (body_id)
--   2. body_building_mapping WHERE body_id = office
--      → list of (lat, long, radius) per building
--   3. Haversine distance: device GPS vs each building GPS
--   4. Find closest building
--   5. distance <= radius → verified true / false
-- ============================================================

DROP PROCEDURE IF EXISTS wow_attendance_location_verification;
DELIMITER $$
CREATE PROCEDURE wow_attendance_location_verification(
    IN p_emp_id      VARCHAR(50),   -- employees.emp_id  e.g. "E-1234"
    IN p_device_lat  DECIMAL(10,7), -- device latitude
    IN p_device_long DECIMAL(10,7)  -- device longitude
)
BEGIN
    DECLARE v_body_id       INT           DEFAULT NULL;
    DECLARE v_emp_name      VARCHAR(255)  DEFAULT NULL;
    DECLARE v_building_id   INT           DEFAULT NULL;
    DECLARE v_building_name VARCHAR(256)  DEFAULT NULL;
    DECLARE v_distance      DECIMAL(10,2) DEFAULT NULL;
    DECLARE v_radius        DECIMAL(10,2) DEFAULT NULL;
    DECLARE v_map_count     INT           DEFAULT 0;

    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('verified', FALSE, 'error', @msg) AS result;
    END;

    -- ── Step 1 ───────────────────────────────────────────────
    -- employees.emp_id → office (body_id)
    -- ─────────────────────────────────────────────────────────
    SELECT office, name_en
    INTO   v_body_id, v_emp_name
    FROM   ictcell.employees
    WHERE  emp_id = p_emp_id
    LIMIT  1;

    IF v_body_id IS NULL THEN
        SELECT JSON_OBJECT(
            'verified', FALSE,
            'error',    CONCAT('Employee not found: ', p_emp_id)
        ) AS result;

    ELSE

        -- ── Step 2 ───────────────────────────────────────────
        -- Count active building mappings for this body
        -- ─────────────────────────────────────────────────────
        SELECT COUNT(*) INTO v_map_count
        FROM body_building_mapping m
        JOIN buildings b ON b.id = m.building_id
        WHERE m.body_id   = v_body_id
          AND m.is_active = 1
          AND b.status    = 'Active'
          AND m.lat       IS NOT NULL
          AND m.`long`    IS NOT NULL;

        IF v_map_count = 0 THEN
            SELECT JSON_OBJECT(
                'verified', FALSE,
                'error',    'No building mapping found for this employee office'
            ) AS result;

        ELSE

            -- ── Step 3 & 4 ───────────────────────────────────
            -- Haversine — find closest building
            -- ─────────────────────────────────────────────────
            SELECT
                m.building_id,
                b.name,
                m.radius,
                ROUND(
                    2 * 6371000 * ASIN(SQRT(
                        POWER(SIN((m.lat    - p_device_lat)  * PI() / 360), 2) +
                        COS(p_device_lat    * PI() / 180)    *
                        COS(m.lat           * PI() / 180)    *
                        POWER(SIN((m.`long` - p_device_long) * PI() / 360), 2)
                    )),
                2) AS distance_m
            INTO
                v_building_id,
                v_building_name,
                v_radius,
                v_distance
            FROM body_building_mapping m
            JOIN buildings b ON b.id = m.building_id
            WHERE m.body_id   = v_body_id
              AND m.is_active = 1
              AND b.status    = 'Active'
              AND m.lat       IS NOT NULL
              AND m.`long`    IS NOT NULL
            ORDER BY distance_m ASC
            LIMIT 1;

            -- ── Step 5 ───────────────────────────────────────
            -- distance <= radius → verified
            -- ─────────────────────────────────────────────────
            IF v_distance <= v_radius THEN
                SELECT JSON_OBJECT(
                    'verified',      TRUE,
                    'emp_id',        p_emp_id,
                    'emp_name',      v_emp_name,
                    'body_id',       v_body_id,
                    'building_id',   v_building_id,
                    'building_name', v_building_name,
                    'distance_m',    v_distance,
                    'radius_m',      v_radius
                ) AS result;
            ELSE
                SELECT JSON_OBJECT(
                    'verified',   FALSE,
                    'emp_id',     p_emp_id,
                    'body_id',    v_body_id,
                    'distance_m', v_distance,
                    'radius_m',   v_radius,
                    'error',      'Device location does not match any mapped building'
                ) AS result;
            END IF;

        END IF;
    END IF;
END$$
DELIMITER ;
