-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- When the server takes copies of the database by itself (ADR 0026,
-- docs/plans/backups-ui.md D5). At most one row; with none, the default
-- holds: every day at 03:00 UTC. A restore brings back the schedule its
-- copy held, as it does every other setting the catalogue keeps.
--
-- Additive: the previous version neither reads nor writes it.

CREATE TABLE backup_schedule (
    singleton     BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    enabled       BOOLEAN NOT NULL,
    -- Minutes past midnight, UTC.
    minute_of_day SMALLINT NOT NULL CHECK (minute_of_day BETWEEN 0 AND 1439),
    updated_by    TEXT NOT NULL CHECK (length(updated_by) BETWEEN 1 AND 200),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER backup_schedule_touch BEFORE UPDATE ON backup_schedule
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
