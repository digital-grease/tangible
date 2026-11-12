-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- How many times an import's run was interrupted: the server stopped while
-- it held the import's lease, so the lease lapsed with the work unfinished.
-- An import that makes the server run out of memory, such as a crafted
-- archive, would otherwise be taken up again on every start and stop it
-- again (security review, 2026-11-12). The claim counts each one, and fails
-- the import for good at the third; a retry by a person starts the count
-- again.
--
-- Additive: the previous version neither sets nor reads it, and 0 describes
-- every import it has.

ALTER TABLE import_jobs
    ADD COLUMN interrupted_runs INTEGER NOT NULL DEFAULT 0 CHECK (interrupted_runs >= 0);
