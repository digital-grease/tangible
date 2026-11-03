-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Worker credentials are rotated (BURN_WORKER_PROTOCOL.md 5,
-- docs/plans/credential-rotation.md). Rotation is two-phase so that a lost
-- answer never locks a worker out: the worker asks for a new credential,
-- the server keeps its hash as the next one, and the old credential keeps
-- working until the new one is first presented, when the new one takes its
-- place in the same statement and the old one stops working.
--
-- Additive: the previous version neither reads nor writes these columns. A
-- worker it enrolls gets a credential issued now, by the default, which is
-- when it was.

ALTER TABLE workers
    -- When the credential in use was issued. A worker enrolled before this
    -- migration was issued its credential when it enrolled.
    ADD COLUMN credential_issued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The credential a rotation issued, hashed, until it is first used.
    ADD COLUMN next_credential_hash TEXT,
    ADD COLUMN next_credential_issued_at TIMESTAMPTZ,
    -- When an administrator asked for the credential to be rotated, until it
    -- is.
    ADD COLUMN credential_rotation_requested_at TIMESTAMPTZ,
    ADD CONSTRAINT workers_next_credential_whole CHECK (
        (next_credential_hash IS NULL) = (next_credential_issued_at IS NULL)
    );

UPDATE workers SET credential_issued_at = created_at;

-- Credentials are found by their hash, the next one included.
CREATE UNIQUE INDEX workers_credential_hash_idx ON workers (credential_hash);
CREATE UNIQUE INDEX workers_next_credential_hash_idx ON workers (next_credential_hash)
    WHERE next_credential_hash IS NOT NULL;
