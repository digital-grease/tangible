-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- What a rip asks for about an encrypted disc (ADR 0013). `decrypt` says the
-- operator asked for decryption; `keep` is then the original they chose, and
-- is absent otherwise, because without decryption there is only one
-- possible original.
--
-- Additive, with defaults a rip asked for by the previous release satisfies:
-- it does not decrypt and keeps nothing to choose.

ALTER TABLE rip_jobs
    ADD COLUMN decrypt BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN keep TEXT
        CHECK (keep IS NULL OR keep IN ('encrypted_original', 'decrypted_original')),
    ADD CONSTRAINT rip_jobs_keep_only_when_decrypting CHECK ((keep IS NOT NULL) = decrypt);
