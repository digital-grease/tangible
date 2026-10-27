-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- RomM's API (docs/plans/romm-api.md M1, backlog E9-05 and E9-06): an
-- administrator configures the API of the RomM the export writes into, with
-- a client token sealed in integration_secrets (0016). There is one export,
-- so there is at most one such integration. The export's own row (kind
-- romm, 0007) is unchanged.
--
-- Additive: the previous version never writes the new kind.

ALTER TABLE integrations DROP CONSTRAINT integrations_kind_check;
ALTER TABLE integrations ADD CONSTRAINT integrations_kind_check
    CHECK (kind IN (
        'romm', 'romm_api', 'qbittorrent', 'sabnzbd', 'nzbget',
        'metadata_provider', 'webhook', 's3', 'peer_instance'
    ));

CREATE UNIQUE INDEX integrations_one_romm_api ON integrations (kind) WHERE kind = 'romm_api';
