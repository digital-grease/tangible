-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Signing in through an OpenID Connect provider (ADR 0018,
-- docs/plans/oidc.md O2).
--
-- An account the provider vouches for is found by the provider's issuer and
-- its subject for the person, never by name or email. Such an account has
-- no password, so password_hash becomes optional. A sign-in under way is
-- kept for ten minutes: the hash of the state the provider will send back
-- and of the cookie binding it to the browser that started it, the nonce
-- the ID token must carry, and the PKCE verifier the code exchange needs.
--
-- Additive, but for password_hash: a build before this one reads it as
-- always present and would fail on an account this one created by OIDC.

ALTER TABLE users ALTER COLUMN password_hash DROP NOT NULL;

CREATE TABLE user_identities (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL UNIQUE REFERENCES users (id) ON DELETE CASCADE,
    issuer      TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 2048),
    subject     TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 255),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

CREATE TABLE oidc_logins (
    id           UUID PRIMARY KEY,
    state_hash   TEXT NOT NULL UNIQUE CHECK (state_hash ~ '^[0-9a-f]{64}$'),
    browser_hash TEXT NOT NULL CHECK (browser_hash ~ '^[0-9a-f]{64}$'),
    nonce        TEXT NOT NULL CHECK (length(nonce) BETWEEN 16 AND 128),
    verifier     TEXT NOT NULL CHECK (length(verifier) BETWEEN 43 AND 128),
    -- A path on Tangible to go back to, never a URL elsewhere: a slash and
    -- then anything but another slash or a backslash (browsers read `/\`
    -- as `//`), and no backslash or control character anywhere.
    return_to    TEXT NOT NULL CHECK (return_to ~ '^/([^/\\]|$)'
                                      AND return_to !~ '[\\[:cntrl:]]'
                                      AND length(return_to) <= 2048),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    CHECK (expires_at > created_at)
);

CREATE INDEX oidc_logins_expires_idx ON oidc_logins (expires_at);
