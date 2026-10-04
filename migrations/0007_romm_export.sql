-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Exporting games to a RomM library.
--
-- An edition records the platform it was released on, in RomM's own slugs,
-- and whether it should appear in RomM. The export itself is a folder view
-- kept in step by the server; what it last wrote, or why it could not, is
-- recorded per edition, and each file it put in place is a row in
-- export_receipts (0001), which is what lets it remove only what it made.

ALTER TABLE editions
    ADD COLUMN platform    TEXT CHECK (platform IS NULL OR platform ~ '^[a-z0-9-]{1,64}$'),
    ADD COLUMN romm_export BOOLEAN NOT NULL DEFAULT FALSE,
    -- Exporting needs a platform: it is the folder the game goes in.
    ADD CONSTRAINT editions_romm_export_needs_platform
        CHECK (NOT romm_export OR platform IS NOT NULL);

CREATE INDEX editions_romm_export_idx ON editions (id) WHERE romm_export;

CREATE TABLE romm_edition_exports (
    -- RESTRICT: the record of what was written into somebody's RomM library
    -- outlives a change of mind about the edition.
    edition_id   UUID PRIMARY KEY REFERENCES editions (id) ON DELETE RESTRICT,
    -- current: the folder matches the library. blocked: it cannot be written
    -- as things stand, and detail says why. failed: writing it went wrong.
    -- removed: switched off and taken out of RomM.
    state        TEXT NOT NULL CHECK (state IN ('current', 'blocked', 'failed', 'removed')),
    -- The folder written, relative to the export root: platform/game.
    folder       TEXT CHECK (folder IS NULL OR length(folder) BETWEEN 1 AND 1024),
    detail       TEXT CHECK (detail IS NULL OR length(detail) <= 2000),
    file_count   INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    exported_at  TIMESTAMPTZ,
    checked_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER romm_edition_exports_touch BEFORE UPDATE ON romm_edition_exports
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
