-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- A version on each catalog row, so an edit can say which one it was made
-- against (API.md 11, docs/plans/catalog-editing.md): an `If-Match` naming a
-- version the row has moved past is refused rather than overwriting somebody
-- else's change. Every row starts at 1; each edit adds one.
--
-- Additive: the previous version neither reads nor writes the column, and
-- the default fills it for the rows it creates.

ALTER TABLE titles    ADD COLUMN version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0);
ALTER TABLE editions  ADD COLUMN version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0);
ALTER TABLE disc_sets ADD COLUMN version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0);
ALTER TABLE discs     ADD COLUMN version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0);
