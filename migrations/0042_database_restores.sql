-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Restores of a copy of the database from the Backups page (ADR 0026,
-- docs/plans/backups-ui.md D4). A restore that succeeded is recorded in
-- the database it made, with its restore check's report; one that failed,
-- in the database it left as it was. Immutable once written.
--
-- Additive: the previous version neither reads nor writes it.

CREATE TABLE database_restores (
    id           UUID PRIMARY KEY,
    requested_by TEXT NOT NULL CHECK (length(requested_by) BETWEEN 1 AND 200),
    copy_name    TEXT NOT NULL CHECK (length(copy_name) BETWEEN 1 AND 200),
    -- The copy taken of the database as it was, before anything changed.
    safety_copy  TEXT CHECK (safety_copy IS NULL OR length(safety_copy) <= 200),
    outcome      TEXT NOT NULL CHECK (outcome IN ('restored', 'failed')),
    error        TEXT CHECK (error IS NULL OR length(error) <= 4000),
    report       JSONB CHECK (report IS NULL OR pg_column_size(report) <= 262144),
    started_at   TIMESTAMPTZ NOT NULL,
    finished_at  TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((outcome = 'restored') = (error IS NULL))
);
