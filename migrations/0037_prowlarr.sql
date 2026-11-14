-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Prowlarr (ADR 0021, docs/plans/requests.md Q3): where an approved
-- request's releases are searched for, an integration an administrator
-- configures with its address and API key, the key sealed in
-- integration_secrets (0016) like any other. Requests are searched for in
-- one Prowlarr, so there is at most one.
--
-- Additive: the previous version never writes the new kind.

ALTER TABLE integrations DROP CONSTRAINT integrations_kind_check;
ALTER TABLE integrations ADD CONSTRAINT integrations_kind_check
    CHECK (kind IN (
        'romm', 'romm_api', 'qbittorrent', 'sabnzbd', 'nzbget',
        'metadata_provider', 'webhook', 's3', 'peer_instance',
        'musicbrainz', 'tmdb', 'igdb', 'prowlarr'
    ));

CREATE UNIQUE INDEX integrations_one_prowlarr ON integrations (kind) WHERE kind = 'prowlarr';
