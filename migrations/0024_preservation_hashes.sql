-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Preservation hashes (docs/plans/dat-matching.md H1): CRC32, MD5 and SHA-1
-- of each component, kept in artifact_hashes (0001, unused until now), and of
-- each CD track's bytes, kept here, for matching against preservation DATs
-- such as Redump's, which list each track's hashes.
--
-- They are catalogue facts computed from stored bytes, not manifest content:
-- SHA-256 stays the identity, and these can always be computed again.
--
-- Every artifact, those already catalogued included, starts pending; a
-- background runner reads its stored objects once and records the hashes,
-- with a lease so two servers never hash the same artifact, and a retry
-- held off by the lease after a failure.
--
-- Additive: the previous version never reads or writes the new columns or
-- table.

ALTER TABLE artifacts
    ADD COLUMN hashes_state TEXT NOT NULL DEFAULT 'pending'
        CHECK (hashes_state IN ('pending', 'done', 'failed')),
    ADD COLUMN hashes_error TEXT CHECK (hashes_error IS NULL OR length(hashes_error) <= 2000),
    ADD COLUMN hashes_attempts INTEGER NOT NULL DEFAULT 0 CHECK (hashes_attempts >= 0),
    ADD COLUMN hashes_lease_owner TEXT
        CHECK (hashes_lease_owner IS NULL OR length(hashes_lease_owner) BETWEEN 1 AND 200),
    ADD COLUMN hashes_lease_until TIMESTAMPTZ,
    ADD COLUMN hashed_at TIMESTAMPTZ,
    ADD CONSTRAINT artifacts_hashes_lease_whole CHECK (
        (hashes_lease_owner IS NULL) = (hashes_lease_until IS NULL)),
    ADD CONSTRAINT artifacts_hashed_when_done CHECK (
        (hashes_state = 'done') = (hashed_at IS NOT NULL));

CREATE INDEX artifacts_hashes_pending_idx ON artifacts (id) WHERE hashes_state = 'pending';

CREATE TABLE artifact_track_hashes (
    artifact_id  UUID NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    track_number INTEGER NOT NULL CHECK (track_number BETWEEN 1 AND 99),
    length_bytes BIGINT NOT NULL CHECK (length_bytes >= 0),
    crc32        TEXT NOT NULL CHECK (crc32 ~ '^[0-9a-f]{8}$'),
    md5          TEXT NOT NULL CHECK (md5 ~ '^[0-9a-f]{32}$'),
    sha1         TEXT NOT NULL CHECK (sha1 ~ '^[0-9a-f]{40}$'),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (artifact_id, track_number)
);

CREATE INDEX artifact_track_hashes_sha1_idx ON artifact_track_hashes (sha1);
