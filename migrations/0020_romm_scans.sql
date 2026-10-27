-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Asking RomM to scan what the export changed (docs/plans/romm-api.md M2,
-- backlog E9-07).
--
-- When the exporter writes, renames or removes a game's folder, the
-- platform folder it is in is owed a scan, whether or not RomM's API is
-- configured: once it is, RomM is asked for one quick scan of every
-- platform owed one. A platform stays owed until a scan asked for after
-- the change finishes, so a scan RomM refuses or that fails is asked for
-- again.
--
-- Each scan asked for is a row in romm_scans, with RomM's task id and what
-- became of it. They are operational records, pruned past a bound, and go
-- with the integration that asked.
--
-- Additive: the previous version never reads or writes either table.

CREATE TABLE romm_owed_scans (
    -- RomM's folder for the platform, the first part of an export's folder.
    platform   TEXT PRIMARY KEY CHECK (platform ~ '^[a-z0-9-]{1,64}$'),
    -- When the export last changed a folder under it. A scan settles it
    -- only if it was asked for after this.
    owed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER romm_owed_scans_touch BEFORE UPDATE ON romm_owed_scans
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE TABLE romm_scans (
    id             UUID PRIMARY KEY,
    -- CASCADE: a record of what a RomM was asked means nothing without it.
    integration_id UUID NOT NULL REFERENCES integrations (id) ON DELETE CASCADE,
    -- The platform folders scanned.
    platforms      TEXT[] NOT NULL CHECK (cardinality(platforms) BETWEEN 1 AND 256),
    -- RomM's id for the task; none when RomM did not queue it.
    task_id        TEXT CHECK (task_id IS NULL OR task_id ~ '^[0-9A-Fa-f-]{1,64}$'),
    -- refused: RomM did not queue it (a scan already running, no scan
    -- worker, or RomM not answering), and detail says why. queued, running,
    -- finished, failed, canceled: RomM's word for the task. lost: RomM no
    -- longer has the task. timed_out: it had not ended when Tangible
    -- stopped waiting.
    state          TEXT NOT NULL CHECK (state IN (
                       'refused', 'queued', 'running', 'finished', 'failed', 'canceled',
                       'lost', 'timed_out')),
    detail         TEXT CHECK (detail IS NULL OR length(detail) <= 2000),
    -- What the scan counted, once it had.
    total_roms     BIGINT CHECK (total_roms IS NULL OR total_roms >= 0),
    scanned_roms   BIGINT CHECK (scanned_roms IS NULL OR scanned_roms >= 0),
    new_roms       BIGINT CHECK (new_roms IS NULL OR new_roms >= 0),
    updated_roms   BIGINT CHECK (updated_roms IS NULL OR updated_roms >= 0),
    requested_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at       TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT romm_scans_task_unless_refused CHECK ((state = 'refused') = (task_id IS NULL)),
    CONSTRAINT romm_scans_ended_when_over CHECK (
        (state IN ('queued', 'running')) = (ended_at IS NULL))
);

CREATE INDEX romm_scans_recent_idx ON romm_scans (integration_id, requested_at DESC);
CREATE INDEX romm_scans_open_idx ON romm_scans (integration_id) WHERE state IN ('queued', 'running');

CREATE TRIGGER romm_scans_touch BEFORE UPDATE ON romm_scans
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
