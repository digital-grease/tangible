-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Deleting artifacts, as tombstones (ADR 0020, docs/plans/deletion.md).
--
-- An artifact is kept, pending deletion until `delete_after`, or deleted:
-- its bytes gone, its row, components and manifest kept. `pending_deletion`
-- has existed since 0001 and was never set; the request's time, who asked
-- and when the bytes may go come with it now, and `deleted_at` marks the
-- tombstone. An object whose bytes were collected keeps its row, since a
-- deleted artifact's components still name it, and says when it was
-- removed; bytes that come back in a later import are stored again and the
-- row is reused.
--
-- Additive: the previous version neither sets nor reads these, and the
-- defaults describe every artifact it makes, kept.

-- No version has set the flag; a row that has it was set by hand, and is
-- kept rather than deleted on nobody's request. It can be asked to be
-- deleted through the routes.
UPDATE artifacts SET pending_deletion = FALSE WHERE pending_deletion;

ALTER TABLE artifacts
    ADD COLUMN deletion_requested_at TIMESTAMPTZ,
    ADD COLUMN deletion_requested_by TEXT
        CHECK (deletion_requested_by IS NULL OR length(deletion_requested_by) BETWEEN 1 AND 200),
    ADD COLUMN delete_after TIMESTAMPTZ,
    ADD COLUMN deleted_at TIMESTAMPTZ,
    -- Pending: asked for, by somebody, with a time the bytes may go, and
    -- not yet deleted.
    ADD CONSTRAINT artifacts_pending_deletion_whole CHECK (
        NOT pending_deletion
        OR (deletion_requested_at IS NOT NULL
            AND deletion_requested_by IS NOT NULL
            AND delete_after IS NOT NULL
            AND deleted_at IS NULL)
    ),
    -- A tombstone was asked for first, and is no longer pending.
    ADD CONSTRAINT artifacts_deleted_was_asked CHECK (
        deleted_at IS NULL
        OR (deletion_requested_at IS NOT NULL AND NOT pending_deletion)
    );

CREATE INDEX artifacts_delete_after_idx ON artifacts (delete_after) WHERE pending_deletion;
CREATE INDEX artifacts_deleted_idx ON artifacts (id) WHERE deleted_at IS NOT NULL;

ALTER TABLE cas_objects
    ADD COLUMN removed_at TIMESTAMPTZ,
    -- Only an object nothing refers to is collected.
    ADD CONSTRAINT cas_objects_removed_unreferenced CHECK (
        removed_at IS NULL OR reference_count = 0
    );
