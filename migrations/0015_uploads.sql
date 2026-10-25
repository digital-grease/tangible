-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Uploads sent in parts (docs/plans/acquisition.md A3), so a dropped
-- connection costs one part rather than the whole file. An upload's id is
-- also its staging area's and, once complete, its import's: the bytes never
-- move between the last part arriving and the import starting.
--
-- Rows are never deleted: an upload left idle past expires_at is marked
-- abandoned and its staging area removed, and the row stays as a record.
-- Additive.

CREATE TABLE uploads (
    id              UUID PRIMARY KEY,
    filename        TEXT NOT NULL CHECK (length(filename) BETWEEN 1 AND 255),
    total_bytes     BIGINT NOT NULL CHECK (total_bytes >= 0),
    part_size       BIGINT NOT NULL CHECK (part_size > 0),
    expected_sha256 TEXT CHECK (expected_sha256 IS NULL OR expected_sha256 ~ '^[0-9a-f]{64}$'),
    state           TEXT NOT NULL DEFAULT 'open'
                    CHECK (state IN ('open', 'completed', 'abandoned')),
    created_by      TEXT NOT NULL CHECK (length(created_by) BETWEEN 1 AND 200),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX uploads_open_expiry_idx ON uploads (expires_at) WHERE state = 'open';

CREATE TABLE upload_parts (
    upload_id   UUID NOT NULL REFERENCES uploads (id) ON DELETE RESTRICT,
    part_number INTEGER NOT NULL CHECK (part_number >= 0),
    bytes       BIGINT NOT NULL CHECK (bytes >= 0),
    sha256      TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- A part sent again replaces itself.
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (upload_id, part_number)
);
