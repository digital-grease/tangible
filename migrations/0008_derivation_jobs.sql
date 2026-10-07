-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Derivation jobs: a request to make a derivative (a CHD of a disc image),
-- queued and worked in the background like an import.
--
-- The finished result is recorded in `derivations`, which already exists and
-- is the lineage. A job is the work; a derivation is the evidence. A job that
-- failed leaves no derivation, and a derivation outlives the job that made it.

CREATE TABLE derivation_jobs (
    id                    UUID PRIMARY KEY,
    -- RESTRICT: a job names its parent for as long as the job exists.
    parent_artifact_id    UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    transformation        TEXT NOT NULL
                          CHECK (transformation IN ('chd_create_cd', 'chd_create_dvd')),
    -- The normalized options, every default written out, exactly as the
    -- fingerprint covers them.
    options_json          JSONB NOT NULL,
    tool_name             TEXT NOT NULL CHECK (length(tool_name) BETWEEN 1 AND 200),
    tool_version          TEXT NOT NULL CHECK (length(tool_version) BETWEEN 1 AND 200),
    command_fingerprint   TEXT NOT NULL CHECK (command_fingerprint ~ '^[0-9a-f]{64}$'),
    state                 TEXT NOT NULL DEFAULT 'queued'
                          CHECK (state IN (
                              'queued', 'running', 'complete',
                              'failed_retryable', 'failed_terminal', 'canceled'
                          )),
    child_artifact_id     UUID REFERENCES artifacts (id) ON DELETE RESTRICT,
    attempts              INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    error_code            TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail          TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 10000),
    lease_owner           TEXT CHECK (lease_owner IS NULL OR length(lease_owner) <= 200),
    lease_expires_at      TIMESTAMPTZ,
    created_by            TEXT NOT NULL CHECK (length(created_by) BETWEEN 1 AND 200),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at            TIMESTAMPTZ,
    completed_at          TIMESTAMPTZ,

    -- A finished job has its child; an unfinished one does not claim one.
    CONSTRAINT derivation_jobs_complete_has_child CHECK (
        (state = 'complete') = (child_artifact_id IS NOT NULL)
    ),
    -- A lease has an owner and an expiry, or neither.
    CONSTRAINT derivation_jobs_lease_whole CHECK (
        (lease_owner IS NULL) = (lease_expires_at IS NULL)
    ),
    CONSTRAINT derivation_jobs_not_own_parent CHECK (
        child_artifact_id IS NULL OR child_artifact_id <> parent_artifact_id
    )
);

-- One open job per parent and fingerprint: asking again while the first is
-- queued or running gets the first, not a second run of the same work.
CREATE UNIQUE INDEX derivation_jobs_open_unique
    ON derivation_jobs (parent_artifact_id, command_fingerprint)
    WHERE state IN ('queued', 'running', 'failed_retryable');

CREATE INDEX derivation_jobs_claimable_idx
    ON derivation_jobs (created_at)
    WHERE state IN ('queued', 'running', 'failed_retryable');

CREATE INDEX derivation_jobs_parent_idx ON derivation_jobs (parent_artifact_id);

CREATE TRIGGER derivation_jobs_touch BEFORE UPDATE ON derivation_jobs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
