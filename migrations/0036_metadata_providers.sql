-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Metadata providers (ADR 0021, docs/plans/requests.md Q2): what a person
-- searches to find the item they ask for. MusicBrainz (no key), TMDB (a
-- read access token) and IGDB (a Twitch client ID and secret), each an
-- integration an administrator configures, its key sealed in
-- integration_secrets (0016) like any other. Each is one service, so there
-- is at most one of each.
--
-- Additive: the previous version never writes the new kinds.

ALTER TABLE integrations DROP CONSTRAINT integrations_kind_check;
ALTER TABLE integrations ADD CONSTRAINT integrations_kind_check
    CHECK (kind IN (
        'romm', 'romm_api', 'qbittorrent', 'sabnzbd', 'nzbget',
        'metadata_provider', 'webhook', 's3', 'peer_instance',
        'musicbrainz', 'tmdb', 'igdb'
    ));

CREATE UNIQUE INDEX integrations_one_musicbrainz ON integrations (kind) WHERE kind = 'musicbrainz';
CREATE UNIQUE INDEX integrations_one_tmdb ON integrations (kind) WHERE kind = 'tmdb';
CREATE UNIQUE INDEX integrations_one_igdb ON integrations (kind) WHERE kind = 'igdb';
