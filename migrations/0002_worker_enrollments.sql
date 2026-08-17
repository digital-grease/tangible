-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Enrollment tokens: the one-use secret an administrator hands to a new burn
-- worker so it can exchange it for a credential.
--
-- Added as a second migration rather than folded into the initial schema
-- because the initial schema is already released. Migrations are forward-only.

CREATE TABLE worker_enrollments (
    id                    UUID PRIMARY KEY,
    -- The hash, never the token. A row of this table is not enough to enroll.
    -- UNIQUE so a hash collision surfaces as an error rather than as two rows
    -- one secret could satisfy.
    token_hash            TEXT NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    state                 TEXT NOT NULL DEFAULT 'unused'
                          CHECK (state IN ('unused', 'consumed', 'revoked')),
    -- Short-lived. An enrollment token grants the ability to become a worker,
    -- and a worker drives hardware.
    expires_at            TIMESTAMPTZ NOT NULL,
    created_by            TEXT NOT NULL,
    consumed_at           TIMESTAMPTZ,
    -- RESTRICT: the enrollment row records how a worker came to exist, which
    -- stays meaningful for audit after the worker is revoked.
    consumed_by_worker_id UUID REFERENCES workers (id) ON DELETE RESTRICT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A consumed token names the worker it produced and when, and an unconsumed
    -- one names neither. Without this a bug could mark a token used while
    -- leaving no trace of what it created.
    CONSTRAINT worker_enrollments_consumed_is_whole CHECK (
        (state = 'consumed')
        = (consumed_at IS NOT NULL AND consumed_by_worker_id IS NOT NULL)
    )
);

-- Authentication looks a token up by hash on every enrollment attempt, and
-- expiry sweeps scan the unused ones.
CREATE INDEX worker_enrollments_unused_idx ON worker_enrollments (expires_at)
    WHERE state = 'unused';

CREATE TRIGGER worker_enrollments_touch BEFORE UPDATE ON worker_enrollments
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
