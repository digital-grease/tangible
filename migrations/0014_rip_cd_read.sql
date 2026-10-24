-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- How redumper read a CD (ADR 0015): the drive's read offset it corrected,
-- or none when it has no offset for the drive; the disc's write offset; and
-- whether it treated the drive as generic. The same record goes into the
-- original's manifest, as origin.physical_dump.cd_read.
--
-- Additive and nullable: only a CD read by redumper has one, and no rip of
-- the previous release was.

ALTER TABLE rip_jobs
    ADD COLUMN cd_read_json JSONB
        CHECK (cd_read_json IS NULL
               OR (jsonb_typeof(cd_read_json) = 'object'
                   AND pg_column_size(cd_read_json) <= 4096));
