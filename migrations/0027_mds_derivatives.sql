-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Derivations Tangible makes itself of an Alcohol 120% image
-- (docs/plans/mds-images.md M3): a CUE/BIN of a CD image, and the ISO a DVD
-- image's one data file is. Their jobs name the tool `tangible`.
--
-- Additive: the constraint only widens, so the previous version, which never
-- writes the new values, runs unchanged against it.

ALTER TABLE derivation_jobs
    DROP CONSTRAINT derivation_jobs_transformation_check,
    ADD CONSTRAINT derivation_jobs_transformation_check
        CHECK (transformation IN (
            'chd_create_cd', 'chd_create_dvd', 'cue_bin_from_ccd', 'cue_bin_from_mds', 'iso_from_mds'
        ));
