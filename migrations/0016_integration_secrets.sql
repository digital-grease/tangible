-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Integration secrets, sealed (ADR 0016): a download client's password, an
-- API key. Each is sealed with ChaCha20-Poly1305 under the key in
-- TANGIBLE_SECRET_KEY_FILE, with the integration's id and the field's name as
-- additional data, so a value moved to another row or field does not open.
-- key_id names the key (not the key itself), so a value sealed under another
-- key is told from a corrupt one, and rotation can find what to re-seal.
--
-- integrations.secret_ref (0001) is left unused: secrets live here now.
-- Additive.

CREATE TABLE integration_secrets (
    integration_id UUID NOT NULL REFERENCES integrations (id) ON DELETE RESTRICT,
    field          TEXT NOT NULL CHECK (field ~ '^[a-z][a-z0-9_]{0,63}$'),
    key_id         TEXT NOT NULL CHECK (key_id ~ '^[0-9a-f]{16}$'),
    nonce          BYTEA NOT NULL CHECK (octet_length(nonce) = 12),
    ciphertext     BYTEA NOT NULL CHECK (octet_length(ciphertext) BETWEEN 16 AND 4096),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (integration_id, field)
);
