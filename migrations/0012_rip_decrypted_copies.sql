-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- What a decrypting rip found and produced (ADR 0013).
--
-- A rip that keeps the encrypted original sends a decrypted copy beside it.
-- The copy is uploaded into its own staging area, copy_staging_id, chosen
-- when the rip is asked for, and is imported only once the original has
-- become an artifact, as a derivative of it; copy_import_job_id is that
-- import, with the same id, as import_job_id is the original's.
--
-- encryption is what the worker found on the disc; decryption_json the
-- library and tool it decrypted with, never a key. Both are what the worker
-- reported when it finished reading.
--
-- Additive: the previous release writes none of these, which every
-- constraint below allows.

ALTER TABLE rip_jobs
    ADD COLUMN copy_staging_id UUID UNIQUE,
    ADD COLUMN copy_import_job_id UUID REFERENCES import_jobs (id) ON DELETE RESTRICT,
    ADD COLUMN encryption TEXT
        CHECK (encryption IS NULL OR encryption IN ('none', 'css', 'aacs', 'unknown')),
    ADD COLUMN decryption_json JSONB
        CHECK (decryption_json IS NULL OR pg_column_size(decryption_json) <= 4096),
    ADD CONSTRAINT rip_jobs_copy_only_beside_an_encrypted_original
        CHECK (copy_staging_id IS NULL OR keep = 'encrypted_original'),
    ADD CONSTRAINT rip_jobs_copy_import_is_its_staging
        CHECK (copy_import_job_id IS NULL OR copy_import_job_id = copy_staging_id),
    ADD CONSTRAINT rip_jobs_decryption_only_when_asked
        CHECK (decryption_json IS NULL OR decrypt);

CREATE INDEX rip_jobs_copy_import_idx ON rip_jobs (copy_import_job_id)
    WHERE copy_import_job_id IS NOT NULL;
