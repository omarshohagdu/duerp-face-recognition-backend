# wow_attendance_location_verification

**Endpoint:** `POST /ext-api/wow-attendance/verify`
**Procedure:** `wow_attendance_location_verification`
**Schema:** `ictcell` | **Engine:** MySQL / MariaDB

---

## Table of Contents

1. [Overview](#1-overview)
2. [Verify Flow](#2-verify-flow)
3. [Request](#3-request)
4. [Response](#4-response)
5. [Stored Procedure](#5-stored-procedure)
6. [Rust Handler](#6-rust-handler)
7. [Radius Logic](#7-radius-logic)
8. [Haversine Formula](#8-haversine-formula)

---

## 1. Overview

When `POST /ext-api/wow-attendance/verify` is hit:

1. `emp_id` + device `lat` / `long` request থেকে নেয়।
2. `employees.emp_id` → `employees.office` (body_id) বের করে।
3. `body_building_mapping` থেকে সেই `body_id` এর সব active building এর `lat`, `long`, `radius` fetch করে।
4. প্রতিটা building এর সাথে **Haversine distance** calculate করে।
5. **সবচেয়ে কাছের** building বের করে।
6. `distance <= radius` → **verified true / false**।

> `radius` প্রতিটা building mapping এ আলাদাভাবে store হয়। Admin `body_building_mapping_save` এ per-building radius set করতে পারবে।

---

## 2. Verify Flow

```
POST /ext-api/wow-attendance/verify
{ emp_id, device_lat, device_long }
              │
              ▼
  ictcell.employees
  WHERE emp_id = ?
  → office  = body_id
  → name_en = emp_name
              │
              ▼
  body_building_mapping
  WHERE body_id = office
    AND is_active = 1
  JOIN buildings WHERE status = 'Active'
              │
    ┌─────────┼──────────┐
    │         │          │
 Building A  B          C
 radius=50m  radius=30m  radius=100m
    │         │          │
    └────Haversine distance────┘
         from device GPS
    │         │          │
   18m       210m       560m
    │
 Closest = Building A (18m)
    │
 18m ≤ 50m ?
    │
  ┌─┴──────────────┐
  YES               NO
  verified=true     verified=false
  HTTP 200          HTTP 403
```

---

## 3. Request

**Content-Type:** `application/json`

| Field | Type | Required | Description |
|---|---|---|---|
| `emp_id` | `String` | ✅ | `employees.emp_id` (e.g. `"E-1234"`) |
| `device_lat` | `f64` | ✅ | Device GPS latitude |
| `device_long` | `f64` | ✅ | Device GPS longitude |

```json
{
  "emp_id":      "E-1166",
  "device_lat":  23.7281500,
  "device_long": 90.3992500
}
```

---

## 4. Response

### ✅ `200 OK` — Verified

```json
{
  "verified":      true,
  "emp_id":        "E-1166",
  "emp_name":      "Dr. Anis Ahmed",
  "body_id":       5,
  "building_id":   5,
  "building_name": "Qazi Motahar Husain Bhaban",
  "distance_m":    18.4,
  "radius_m":      50.0
}
```

### ❌ `403 Forbidden` — Outside all building radii

```json
{
  "verified":   false,
  "emp_id":     "E-1166",
  "body_id":    5,
  "distance_m": 312.7,
  "radius_m":   50.0,
  "error":      "Device location does not match any mapped building"
}
```

### ❌ `403 Forbidden` — No building mapped

```json
{
  "verified": false,
  "error":    "No building mapping found for this employee office"
}
```

### ❌ `403 Forbidden` — Employee not found

```json
{
  "verified": false,
  "error":    "Employee not found: E-9999"
}
```

### ❌ `500` — DB error

```json
{
  "verified": false,
  "error":    "Internal server error"
}
```

---

## 5. Stored Procedure

```sql
DROP PROCEDURE IF EXISTS wow_attendance_location_verification;
DELIMITER $$
CREATE PROCEDURE wow_attendance_location_verification(
    IN p_emp_id      VARCHAR(50),
    IN p_device_lat  DECIMAL(10,7),
    IN p_device_long DECIMAL(10,7)
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

    -- Step 1: employees.emp_id → office (body_id)
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
        -- Step 2: count active mappings
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
            -- Step 3 & 4: Haversine — closest building
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
            INTO v_building_id, v_building_name, v_radius, v_distance
            FROM body_building_mapping m
            JOIN buildings b ON b.id = m.building_id
            WHERE m.body_id   = v_body_id
              AND m.is_active = 1
              AND b.status    = 'Active'
              AND m.lat       IS NOT NULL
              AND m.`long`    IS NOT NULL
            ORDER BY distance_m ASC
            LIMIT 1;

            -- Step 5: distance vs radius
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
```

---

## 6. Rust Handler

```rust
use actix_web::{web, HttpResponse};
use serde::Deserialize;
use serde_json::Value;
use sqlx::MySqlPool;

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub emp_id:      String,
    pub device_lat:  f64,
    pub device_long: f64,
}

pub async fn wow_attendance_verify(
    pool: web::Data<MySqlPool>,
    body: web::Json<VerifyRequest>,
) -> HttpResponse {

    let result = sqlx::query_scalar::<_, Value>(
        "CALL wow_attendance_location_verification(?, ?, ?)"
    )
    .bind(&body.emp_id)
    .bind(body.device_lat)
    .bind(body.device_long)
    .fetch_one(pool.get_ref())
    .await;

    match result {
        Ok(val) => {
            let verified = val
                .get("verified")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if verified {
                HttpResponse::Ok().json(val)
            } else {
                HttpResponse::Forbidden().json(val)
            }
        }
        Err(e) => {
            eprintln!("[wow_attendance_verify] DB error: {e}");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "verified": false,
                "error":    "Internal server error"
            }))
        }
    }
}
```

**Route registration:**

```rust
cfg.service(
    web::resource("/ext-api/wow-attendance/verify")
        .route(web::post().to(wow_attendance_verify))
);
```

---

## 7. Radius Logic

`radius` প্রতিটা building mapping এ আলাদা — hardcoded না।

```
body_building_mapping:

building_id | radius
     1      | 50m   ← Admin set করেছে
     2      | 30m   ← Admin set করেছে
     3      | 100m  ← Admin set করেছে
```

| Situation | Recommended Radius |
|---|---|
| Normal department building | `50m` (default) |
| Small room / lab | `30m` |
| Large campus area | `100m` |
| Indoor only | `20m` ⚠️ GPS drift এ fail করতে পারে |

> Device GPS নিজেই **3–50m error** দেয় (hardware limitation)। তাই **20m এর নিচে** না যাওয়াই ভালো।

---

## 8. Haversine Formula

```
distance (m) = 2 × 6371000 × arcsin(√(
    sin²(Δlat/2) + cos(lat₁) × cos(lat₂) × sin²(Δlong/2)
))
```

```sql
ROUND(
    2 * 6371000 * ASIN(SQRT(
        POWER(SIN((building_lat - device_lat) * PI() / 360), 2) +
        COS(device_lat   * PI() / 180) *
        COS(building_lat * PI() / 180) *
        POWER(SIN((building_long - device_long) * PI() / 360), 2)
    )),
2)
```

| Variable | Description |
|---|---|
| `6371000` | Earth radius (metres) |
| `building_lat/long` | `body_building_mapping` এর GPS |
| `device_lat/long` | Request body এর GPS |
| Result | Distance in metres, 2 decimal places |

> Accuracy: 1km এর মধ্যে ~0.3% error — campus attendance এর জন্য যথেষ্ট।

---

*End of Document*
