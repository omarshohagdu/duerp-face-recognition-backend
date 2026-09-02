-- ============================================================
-- Audit table: token / face-ownership mismatches
--
-- One row per rejected enroll/verify where the person the request acted on is
-- NOT the logged-in token holder:
--   * Verify: the AI recognized a face whose identifier != token user.
--   * Enroll: the id being enrolled != token user.
--
-- ai_requested_id and ai_similarity come from the AI platform's response and are
-- nullable: they are only present on flows that call the AI (verify recognition),
-- and only once the platform returns them. Enroll rejects before any AI call, so
-- both stay NULL for Enroll rows.
-- ============================================================

CREATE TABLE IF NOT EXISTS ictcell.wow_attendance_token_mismatch_record (
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
    ON ictcell.wow_attendance_token_mismatch_record (requested_user_id);
CREATE INDEX IF NOT EXISTS wow_attendance_token_mismatch_created_idx
    ON ictcell.wow_attendance_token_mismatch_record (created_at DESC);