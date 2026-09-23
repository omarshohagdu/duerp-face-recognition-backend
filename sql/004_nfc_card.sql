-- =====================================================================
-- NFC card <-> student mapping
--
-- Backs `src/routes/nfc_card.rs`:
--   GET|POST /ext-api/nfc-card/get_card_info?card_number=...  (or in the body)
--   POST /ext-api/nfc-card/save_card_info   (multipart/form-data)
--   GET|POST /ext-api/nfc-card/checking_card_reg_status?registration_no=...
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
                                 -- Written by the service, never by a caller:
                                 -- the save happened while the face gate was
                                 -- OFF, so the two photographs were never
                                 -- compared. Distinct from 2 on purpose — that
                                 -- one means "checked, awaiting review".
                                 WHEN 0 THEN 'Not Face-Checked'
                             END,
        'registration_type_label', CASE p_registration_type
                                       WHEN 1 THEN 'Admin'
                                       WHEN 2 THEN 'Self'
                                   END
    )
$function$;

COMMENT ON FUNCTION attendance.nfc_card_labels(smallint, smallint) IS
    'is_verified 0=Not Face-Checked 1=Verified 2=Not Verified; registration_type 1=Admin 2=Self.';

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
    -- 1 = Verified, 2 = Not Verified (checked, awaiting review),
    -- 0 = Not Face-Checked — the save was made while the face gate was OFF.
    -- 0 is set by the service, never accepted from a caller: it is a statement
    -- about what this service did, not a claim the desk gets to make.
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
        CHECK (is_verified IN (0, 1, 2)),
    CONSTRAINT nfc_student_cards_registration_type_ck
        CHECK (registration_type IN (1, 2))
);

COMMENT ON TABLE attendance.nfc_student_cards IS
    'NFC card <-> student mapping. student_applicant_id is an opaque external key: no applicant registry exists to reference.';

-- ---------------------------------------------------------------------
-- 2b · Widen `is_verified` on a table that already exists
--
-- `CREATE TABLE IF NOT EXISTS` above skips everything — including the CHECK —
-- on a database that already has this table, so the third state has to be
-- added explicitly. Without this, a save made while the face gate is OFF fails
-- with a constraint violation instead of storing 0.
--
-- Safe to re-run, and safe on rows that already exist: 0 only widens what is
-- allowed, so nothing stored can suddenly violate it.
-- ---------------------------------------------------------------------

ALTER TABLE attendance.nfc_student_cards
    DROP CONSTRAINT IF EXISTS nfc_student_cards_is_verified_ck;

ALTER TABLE attendance.nfc_student_cards
    ADD CONSTRAINT nfc_student_cards_is_verified_ck CHECK (is_verified IN (0, 1, 2));

-- ---------------------------------------------------------------------
-- 3 · Audit trail
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
-- 4b · Function: registration-status check
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
-- TWO RULES FORCE `is_verified`, whatever the caller sent, because both are
-- statements about what actually happened rather than what the desk claims:
--
--   * `p_face_checked = false` FORCES 0 ("Not Face-Checked"). The save was
--     made while the face gate was OFF — see NFC_FACE_VERIFY in
--     sql/008_system_settings.sql — so the card photo and the selfie were
--     never compared. Recording 1 there would be a lie, and recording 2 would
--     be indistinguishable from a save that WAS checked and is merely awaiting
--     review. It outranks the self-registration rule below, which it also
--     satisfies: 0 is not "verified" either.
--
--   * registration_type = 2 (Self) FORCES 2, so a self-registration can never
--     mark itself trusted.
--
-- The handler supplies `p_face_checked` because only it knows: the comparison
-- is an HTTP call Postgres cannot make, and the toggle has an environment
-- fallback SQL cannot read (src/utils/settings.rs).
-- ---------------------------------------------------------------------

-- Adding a parameter is not something CREATE OR REPLACE can do — it would
-- leave the 9-argument version in place as an overload, and a caller that
-- omitted the new flag would silently get the old behaviour. Dropping first is
-- what makes the new rule unavoidable.
DROP FUNCTION IF EXISTS attendance.nfc_card_save_info(
    varchar, varchar, smallint, smallint, text, text, boolean, bigint, varchar);

CREATE OR REPLACE FUNCTION attendance.nfc_card_save_info(
    p_student_applicant_id varchar,
    p_card_number          varchar,
    p_is_verified          smallint,
    p_registration_type    smallint,
    p_card_image           text,
    p_student_selfie       text,
    p_force_reassign       boolean,
    p_performed_by         bigint,
    p_client_ip            varchar,
    -- Did the face gate actually compare the two photographs for this save?
    -- Defaults true so a direct SQL caller keeps the old meaning; the handler
    -- always passes it explicitly.
    p_face_checked         boolean DEFAULT true
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

    -- 0 is deliberately NOT accepted here. It means "this service did not
    -- compare the photographs", which is ours to record and not a desk's to
    -- claim — and a caller who reads a 0 back and sends it again is telling us
    -- something they cannot know. The gate below sets it.
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

    -- ...and nothing can be called verified, or even "checked and awaiting
    -- review", when no comparison happened. Applied LAST so it outranks both
    -- the caller's value and the self-registration rule.
    IF NOT coalesce(p_face_checked, true) THEN
        v_verified := 0;
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

    -- A card is registered once. Re-saving the card a student already holds is
    -- refused rather than treated as an update: the desk is told to use
    -- another card, and the stored row, images and verification stay as they
    -- are. `force_reassign` does not override this — there is nobody to take
    -- the card from.
    IF v_holder.id IS NOT NULL AND v_holder.student_applicant_id = v_applicant THEN
        RETURN jsonb_build_object(
            'status',  'error',
            'code',    'card_exists',
            'message', 'This card already exists. Please try another card.');
    END IF;

    IF v_holder.id IS NOT NULL AND v_holder.student_applicant_id <> v_applicant THEN
        IF NOT v_force THEN
            RETURN jsonb_build_object(
                'status',  'error',
                'code',    'card_conflict',
                'message', 'This card already exists. Please try another card.',
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
                   AND coalesce(p_face_checked, true)
                UNION ALL
                SELECT 'is_verified recorded as 0 (Not Face-Checked) — face verification '
                    || 'was OFF, so the card photo and the selfie were not compared'
                 WHERE NOT coalesce(p_face_checked, true)
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
            'message', 'This card already exists. Please try another card.');
END;
$function$;

-- ---------------------------------------------------------------------
-- 6 · Seed the ext-api IP allow-list — OPEN TO ALL IPs
--
-- ExtAuthMiddleware runs before every /ext-api handler and matches the FULL
-- request path against `attendance.ext_api_allowed_ips`. Without a row here every
-- call to these three endpoints is a 403 before the handler is ever reached.
--
-- All three carry `'*'`, the wildcard the middleware reads as "any IP"
-- (`ext_auth_middleware.rs`). The card readers are on campus DHCP with no
-- stable addresses to list, so the allow-list cannot scope them and was asked
-- to stop trying.
--
-- WHAT IS LEFT GUARDING THEM: the app credentials (`X-App-Id` /
-- `X-App-Password`) and a valid bearer token — nothing else. `save_card_info`
-- with `force_reassign=true` moves a card off another student, so any holder of
-- those three, from anywhere that can reach the service, can do that. Whether
-- that is acceptable depends entirely on the network the service is exposed on.
--
-- TO PUT THE ALLOW-LIST BACK, list the real IPs and drop the wildcard:
--   UPDATE attendance.ext_api_allowed_ips
--      SET ip_address = '{203.0.113.10,203.0.113.11}'::text[]
--    WHERE endpoint IN ('/ext-api/nfc-card/get_card_info',
--                       '/ext-api/nfc-card/save_card_info',
--                       '/ext-api/nfc-card/checking_card_reg_status');
--
-- NOT `ON CONFLICT`: the production table has no unique constraint on
-- `endpoint`, so an ON CONFLICT target would abort the script there.
-- ---------------------------------------------------------------------

INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES
    ('/ext-api/nfc-card/get_card_info'),
    ('/ext-api/nfc-card/save_card_info'),
    ('/ext-api/nfc-card/checking_card_reg_status')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_allowed_ips a
     WHERE a.endpoint = v.endpoint
 );

-- The INSERT above skips endpoints that already have a row, which on any
-- database this has run against before is the two older paths. Open those too, and
-- re-activate any that were switched off — idempotent, so re-running this file
-- is safe.
UPDATE attendance.ext_api_allowed_ips
   SET ip_address = ip_address || '{*}'::text[],
       is_active  = true
 WHERE endpoint IN ('/ext-api/nfc-card/get_card_info',
                    '/ext-api/nfc-card/save_card_info',
                    '/ext-api/nfc-card/checking_card_reg_status')
   AND NOT ('*' = ANY(ip_address));
