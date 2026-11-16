-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Cover art (ADR 0023, docs/plans/cover-art.md A1): one row per cover a
-- provider keeps, named by the provider and its own id for the item
-- (`tmdb` `movie/603`, `imdb` `tt0113243`, `musicbrainz` a release id,
-- `igdb` a game id, `libretro` a system and game name). The artwork runner
-- adds a row for what is wanted, fetches it, and keeps the checked bytes:
-- a JPEG, PNG or WebP of at most 2 MiB. `missing` is a provider that has
-- none, asked again after thirty days; `failed` is retried with backoff.
--
-- Additive: the previous version neither reads nor writes it.

CREATE TABLE artwork (
    id              UUID PRIMARY KEY,
    source          TEXT NOT NULL
                    CHECK (source IN ('tmdb', 'imdb', 'musicbrainz', 'igdb', 'libretro')),
    external_id     TEXT NOT NULL CHECK (length(external_id) BETWEEN 1 AND 500),
    state           TEXT NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending', 'ready', 'missing', 'failed')),
    content_type    TEXT CHECK (content_type IS NULL
                                OR content_type IN ('image/jpeg', 'image/png', 'image/webp')),
    bytes           BYTEA CHECK (bytes IS NULL OR octet_length(bytes) BETWEEN 1 AND 2097152),
    sha256          TEXT CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$'),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    error           TEXT CHECK (error IS NULL OR length(error) <= 500),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    fetched_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT artwork_once UNIQUE (source, external_id),
    -- Ready is exactly having the checked bytes.
    CONSTRAINT artwork_ready_is_whole CHECK (
        (state = 'ready')
        = (bytes IS NOT NULL AND content_type IS NOT NULL AND sha256 IS NOT NULL)
    )
);

CREATE INDEX artwork_due_idx ON artwork (next_attempt_at) WHERE state <> 'ready';

CREATE TRIGGER artwork_touch BEFORE UPDATE ON artwork
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
