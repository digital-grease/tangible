-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Ripping the disc in a drive (ADR 0013). A rip belongs to one drive, as an
-- erasure does: only that drive's worker takes it, and a drive never rips,
-- burns and erases at once (the claims lock the drive's row and check the
-- others). What the worker reads is uploaded into a staging area named by
-- staging_id, which becomes the id of the import job created when the rip
-- completes; until then there is no import row to point at.
--
-- Additive: nothing existing changes, so the previous release keeps working
-- against this schema.

CREATE TABLE rip_jobs (
    id                UUID PRIMARY KEY,
    drive_id          UUID NOT NULL REFERENCES drives (id) ON DELETE RESTRICT,
    state             TEXT NOT NULL DEFAULT 'queued'
                      CHECK (state IN (
                          'queued', 'reading', 'uploading', 'registering',
                          'complete', 'canceled', 'refused', 'failed'
                      )),
    requested_by      TEXT NOT NULL CHECK (length(requested_by) BETWEEN 1 AND 200),
    staging_id        UUID NOT NULL UNIQUE,
    import_job_id     UUID REFERENCES import_jobs (id) ON DELETE RESTRICT,
    medium_json       JSONB CHECK (medium_json IS NULL OR pg_column_size(medium_json) <= 16384),
    engine            TEXT CHECK (engine IS NULL OR length(engine) BETWEEN 1 AND 100),
    engine_version    TEXT CHECK (engine_version IS NULL OR length(engine_version) BETWEEN 1 AND 200),
    read_errors_json  JSONB CHECK (read_errors_json IS NULL
                                   OR pg_column_size(read_errors_json) <= 16384),
    -- What arrived: {"disc.bin": {"sha256": "...", "bytes": 123}}, as the
    -- server computed it.
    outputs_json      JSONB NOT NULL DEFAULT '{}'::jsonb
                      CHECK (pg_column_size(outputs_json) <= 16384),
    bytes_received    BIGINT NOT NULL DEFAULT 0 CHECK (bytes_received >= 0),
    error_code        TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail      TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 10000),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at        TIMESTAMPTZ,
    completed_at      TIMESTAMPTZ,

    CONSTRAINT rip_jobs_queued_not_started CHECK (
        state <> 'queued' OR started_at IS NULL
    ),
    CONSTRAINT rip_jobs_started_when_taken CHECK (
        state NOT IN ('reading', 'uploading', 'registering', 'complete')
        OR started_at IS NOT NULL
    ),
    CONSTRAINT rip_jobs_completed_when_terminal CHECK (
        (state IN ('queued', 'reading', 'uploading', 'registering')) = (completed_at IS NULL)
    ),
    -- The import is the staging area's: one id for both.
    CONSTRAINT rip_jobs_import_is_its_staging CHECK (
        import_job_id IS NULL OR import_job_id = staging_id
    ),
    CONSTRAINT rip_jobs_registered_has_import CHECK (
        state NOT IN ('registering', 'complete') OR import_job_id IS NOT NULL
    )
);

CREATE TRIGGER rip_jobs_touch BEFORE UPDATE ON rip_jobs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- One rip at a time per drive, until everything it read has been sent.
CREATE UNIQUE INDEX rip_jobs_one_open_per_drive
    ON rip_jobs (drive_id)
    WHERE state IN ('queued', 'reading', 'uploading');

CREATE INDEX rip_jobs_created_idx ON rip_jobs (created_at DESC, id DESC);

-- Registering rips are followed to their import's end.
CREATE INDEX rip_jobs_registering_idx ON rip_jobs (import_job_id)
    WHERE state = 'registering';

-- What the worker said while ripping. Append-only, and idempotent by
-- sequence, as burn events are.
CREATE TABLE rip_events (
    rip_id             UUID NOT NULL REFERENCES rip_jobs (id) ON DELETE RESTRICT,
    sequence           BIGINT NOT NULL CHECK (sequence >= 0),
    event_type         TEXT NOT NULL CHECK (length(event_type) BETWEEN 1 AND 100),
    message_code       TEXT NOT NULL CHECK (length(message_code) BETWEEN 1 AND 100),
    progress_fraction  REAL CHECK (progress_fraction IS NULL OR progress_fraction BETWEEN 0 AND 1),
    data_json          JSONB NOT NULL DEFAULT '{}'::jsonb
                       CHECK (pg_column_size(data_json) <= 16384),
    worker_timestamp   TIMESTAMPTZ NOT NULL,
    server_received_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (rip_id, sequence)
);
