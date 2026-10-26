-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Integrity checks (docs/plans/storage-integrity.md, backlog E3-06): stored
-- objects read back and compared with the digest and length recorded, for
-- one artifact or for the whole library. A check resumes after the last
-- digest it finished (objects are read in digest order), keeps a bounded
-- list of what it found, and counts every problem.
--
-- An artifact's integrity_state is what the last check covering all its
-- objects found. Nothing is ever repaired: originals are not rewritten.
-- Additive.

CREATE TABLE integrity_checks (
    id               UUID PRIMARY KEY,
    scope            TEXT NOT NULL CHECK (scope IN ('artifact', 'all')),
    artifact_id      UUID REFERENCES artifacts (id) ON DELETE RESTRICT,
    state            TEXT NOT NULL DEFAULT 'queued'
                     CHECK (state IN ('queued', 'running', 'complete', 'failed', 'canceled')),
    -- Known once the runner has counted what it will read.
    objects_total    BIGINT CHECK (objects_total IS NULL OR objects_total >= 0),
    bytes_total      BIGINT CHECK (bytes_total IS NULL OR bytes_total >= 0),
    objects_checked  BIGINT NOT NULL DEFAULT 0 CHECK (objects_checked >= 0),
    bytes_checked    BIGINT NOT NULL DEFAULT 0 CHECK (bytes_checked >= 0),
    -- The last digest finished, so a check taken over resumes after it.
    resume_after     TEXT CHECK (resume_after IS NULL OR resume_after ~ '^[0-9a-f]{64}$'),
    -- The first problems found, bounded; problem_count counts them all.
    problems         JSONB NOT NULL DEFAULT '[]'::jsonb,
    problem_count    INTEGER NOT NULL DEFAULT 0 CHECK (problem_count >= 0),
    error_code       TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail     TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 10000),
    lease_owner      TEXT CHECK (lease_owner IS NULL OR length(lease_owner) <= 200),
    lease_expires_at TIMESTAMPTZ,
    created_by       TEXT NOT NULL CHECK (length(created_by) BETWEEN 1 AND 200),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at       TIMESTAMPTZ,
    completed_at     TIMESTAMPTZ,

    -- A check of one artifact names it; a check of everything names none.
    CONSTRAINT integrity_checks_scope_names_artifact CHECK (
        (scope = 'artifact') = (artifact_id IS NOT NULL)
    ),
    CONSTRAINT integrity_checks_lease_whole CHECK (
        (lease_owner IS NULL) = (lease_expires_at IS NULL)
    )
);

-- One full scrub at a time, and one check of an artifact at a time: asking
-- again answers the one under way.
CREATE UNIQUE INDEX integrity_checks_one_active_scrub ON integrity_checks (scope)
    WHERE scope = 'all' AND state IN ('queued', 'running');
CREATE UNIQUE INDEX integrity_checks_one_active_per_artifact ON integrity_checks (artifact_id)
    WHERE state IN ('queued', 'running');
CREATE INDEX integrity_checks_created_idx ON integrity_checks (created_at DESC, id DESC);

CREATE TRIGGER integrity_checks_touch BEFORE UPDATE ON integrity_checks
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

ALTER TABLE artifacts
    ADD COLUMN integrity_state TEXT NOT NULL DEFAULT 'unchecked'
        CHECK (integrity_state IN ('unchecked', 'intact', 'damaged')),
    ADD COLUMN integrity_checked_at TIMESTAMPTZ;

CREATE INDEX artifacts_damaged_idx ON artifacts (id) WHERE integrity_state = 'damaged';
