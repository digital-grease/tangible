-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Jobs handed to a download client (docs/plans/acquisition.md A4): one row
-- per torrent per integration, naming the import it feeds. The client's own
-- identity for the job (a torrent's infohash) is unique per integration, so
-- a torrent submitted twice is found rather than added twice; when its
-- import ended without an artifact, a resubmission points the row at a new
-- import. Names the client reports are not kept.
--
-- integrations.client_version holds the client's API version as last seen,
-- shown on the Integrations page.
-- Additive.

CREATE TABLE integration_jobs (
    id             UUID PRIMARY KEY,
    integration_id UUID NOT NULL REFERENCES integrations (id) ON DELETE RESTRICT,
    client_job_id  TEXT NOT NULL CHECK (length(client_job_id) BETWEEN 1 AND 200),
    import_job_id  UUID NOT NULL UNIQUE REFERENCES import_jobs (id) ON DELETE RESTRICT,
    -- The client's last word on it, such as `downloading` or `stalledUP`.
    client_state   TEXT CHECK (client_state IS NULL OR length(client_state) <= 64),
    created_by     TEXT NOT NULL CHECK (length(created_by) BETWEEN 1 AND 200),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT integration_jobs_identity_unique UNIQUE (integration_id, client_job_id)
);

ALTER TABLE integrations
    ADD COLUMN client_version TEXT
        CHECK (client_version IS NULL OR length(client_version) <= 64);

CREATE TRIGGER integration_jobs_touch BEFORE UPDATE ON integration_jobs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
