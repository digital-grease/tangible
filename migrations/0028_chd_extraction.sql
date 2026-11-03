-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- A CHD imported as an original extracted by chdman
-- (docs/plans/chd-images.md C3): a CUE/BIN of a CD CHD, the ISO of a DVD
-- one. Their jobs name the tool `chdman`.
--
-- Additive: the constraint only widens, so the previous version, which never
-- writes the new values, runs unchanged against it.

ALTER TABLE derivation_jobs
    DROP CONSTRAINT derivation_jobs_transformation_check,
    ADD CONSTRAINT derivation_jobs_transformation_check
        CHECK (transformation IN (
            'chd_create_cd', 'chd_create_dvd', 'cue_bin_from_ccd', 'cue_bin_from_mds',
            'iso_from_mds', 'cue_bin_from_chd', 'iso_from_chd'
        ));
