-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Requests to erase the rewritable disc in one drive.
--
-- An erase destroys whatever is on the disc, so it is never a side effect of
-- a burn: a burn refuses a disc that is not blank, and this is how one
-- becomes blank, asked for by name, for one drive.

CREATE TABLE media_erasures (
    id                 UUID PRIMARY KEY,
    -- RESTRICT: the record of what was erased, where and by whom outlives the
    -- drive it happened in.
    drive_id           UUID NOT NULL REFERENCES drives (id) ON DELETE RESTRICT,
    mode               TEXT NOT NULL CHECK (mode IN ('quick', 'full')),
    state              TEXT NOT NULL DEFAULT 'queued'
                       CHECK (state IN (
                           'queued', 'erasing', 'erased', 'already_blank',
                           'refused', 'failed', 'canceled'
                       )),
    requested_by       TEXT NOT NULL CHECK (length(requested_by) BETWEEN 1 AND 200),
    -- What the worker found in the drive before deciding: profile, blank or
    -- not, sessions. Evidence for "what was on the disc that was erased".
    medium_before_json JSONB CHECK (medium_before_json IS NULL
                                    OR pg_column_size(medium_before_json) <= 16384),
    error_code         TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail       TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 10000),
    duration_seconds   INTEGER CHECK (duration_seconds IS NULL OR duration_seconds >= 0),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at         TIMESTAMPTZ,
    completed_at       TIMESTAMPTZ,

    CONSTRAINT media_erasures_started_when_taken CHECK (
        state IN ('queued', 'canceled') OR started_at IS NOT NULL
    ),
    CONSTRAINT media_erasures_completed_when_terminal CHECK (
        (state IN ('queued', 'erasing')) = (completed_at IS NULL)
    )
);

CREATE TRIGGER media_erasures_touch BEFORE UPDATE ON media_erasures
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- One open request per drive. A second one would erase a disc somebody put
-- in after the first had finished, which nobody asked for.
CREATE UNIQUE INDEX media_erasures_one_open_per_drive
    ON media_erasures (drive_id)
    WHERE state IN ('queued', 'erasing');

CREATE INDEX media_erasures_created_idx ON media_erasures (created_at DESC);
