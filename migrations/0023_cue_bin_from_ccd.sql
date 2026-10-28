-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- A derivation Tangible makes itself (docs/plans/ccd-images.md C3): a CUE/BIN
-- of a CloneCD image, its sheet written from the image's tracks and its IMG
-- the BIN, dropping the subchannel. Its jobs name the tool `tangible`.
--
-- Additive: the constraint only widens, so the previous version, which never
-- writes the new value, runs unchanged against it.

ALTER TABLE derivation_jobs
    DROP CONSTRAINT derivation_jobs_transformation_check,
    ADD CONSTRAINT derivation_jobs_transformation_check
        CHECK (transformation IN ('chd_create_cd', 'chd_create_dvd', 'cue_bin_from_ccd'));
