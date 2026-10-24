-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- The tool a rip asked to read a CD with (ADR 0015). Absent means the
-- worker's best: redumper where its image has it, cdrdao everywhere else.
-- `cdrdao` is the operator's switch away from redumper; `redumper` insists on
-- it, and such a rip is refused rather than read with something else.
--
-- Additive and nullable: a rip asked for by the previous release asked for
-- nothing, which is what it got.

ALTER TABLE rip_jobs
    ADD COLUMN cd_engine TEXT
        CHECK (cd_engine IS NULL OR cd_engine IN ('redumper', 'cdrdao'));
