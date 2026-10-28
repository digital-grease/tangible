-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Preservation DATs and what they identify (docs/plans/dat-matching.md H2).
--
-- An administrator loads a Logiqx XML DAT (Redump's, No-Intro's); its games
-- and their files are kept here, the files indexed by SHA-1. A DAT loaded
-- with the name of one already loaded replaces it. An artifact matches a
-- game when its CD tracks (or, without tracks, its files other than a
-- descriptor) are the game's files other than a CUE, by SHA-1 and size:
-- `exact` when every file matched and nothing was left over, `partial`
-- otherwise. Matches are recomputed when a DAT is loaded or an artifact is
-- hashed; removing a DAT removes its games, files and matches with it.
--
-- Additive.

CREATE TABLE dat_files (
    id            UUID PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 2048),
    description   TEXT CHECK (description IS NULL OR length(description) <= 2048),
    version       TEXT CHECK (version IS NULL OR length(version) <= 2048),
    author        TEXT CHECK (author IS NULL OR length(author) <= 2048),
    homepage      TEXT CHECK (homepage IS NULL OR length(homepage) <= 2048),
    url           TEXT CHECK (url IS NULL OR length(url) <= 2048),
    -- The SHA-256 of the file as loaded, so the same file loaded again is
    -- recognised.
    source_sha256 TEXT NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    game_count    INTEGER NOT NULL CHECK (game_count >= 0),
    rom_count     INTEGER NOT NULL CHECK (rom_count >= 0),
    loaded_by     TEXT NOT NULL CHECK (length(loaded_by) BETWEEN 1 AND 200),
    loaded_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER dat_files_touch BEFORE UPDATE ON dat_files
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE TABLE dat_games (
    id          UUID PRIMARY KEY,
    dat_id      UUID NOT NULL REFERENCES dat_files (id) ON DELETE CASCADE,
    name        TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 2048),
    description TEXT CHECK (description IS NULL OR length(description) <= 2048),
    category    TEXT CHECK (category IS NULL OR length(category) <= 2048),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX dat_games_dat_idx ON dat_games (dat_id);

CREATE TABLE dat_roms (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    game_id    UUID NOT NULL REFERENCES dat_games (id) ON DELETE CASCADE,
    name       TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 2048),
    size       BIGINT CHECK (size IS NULL OR size >= 0),
    crc32      TEXT CHECK (crc32 IS NULL OR crc32 ~ '^[0-9a-f]{8}$'),
    md5        TEXT CHECK (md5 IS NULL OR md5 ~ '^[0-9a-f]{32}$'),
    sha1       TEXT CHECK (sha1 IS NULL OR sha1 ~ '^[0-9a-f]{40}$'),
    sha256     TEXT CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$'),
    status     TEXT CHECK (status IS NULL OR length(status) <= 64),
    -- A CUE sheet's hash identifies nothing: never a match key.
    is_cue     BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX dat_roms_game_idx ON dat_roms (game_id);
CREATE INDEX dat_roms_sha1_idx ON dat_roms (sha1) WHERE sha1 IS NOT NULL AND NOT is_cue;

CREATE TABLE artifact_dat_matches (
    artifact_id  UUID NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    game_id      UUID NOT NULL REFERENCES dat_games (id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('exact', 'partial')),
    roms_matched INTEGER NOT NULL CHECK (roms_matched >= 1),
    roms_total   INTEGER NOT NULL CHECK (roms_total >= roms_matched),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (artifact_id, game_id),
    CONSTRAINT artifact_dat_matches_exact_is_whole CHECK (
        kind <> 'exact' OR roms_matched = roms_total)
);

CREATE INDEX artifact_dat_matches_game_idx ON artifact_dat_matches (game_id);
