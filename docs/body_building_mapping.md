# Body-Building Mapping Module

**Database:** `ict_mis` | **Engine:** MySQL / MariaDB | **Language:** SQL Stored Procedures

---

## Table of Contents

1. [Overview](#1-overview)
2. [Entity Relationship](#2-entity-relationship)
3. [Table Definitions](#3-table-definitions)
   - [buildings](#31-buildings-existing)
   - [body_building_mapping](#32-body_building_mapping-new)
4. [Indexes](#4-indexes)
5. [Stored Procedures](#5-stored-procedures)
   - [building_create](#51-building_create)
   - [building_update](#52-building_update)
   - [building_delete](#53-building_delete)
   - [building_list](#54-building_list)
   - [building_get_single](#55-building_get_single)
   - [body_building_mapping_save](#56-body_building_mapping_save)
   - [body_building_mapping_get](#57-body_building_mapping_get)
   - [body_building_mapping_delete](#58-body_building_mapping_delete)
6. [Usage Examples](#6-usage-examples)

---

## 1. Overview

This module manages the mapping between **bodies** (departments, institutes, bureaus) and **buildings** within the university campus.

Key rules:
- One body can occupy **multiple buildings**.
- Each building mapping stores **floor/room remarks** and **GPS coordinates**.
- Both `buildings` and `body_building_mapping` support **soft-delete** (no physical row removal).
- The `body_building_mapping_save` procedure replaces all existing mappings for a body in a single call — ideal for a multi-select form submission.

---

## 2. Entity Relationship

```
ict_mis.body (id)
    │
    │  1 : Many
    ▼
body_building_mapping (body_id, building_id, remarks, lat, long)
    │
    │  Many : 1
    ▼
buildings (id, name, location, status)
```

| Relationship | Cardinality | Description |
|---|---|---|
| `body` → `body_building_mapping` | 1 : Many | One body maps to many buildings |
| `buildings` → `body_building_mapping` | 1 : Many | One building can be mapped to many bodies |

---

## 3. Table Definitions

### 3.1 `buildings` (existing)

Stores physical building records across the campus.

| Column | Data Type | Constraint | Description |
|---|---|---|---|
| `id` | `INT(11)` | `PRIMARY KEY AUTO_INCREMENT` | Unique building identifier |
| `name` | `VARCHAR(256)` | `NOT NULL` | Building display name |
| `body_id` | `INT(11)` | `DEFAULT NULL` | Optional FK → body (legacy reference) |
| `location` | `VARCHAR(256)` | `DEFAULT NULL` | Human-readable location / area |
| `status` | `ENUM('Active','Inactive')` | `DEFAULT 'Active'` | Soft-delete flag |
| `created_at` | `TIMESTAMP` | `DEFAULT CURRENT_TIMESTAMP` | Row creation timestamp |
| `updated_at` | `TIMESTAMP` | `DEFAULT NULL` | Last update timestamp |

---

### 3.2 `body_building_mapping` (new)

Maps a body to one or more buildings with per-entry remarks and GPS coordinates.

| Column | Data Type | Constraint | Description |
|---|---|---|---|
| `id` | `INT(11)` | `PRIMARY KEY AUTO_INCREMENT` | Unique mapping identifier |
| `body_id` | `INT(11)` | `NOT NULL` | FK → `ict_mis.body.id` |
| `building_id` | `INT(11)` | `NOT NULL` | FK → `buildings.id` |
| `building_remarks` | `VARCHAR(500)` | `DEFAULT NULL` | Floor, room, or extra location notes |
| `lat` | `DECIMAL(10,7)` | `DEFAULT NULL` | Latitude coordinate |
| `long` | `DECIMAL(10,7)` | `DEFAULT NULL` | Longitude coordinate |
| `is_active` | `TINYINT(1)` | `NOT NULL DEFAULT 1` | Soft-delete flag |
| `created_at` | `TIMESTAMP` | `DEFAULT CURRENT_TIMESTAMP` | Row creation timestamp |
| `updated_at` | `TIMESTAMP` | `ON UPDATE CURRENT_TIMESTAMP` | Last update timestamp |

**Unique Constraint:** `UNIQUE KEY uq_body_building (body_id, building_id)`

```sql
CREATE TABLE `body_building_mapping` (
    `id`                INT(11)       NOT NULL AUTO_INCREMENT,
    `body_id`           INT(11)       NOT NULL COMMENT 'FK → ict_mis.body.id',
    `building_id`       INT(11)       NOT NULL COMMENT 'FK → buildings.id',
    `building_remarks`  VARCHAR(500)  DEFAULT NULL,
    `lat`               DECIMAL(10,7) DEFAULT NULL,
    `long`              DECIMAL(10,7) DEFAULT NULL,
    `is_active`         TINYINT(1)    NOT NULL DEFAULT 1,
    `created_at`        TIMESTAMP     NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at`        TIMESTAMP     NULL DEFAULT NULL ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (`id`),
    UNIQUE KEY `uq_body_building` (`body_id`, `building_id`),
    KEY `idx_body_id`     (`body_id`),
    KEY `idx_building_id` (`building_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
```

---

## 4. Indexes

| Index Name | Table | Column(s) | Purpose |
|---|---|---|---|
| `PRIMARY` | `body_building_mapping` | `id` | Row lookup |
| `uq_body_building` | `body_building_mapping` | `body_id, building_id` | Prevent duplicate mapping |
| `idx_body_id` | `body_building_mapping` | `body_id` | Fast filter by body |
| `idx_building_id` | `body_building_mapping` | `building_id` | Fast filter by building |

---

## 5. Stored Procedures

---

### 5.1 `building_create`

Inserts a new building record.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_name` | `VARCHAR(256)` | Building display name (required) |
| `p_body_id` | `INT` | Optional body reference |
| `p_location` | `VARCHAR(256)` | Area / location description |

**Returns**

```json
{ "success": true,  "building_id": 21 }
{ "success": false, "error": "message" }
```

```sql
CALL building_create('New Science Block', 4, 'Curzon Hall Area, Floor-5');
```

```sql
DROP PROCEDURE IF EXISTS building_create;
DELIMITER $$
CREATE PROCEDURE building_create(
    IN p_name     VARCHAR(256),
    IN p_body_id  INT,
    IN p_location VARCHAR(256)
)
BEGIN
    DECLARE v_building_id INT DEFAULT 0;
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    IF p_name IS NULL OR TRIM(p_name) = '' THEN
        SELECT JSON_OBJECT('success', FALSE, 'error', 'Building name cannot be empty') AS result;
    ELSE
        INSERT INTO buildings (name, body_id, location, status)
        VALUES (TRIM(p_name), p_body_id, p_location, 'Active');

        SET v_building_id = LAST_INSERT_ID();

        SELECT JSON_OBJECT(
            'success',     TRUE,
            'building_id', v_building_id
        ) AS result;
    END IF;
END$$
DELIMITER ;
```

---

### 5.2 `building_update`

Updates name, body reference, location, and status of an existing building.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_building_id` | `INT` | Building ID to update (required) |
| `p_name` | `VARCHAR(256)` | New building name (required) |
| `p_body_id` | `INT` | New body reference (nullable) |
| `p_location` | `VARCHAR(256)` | New location description (nullable) |
| `p_status` | `VARCHAR(10)` | `'Active'` or `'Inactive'` |

**Returns**

```json
{ "success": true }
{ "success": false, "error": "message" }
```

```sql
CALL building_update(1, 'Mokarram Bhaban (Revised)', 25, 'Mokarram Area, Ground Floor', 'Active');
```

```sql
DROP PROCEDURE IF EXISTS building_update;
DELIMITER $$
CREATE PROCEDURE building_update(
    IN p_building_id INT,
    IN p_name        VARCHAR(256),
    IN p_body_id     INT,
    IN p_location    VARCHAR(256),
    IN p_status      VARCHAR(10)
)
BEGIN
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    IF p_name IS NULL OR TRIM(p_name) = '' THEN
        SELECT JSON_OBJECT('success', FALSE, 'error', 'Building name cannot be empty') AS result;
    ELSE
        UPDATE buildings
        SET name       = TRIM(p_name),
            body_id    = p_body_id,
            location   = p_location,
            status     = IFNULL(p_status, 'Active'),
            updated_at = CURRENT_TIMESTAMP
        WHERE id = p_building_id;

        IF ROW_COUNT() = 0 THEN
            SELECT JSON_OBJECT('success', FALSE, 'error', 'Building not found') AS result;
        ELSE
            SELECT JSON_OBJECT('success', TRUE) AS result;
        END IF;
    END IF;
END$$
DELIMITER ;
```

---

### 5.3 `building_delete`

Soft-deletes a building by setting `status = 'Inactive'`.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_building_id` | `INT` | Building ID to delete |

**Returns**

```json
{ "success": true }
{ "success": false, "error": "Building not found or already inactive" }
```

> **Note:** Physical row is never removed. Active mappings in `body_building_mapping` referencing this building will no longer appear in `body_building_mapping_get` results since it filters `buildings.status = 'Active'`.

```sql
CALL building_delete(5);
```

```sql
DROP PROCEDURE IF EXISTS building_delete;
DELIMITER $$
CREATE PROCEDURE building_delete(
    IN p_building_id INT
)
BEGIN
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    UPDATE buildings
    SET status     = 'Inactive',
        updated_at = CURRENT_TIMESTAMP
    WHERE id     = p_building_id
      AND status = 'Active';

    IF ROW_COUNT() = 0 THEN
        SELECT JSON_OBJECT('success', FALSE, 'error', 'Building not found or already inactive') AS result;
    ELSE
        SELECT JSON_OBJECT('success', TRUE) AS result;
    END IF;
END$$
DELIMITER ;
```

---

### 5.4 `building_list`

Returns a paginated list of buildings with optional filters.

**Parameters**

| Parameter | Type | Default | Description |
|---|---|---|---|
| `p_body_id` | `INT` | `NULL` | Filter by body ID (NULL = all) |
| `p_status` | `VARCHAR(10)` | `NULL` | `'Active'` / `'Inactive'` / NULL = all |
| `p_page` | `INT` | `1` | Page number |
| `p_limit` | `INT` | `20` | Records per page |

**Returns**

```json
{
  "success": true,
  "page": 1,
  "limit": 20,
  "data": [
    {
      "building_id": 1,
      "name": "Mokarram Hossain Khandakar Bhaban Building",
      "body_id": 25,
      "location": "Mokarram Hossain Khandakar Bhaban Area",
      "status": "Active",
      "created_at": "2025-09-22 09:05:24"
    }
  ]
}
```

```sql
-- All active buildings
CALL building_list(NULL, 'Active', 1, 20);

-- Buildings for body_id = 25
CALL building_list(25, 'Active', 1, 10);
```

```sql
DROP PROCEDURE IF EXISTS building_list;
DELIMITER $$
CREATE PROCEDURE building_list(
    IN p_body_id INT,
    IN p_status  VARCHAR(10),
    IN p_page    INT,
    IN p_limit   INT
)
BEGIN
    DECLARE v_offset INT DEFAULT 0;
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    SET p_page   = IFNULL(p_page, 1);
    SET p_limit  = IFNULL(p_limit, 20);
    SET v_offset = (p_page - 1) * p_limit;

    SELECT JSON_OBJECT(
        'success', TRUE,
        'page',    p_page,
        'limit',   p_limit,
        'data', (
            SELECT IFNULL(JSON_ARRAYAGG(
                JSON_OBJECT(
                    'building_id', b.id,
                    'name',        b.name,
                    'body_id',     b.body_id,
                    'location',    b.location,
                    'status',      b.status,
                    'created_at',  b.created_at
                )
            ), JSON_ARRAY())
            FROM (
                SELECT id, name, body_id, location, status, created_at
                FROM buildings
                WHERE (p_body_id IS NULL OR body_id = p_body_id)
                  AND (p_status  IS NULL OR status  = p_status)
                ORDER BY id DESC
                LIMIT p_limit OFFSET v_offset
            ) b
        )
    ) AS result;
END$$
DELIMITER ;
```

---

### 5.5 `building_get_single`

Returns full details of one building by ID.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_building_id` | `INT` | Building ID |

**Returns**

```json
{
  "success": true,
  "data": {
    "building_id": 1,
    "name": "Mokarram Hossain Khandakar Bhaban Building",
    "body_id": 25,
    "location": "Mokarram Hossain Khandakar Bhaban Area",
    "status": "Active",
    "created_at": "2025-09-22 09:05:24",
    "updated_at": "2025-09-22 09:17:45"
  }
}
```

```sql
CALL building_get_single(1);
```

```sql
DROP PROCEDURE IF EXISTS building_get_single;
DELIMITER $$
CREATE PROCEDURE building_get_single(
    IN p_building_id INT
)
BEGIN
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    SELECT JSON_OBJECT(
        'success', TRUE,
        'data', JSON_OBJECT(
            'building_id', id,
            'name',        name,
            'body_id',     body_id,
            'location',    location,
            'status',      status,
            'created_at',  created_at,
            'updated_at',  updated_at
        )
    ) AS result
    FROM buildings
    WHERE id = p_building_id
    LIMIT 1;
END$$
DELIMITER ;
```

---

### 5.6 `body_building_mapping_save`

Saves multiple building mappings for a body in one call. **Replaces** all existing active mappings for that body and inserts the new list.

> This is the main endpoint called when a user submits the building assignment form for a department.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_body_id` | `INT` | Body ID to map buildings to |
| `p_entries` | `JSON` | Array of building mapping objects (see format below) |

**JSON Entry Format**

```json
[
  {
    "building_id":      1,
    "building_remarks": "3rd Floor, Room 301",
    "lat":              23.7280700,
    "long":             90.3990200
  },
  {
    "building_id":      3,
    "building_remarks": "Ground Floor",
    "lat":              23.7281000,
    "long":             90.3991000
  }
]
```

**Returns**

```json
{ "success": true, "mapped_count": 2 }
{ "success": false, "error": "message" }
```

**Logic**
1. Starts a transaction.
2. Soft-deletes all existing active mappings for `p_body_id` (sets `is_active = 0`).
3. Loops through each entry in the JSON array.
4. Inserts each entry using `INSERT ... ON DUPLICATE KEY UPDATE` to handle re-mapping the same building.
5. Commits on success, rolls back on any error.

```sql
CALL body_building_mapping_save(
    2,
    '[
        {"building_id": 1, "building_remarks": "3rd Floor, Room 301", "lat": 23.7280700, "long": 90.3990200},
        {"building_id": 3, "building_remarks": "Ground Floor", "lat": 23.7281000, "long": 90.3991000}
    ]'
);
```

```sql
DROP PROCEDURE IF EXISTS body_building_mapping_save;
DELIMITER $$
CREATE PROCEDURE body_building_mapping_save(
    IN p_body_id INT,
    IN p_entries JSON
)
BEGIN
    DECLARE v_i       INT DEFAULT 0;
    DECLARE v_count   INT DEFAULT 0;
    DECLARE v_entry   JSON;
    DECLARE v_bid     INT;
    DECLARE v_remarks VARCHAR(500);
    DECLARE v_lat     DECIMAL(10,7);
    DECLARE v_long    DECIMAL(10,7);

    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        ROLLBACK;
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    START TRANSACTION;

    -- Soft-delete existing mappings for this body
    UPDATE body_building_mapping
    SET is_active  = 0,
        updated_at = CURRENT_TIMESTAMP
    WHERE body_id  = p_body_id
      AND is_active = 1;

    SET v_count = JSON_LENGTH(p_entries);

    WHILE v_i < v_count DO
        SET v_entry   = JSON_EXTRACT(p_entries, CONCAT('$[', v_i, ']'));
        SET v_bid     = JSON_UNQUOTE(JSON_EXTRACT(v_entry, '$.building_id'));
        SET v_remarks = JSON_UNQUOTE(JSON_EXTRACT(v_entry, '$.building_remarks'));
        SET v_lat     = JSON_EXTRACT(v_entry, '$.lat');
        SET v_long    = JSON_EXTRACT(v_entry, '$.long');

        INSERT INTO body_building_mapping
            (body_id, building_id, building_remarks, lat, `long`, is_active)
        VALUES
            (p_body_id, v_bid, v_remarks, v_lat, v_long, 1)
        ON DUPLICATE KEY UPDATE
            building_remarks = v_remarks,
            lat              = v_lat,
            `long`           = v_long,
            is_active        = 1,
            updated_at       = CURRENT_TIMESTAMP;

        SET v_i = v_i + 1;
    END WHILE;

    COMMIT;

    SELECT JSON_OBJECT('success', TRUE, 'mapped_count', v_count) AS result;
END$$
DELIMITER ;
```

---

### 5.7 `body_building_mapping_get`

Returns all active building mappings for a body, joined with building name and location.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_body_id` | `INT` | Body ID |

**Returns**

```json
{
  "success": true,
  "body_id": 2,
  "data": [
    {
      "mapping_id":        1,
      "building_id":       1,
      "building_name":     "Mokarram Hossain Khandakar Bhaban Building",
      "building_location": "Mokarram Hossain Khandakar Bhaban Area",
      "building_remarks":  "3rd Floor, Room 301",
      "lat":               23.7280700,
      "long":              90.3990200,
      "is_active":         1,
      "created_at":        "2025-09-22 09:05:24"
    }
  ]
}
```

```sql
CALL body_building_mapping_get(2);
```

```sql
DROP PROCEDURE IF EXISTS body_building_mapping_get;
DELIMITER $$
CREATE PROCEDURE body_building_mapping_get(
    IN p_body_id INT
)
BEGIN
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    SELECT JSON_OBJECT(
        'success', TRUE,
        'body_id', p_body_id,
        'data', IFNULL((
            SELECT JSON_ARRAYAGG(
                JSON_OBJECT(
                    'mapping_id',        m.id,
                    'building_id',       b.id,
                    'building_name',     b.name,
                    'building_location', b.location,
                    'building_remarks',  m.building_remarks,
                    'lat',               m.lat,
                    'long',              m.`long`,
                    'is_active',         m.is_active,
                    'created_at',        m.created_at
                )
            )
            FROM body_building_mapping m
            JOIN buildings b ON b.id = m.building_id
            WHERE m.body_id   = p_body_id
              AND m.is_active = 1
              AND b.status    = 'Active'
        ), JSON_ARRAY())
    ) AS result;
END$$
DELIMITER ;
```

---

### 5.8 `body_building_mapping_delete`

Soft-deletes a single mapping entry by its mapping ID.

**Parameters**

| Parameter | Type | Description |
|---|---|---|
| `p_mapping_id` | `INT` | Mapping row ID to delete |

**Returns**

```json
{ "success": true }
{ "success": false, "error": "Mapping not found or already inactive" }
```

```sql
CALL body_building_mapping_delete(3);
```

```sql
DROP PROCEDURE IF EXISTS body_building_mapping_delete;
DELIMITER $$
CREATE PROCEDURE body_building_mapping_delete(
    IN p_mapping_id INT
)
BEGIN
    DECLARE EXIT HANDLER FOR SQLEXCEPTION
    BEGIN
        GET DIAGNOSTICS CONDITION 1 @msg = MESSAGE_TEXT;
        SELECT JSON_OBJECT('success', FALSE, 'error', @msg) AS result;
    END;

    UPDATE body_building_mapping
    SET is_active  = 0,
        updated_at = CURRENT_TIMESTAMP
    WHERE id       = p_mapping_id
      AND is_active = 1;

    IF ROW_COUNT() = 0 THEN
        SELECT JSON_OBJECT('success', FALSE, 'error', 'Mapping not found or already inactive') AS result;
    ELSE
        SELECT JSON_OBJECT('success', TRUE) AS result;
    END IF;
END$$
DELIMITER ;
```

---

## 6. Usage Examples

### Assign buildings to Department of Accounting (body_id = 2)

```sql
CALL body_building_mapping_save(
    2,
    '[
        {
            "building_id":      1,
            "building_remarks": "Faculty of Business Studies, 4th Floor",
            "lat":              23.7280700,
            "long":             90.3990200
        },
        {
            "building_id":      2,
            "building_remarks": "Exam Hall, Ground Floor",
            "lat":              23.7275000,
            "long":             90.3985000
        }
    ]'
);
```

### Retrieve all buildings for a body

```sql
CALL body_building_mapping_get(2);
```

### Add a new building then map it

```sql
-- Step 1: create building
CALL building_create('New MBA Building', NULL, 'Business Studies Area, Floor 1-3');

-- Step 2: use returned building_id in mapping save
CALL body_building_mapping_save(
    9,
    '[{"building_id": 21, "building_remarks": "Ground Floor, Room 101", "lat": 23.7282, "long": 90.3992}]'
);
```

### Remove one building from a body without affecting others

```sql
-- Use mapping_id from body_building_mapping_get result
CALL body_building_mapping_delete(5);
```

### List all active buildings (paginated)

```sql
CALL building_list(NULL, 'Active', 1, 20);
```

---

## Procedure Summary

| Procedure | Operation | Target |
|---|---|---|
| `building_create` | INSERT | `buildings` |
| `building_update` | UPDATE | `buildings` |
| `building_delete` | Soft DELETE | `buildings` |
| `building_list` | SELECT (paginated) | `buildings` |
| `building_get_single` | SELECT (single) | `buildings` |
| `body_building_mapping_save` | REPLACE (multi-entry) | `body_building_mapping` |
| `body_building_mapping_get` | SELECT (with JOIN) | `body_building_mapping` + `buildings` |
| `body_building_mapping_delete` | Soft DELETE (single) | `body_building_mapping` |

---

*End of Document*
