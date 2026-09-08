-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Checks performed on a disc after it was burned.
--
-- `physical_copies.last_checked_at` records that a disc was looked at. This
-- records what was found, and every time: a disc that read cleanly a year ago
-- and fails today is the case the whole inventory exists to catch, and a
-- single timestamp cannot express it.
--
-- Append-only, like burn events. A check is an observation made at a moment,
-- and rewriting one would be rewriting what somebody saw.

CREATE TABLE physical_copy_checks (
    id                UUID PRIMARY KEY,
    -- RESTRICT, not CASCADE: the checks are the evidence for a disc's
    -- condition, and deleting the disc's record must not silently take them.
    physical_copy_id  UUID NOT NULL REFERENCES physical_copies (id) ON DELETE RESTRICT,
    -- What was done. The same vocabulary as a burn's verification, because
    -- checking a shelved disc and verifying a fresh one are the same act.
    method            TEXT NOT NULL
                      CHECK (method IN (
                          'none', 'tool_verify', 'filesystem_compare',
                          'full_sector_readback', 'track_hash_compare', 'provider_match'
                      )),
    -- What was found. `partial` exists because a disc can read for most of
    -- its surface and fail on the rest, which is worth distinguishing from
    -- both a clean read and a total failure.
    result            TEXT NOT NULL
                      CHECK (result IN ('not_performed', 'passed', 'failed', 'partial')),
    -- The digest read back, when the method produces one. Not required: a
    -- filesystem comparison does not yield a whole-disc digest.
    observed_sha256   TEXT CHECK (observed_sha256 IS NULL OR observed_sha256 ~ '^[0-9a-f]{64}$'),
    bytes_read        BIGINT CHECK (bytes_read IS NULL OR bytes_read >= 0),
    -- Which drive read it, when one was used. Text rather than a foreign key:
    -- a disc may be checked on a machine this server has never enrolled.
    checked_with      TEXT CHECK (checked_with IS NULL OR length(checked_with) <= 200),
    notes             TEXT CHECK (notes IS NULL OR length(notes) <= 10000),
    checked_by        TEXT NOT NULL,
    checked_at        TIMESTAMPTZ NOT NULL DEFAULT now()

    -- No updated_at: a check is immutable history.
);

CREATE INDEX physical_copy_checks_copy_idx
    ON physical_copy_checks (physical_copy_id, checked_at DESC);

-- Checks are immutable once written, enforced rather than merely documented.
-- A rewritten check would change the account of what somebody observed, and
-- the condition of a disc is decided from these rows.
CREATE FUNCTION refuse_check_update() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'physical copy checks are immutable; record another check instead'
        USING ERRCODE = 'restrict_violation';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER physical_copy_checks_immutable
    BEFORE UPDATE ON physical_copy_checks
    FOR EACH ROW EXECUTE FUNCTION refuse_check_update();
