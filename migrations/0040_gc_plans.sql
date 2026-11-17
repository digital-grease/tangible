-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Collecting orphaned objects (ADR 0024, docs/plans/gc-sweep.md G2): a
-- plan kept with every candidate, so collecting it removes at most what was
-- planned, and kept after as the record of what went. An orphan has no
-- `cas_objects` row to carry that.
--
-- Additive: the previous version neither reads nor writes these.

CREATE TABLE gc_plans (
    id                   UUID PRIMARY KEY,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The administrator who made it, or NULL for the schedule.
    planned_by           TEXT CHECK (planned_by IS NULL OR length(planned_by) BETWEEN 1 AND 200),
    grace_seconds        BIGINT NOT NULL CHECK (grace_seconds >= 0),
    -- Whether every manifest and checkpoint could be read. One that was not
    -- is never collected.
    certain              BOOLEAN NOT NULL,
    candidate_count      BIGINT NOT NULL CHECK (candidate_count >= 0),
    candidate_bytes      BIGINT NOT NULL CHECK (candidate_bytes >= 0),
    stale_partial_files  BIGINT NOT NULL CHECK (stale_partial_files >= 0),
    -- Asked to be collected: when, and by whom (NULL for the schedule).
    collect_requested_at TIMESTAMPTZ,
    collect_requested_by TEXT
        CHECK (collect_requested_by IS NULL OR length(collect_requested_by) BETWEEN 1 AND 200),
    -- Collected: when, and what it did.
    collected_at         TIMESTAMPTZ,
    removed_count        BIGINT CHECK (removed_count >= 0),
    removed_bytes        BIGINT CHECK (removed_bytes >= 0),
    kept_count           BIGINT CHECK (kept_count >= 0),
    kept_bytes           BIGINT CHECK (kept_bytes >= 0),
    partial_files_removed BIGINT CHECK (partial_files_removed >= 0),
    CHECK (collected_at IS NULL OR collect_requested_at IS NOT NULL),
    CHECK (collect_requested_at IS NULL OR certain),
    CHECK ((collected_at IS NULL) = (removed_count IS NULL)
       AND (collected_at IS NULL) = (removed_bytes IS NULL)
       AND (collected_at IS NULL) = (kept_count IS NULL)
       AND (collected_at IS NULL) = (kept_bytes IS NULL)
       AND (collected_at IS NULL) = (partial_files_removed IS NULL))
);

-- The runner's queue: asked for and not yet collected.
CREATE INDEX gc_plans_queued ON gc_plans (collect_requested_at)
    WHERE collect_requested_at IS NOT NULL AND collected_at IS NULL;
-- The schedule asks when the last collection ended.
CREATE INDEX gc_plans_collected ON gc_plans (collected_at)
    WHERE collected_at IS NOT NULL;

CREATE TABLE gc_plan_objects (
    plan_id       UUID NOT NULL REFERENCES gc_plans (id) ON DELETE RESTRICT,
    sha256        TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size_bytes    BIGINT NOT NULL CHECK (size_bytes >= 0),
    modified_at   TIMESTAMPTZ NOT NULL,
    -- Whether the catalogue recorded it when planned.
    recorded      BOOLEAN NOT NULL,
    -- The ended import whose checkpoint lists it, when one does.
    import_job_id UUID REFERENCES import_jobs (id) ON DELETE RESTRICT,
    -- What collecting did with it; NULL until collected.
    outcome       TEXT CHECK (outcome IN (
                      'removed', 'kept_live', 'kept_held', 'kept_changed',
                      'kept_gone', 'kept_uncertain', 'kept_failed')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (plan_id, sha256)
);

CREATE TRIGGER gc_plans_touch BEFORE UPDATE ON gc_plans
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER gc_plan_objects_touch BEFORE UPDATE ON gc_plan_objects
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
