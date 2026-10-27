-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Removing a source once the library has it (docs/plans/source-retention.md,
-- ADR 0017): a torrent and its files, a Usenet download, an S3 object
-- version. Off unless an administrator set the integration's after_import
-- to remove, and only for imports that complete while it is.
--
-- One row per import, written as the import completes. It waits in
-- verifying until an integrity check of the artifact reads the library's
-- copy back whole, then in removing until the source is removed or found
-- gone. A check that finds a problem ends it as refused, and nothing is
-- removed. kept is a source left where it is, with why: the integration
-- turned removal off, or the source is no longer what was imported.
--
-- integration_jobs.content_path is the path beneath the client's download
-- folder the import was copied from, so the source removed is the one
-- imported, not wherever the client says the job is now.
--
-- Additive: the previous version never reads either.

CREATE TABLE source_removals (
    id              UUID PRIMARY KEY,
    import_job_id   UUID NOT NULL UNIQUE REFERENCES import_jobs (id) ON DELETE RESTRICT,
    -- An integration deleted takes its pending removals with it: nothing can
    -- remove at a source Tangible no longer reaches. The audit log keeps what
    -- was removed.
    integration_id  UUID NOT NULL REFERENCES integrations (id) ON DELETE CASCADE,
    artifact_id     UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    check_id        UUID REFERENCES integrity_checks (id) ON DELETE SET NULL,
    state           TEXT NOT NULL DEFAULT 'verifying'
                    CHECK (state IN ('verifying', 'removing', 'removed', 'already_gone',
                                     'kept', 'refused')),
    detail          TEXT CHECK (detail IS NULL OR length(detail) <= 2000),
    files_removed   INTEGER CHECK (files_removed IS NULL OR files_removed >= 0),
    bytes_removed   BIGINT CHECK (bytes_removed IS NULL OR bytes_removed >= 0),
    files_kept      INTEGER CHECK (files_kept IS NULL OR files_kept >= 0),
    -- Tries at the source that failed, for the backoff.
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TIMESTAMPTZ,
    leased_by       TEXT CHECK (leased_by IS NULL OR length(leased_by) BETWEEN 1 AND 200),
    leased_until    TIMESTAMPTZ,
    requested_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at        TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT source_removals_ended_when_done CHECK (
        (state IN ('verifying', 'removing')) = (ended_at IS NULL)),
    CONSTRAINT source_removals_lease_whole CHECK (
        (leased_by IS NULL) = (leased_until IS NULL)),
    CONSTRAINT source_removals_open_unleased_when_ended CHECK (
        ended_at IS NULL OR leased_by IS NULL)
);

-- What the remover looks for: open rows, by when they are due.
CREATE INDEX source_removals_open_idx ON source_removals (next_attempt_at NULLS FIRST, id)
    WHERE state IN ('verifying', 'removing');

CREATE TRIGGER source_removals_touch BEFORE UPDATE ON source_removals
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

ALTER TABLE integration_jobs
    ADD COLUMN content_path TEXT
        CHECK (content_path IS NULL OR length(content_path) BETWEEN 1 AND 4096);
