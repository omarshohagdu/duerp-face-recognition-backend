-- =====================================================================
-- NFC card <-> student mapping
--
-- Backs `src/routes/nfc_card.rs`:
--   GET|POST /ext-api/nfc-card/get_card_info?card_number=...  (or in the body)
--   POST /ext-api/nfc-card/save_card_info   (multipart/form-data)
--
-- Requirement: docs/nfc-card-reader-api-spec.md
-- API reference: docs/nfc_card.md
--
-- WHICH FILE TO APPLY
--   `docs/attendance_schema.sql` is the source of truth for the `attendance`
--   schema (see docs/DEPLOYMENT.md) and already contains everything below, as
--   its section 5. A fresh deployment applies that file and does NOT need this
--   one.
--
--   This file exists for an EXISTING deployment that wants only the new
--   objects, without re-running the whole schema file:
--
--     psql "$DATABASE_URL" -f sql/004_nfc_card.sql
--
--   Unlike sql/001-003 — the superseded `ictcell` originals — this one targets
--   the current `attendance` schema, so it is safe to apply as-is. Keep the two
--   copies in step: a change here belongs in section 5 of the schema file too.
--
-- Idempotent: every statement is CREATE ... IF NOT EXISTS / CREATE OR
-- REPLACE, so re-running it changes nothing.
--
-- The allow-list seed at the end is deliberately duplicated from
-- sql/000_ext_api_infra.sql, so this file stands alone. Both are guarded by
-- NOT EXISTS, so applying either or both is the same.
--
-- WHY `student_applicant_id` HAS NO FOREIGN KEY
--   The spec describes a `student_table` the applicant id points at. No such
--   table exists in this database — `ictcell.lms_student` carries id / name /
--   reg_no / roll_no and no applicant identifier, and applicants are not
--   students yet, so lms_student could not hold them anyway. This table is
--   therefore the registry: the applicant id is an opaque external key and the
--   first save for an id creates its row. The consequence is deliberate and
--   worth knowing: a typo'd applicant id produces a NEW card record rather
--   than a 404, so nothing here can catch it. Add the FK (and the spec's 404
--   "Student not found") the day an applicant registry lands.
-- =====================================================================

CREATE SCHEMA IF NOT EXISTS attendance;

-- ---------------------------------------------------------------------
-- 1 · Normalisation
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
-- 1b · Human-readable labels for the two 1/2 enums
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
-- 2 · The mapping table
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
-- 3 · Audit trail
--
-- Business-logic step 6 of the spec ("log who saved it, when, which fields
-- changed"). The step logs and `ictcell.ext_api_call_logs` already record the
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
-- 4 · Function: scan lookup
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
-- 5 · Function: save / update a card record
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
-- 6 · Seed the ext-api IP allow-list
--
-- ExtAuthMiddleware runs before every /ext-api handler and matches the FULL
-- request path against `ictcell.ext_api_allowed_ips`. Without a row here every
-- call to these two endpoints is a 403 before the handler is ever reached.
--
-- Localhost only, matching sql/000_ext_api_infra.sql. Add the card reader's
-- real IP before going live:
--   UPDATE ictcell.ext_api_allowed_ips
--      SET ip_address = ip_address || '{203.0.113.10}'
--    WHERE endpoint = '/ext-api/nfc-card/get_card_info';
--
-- NOT `ON CONFLICT`: the production table has no unique constraint on
-- `endpoint`, so an ON CONFLICT target would abort the script there.
-- ---------------------------------------------------------------------

INSERT INTO ictcell.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{127.0.0.1,::1}'::text[]
  FROM (VALUES
    ('/ext-api/nfc-card/get_card_info'),
    ('/ext-api/nfc-card/save_card_info')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM ictcell.ext_api_allowed_ips a
     WHERE a.endpoint = v.endpoint
 );
