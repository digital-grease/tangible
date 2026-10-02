-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- People who sign in, and their sessions.
--
-- Until this existed every operator route was open to whoever could reach the
-- server. Workers have their own credentials (0002) and are not users.

CREATE TABLE users (
    id                UUID PRIMARY KEY,
    -- Stored lowercase by the application, and unique as such, so `Alice` and
    -- `alice` cannot be two accounts that look alike in a list.
    username          TEXT NOT NULL UNIQUE
                      CHECK (username ~ '^[a-z0-9._-]{1,64}$'),
    -- An Argon2id hash in PHC string form. Never the password, and never a
    -- hash of anything weaker.
    password_hash     TEXT NOT NULL CHECK (password_hash LIKE '$argon2id$%'),
    role              TEXT NOT NULL
                      CHECK (role IN ('viewer', 'operator', 'administrator')),
    -- Disabled rather than deleted: an account names the person behind audit
    -- events and burn jobs, which outlive its use.
    disabled_at       TIMESTAMPTZ,
    last_signed_in_at TIMESTAMPTZ,
    created_by        TEXT NOT NULL CHECK (length(created_by) BETWEEN 1 AND 200),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER users_touch BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE TABLE user_sessions (
    id             UUID PRIMARY KEY,
    -- The hash of the cookie's value, never the value: a row of this table is
    -- not enough to be signed in. Unique so a collision is an error rather
    -- than two sessions one cookie could satisfy.
    token_hash     TEXT NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    -- RESTRICT: a session records who was signed in, which an audit trail may
    -- need after the account is disabled.
    user_id        UUID NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    -- The anti-forgery token for this session. Not a credential on its own,
    -- which is why it can be stored as it is and handed back to the page.
    csrf_token     TEXT NOT NULL CHECK (length(csrf_token) BETWEEN 32 AND 128),
    -- Absolute end, whatever the activity.
    expires_at     TIMESTAMPTZ NOT NULL,
    -- Activity, for the idle timeout.
    last_seen_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at     TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER user_sessions_touch BEFORE UPDATE ON user_sessions
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE INDEX user_sessions_user_idx ON user_sessions (user_id);
-- Lookup is by hash on every request; live sessions are the ones that matter.
CREATE INDEX user_sessions_live_idx ON user_sessions (expires_at)
    WHERE revoked_at IS NULL;
