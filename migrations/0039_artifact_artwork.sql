-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- A cover picked by hand for an artifact (ADR 0023, docs/plans/cover-art.md
-- A3): the provider's reference a person chose from the metadata search on
-- the artifact's page. It comes before the cover of the request the
-- artifact fulfilled. The cover itself is an `artwork` row (0038), shared
-- with anything else that wants it.
--
-- Additive: the previous version neither reads nor writes it.

CREATE TABLE artifact_artwork (
    artifact_id UUID PRIMARY KEY REFERENCES artifacts (id) ON DELETE RESTRICT,
    source      TEXT NOT NULL CHECK (source IN ('tmdb', 'imdb', 'musicbrainz', 'igdb')),
    external_id TEXT NOT NULL CHECK (length(external_id) BETWEEN 1 AND 500),
    chosen_by   TEXT NOT NULL CHECK (length(chosen_by) BETWEEN 1 AND 200),
    chosen_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
