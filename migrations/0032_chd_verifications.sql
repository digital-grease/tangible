-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- What `chdman verify` said of a CHD imported as an original, by the CHD's
-- SHA-256 and the chdman that ran (owner decision 2026-11-02, OWNER_QUEUE 14;
-- docs/plans/chd-images.md). The RomM export sends such a CHD as it is once
-- one has passed. A verdict is about those bytes and that tool, so it never
-- changes: a CHD that failed is tried again only by another chdman, which
-- may read what an older one could not.
--
-- Additive: the previous version neither reads nor writes it.

CREATE TABLE chd_verifications (
    sha256        TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    tool_version  TEXT NOT NULL CHECK (length(tool_version) BETWEEN 1 AND 200),
    passed        BOOLEAN NOT NULL,
    -- What chdman said when it failed, bounded.
    detail        TEXT CHECK (detail IS NULL OR length(detail) <= 2000),
    -- No updated_at: a verdict is history.
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (sha256, tool_version)
);
