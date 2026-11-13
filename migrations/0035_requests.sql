-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Requests for items (ADR 0021, docs/plans/requests.md): somebody signed in
-- asks for an item, attesting to owning it; an operator or administrator
-- approves or declines; once approved, a release found through Prowlarr is
-- handed to a download client, and the import that follows it fulfils the
-- request. The attestation is not stored (the owner's choice).
--
-- Additive: the previous version neither reads nor writes it.

CREATE TABLE requests (
    id              UUID PRIMARY KEY,
    -- Who asked: the account, which decides who may see and withdraw it, and
    -- its name, which is what lists and the audit log show.
    requester_id    UUID NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    requested_by    TEXT NOT NULL CHECK (length(requested_by) BETWEEN 1 AND 200),
    kind            TEXT NOT NULL CHECK (kind IN (
                        'game', 'music', 'movie', 'television', 'software', 'other'
                    )),
    title           TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 300),
    -- What else names the item (platform, year, region, edition), shown and
    -- searched with: an object of short strings, bounded.
    details         JSONB NOT NULL DEFAULT '{}'::jsonb
                    CHECK (jsonb_typeof(details) = 'object'
                           AND octet_length(details::text) <= 4096),
    -- Where the item was found: [{"provider", "external_id"}], bounded.
    refs            JSONB NOT NULL DEFAULT '[]'::jsonb
                    CHECK (jsonb_typeof(refs) = 'array'
                           AND jsonb_array_length(refs) <= 16
                           AND octet_length(refs::text) <= 4096),
    note            TEXT CHECK (note IS NULL OR length(note) <= 2000),
    state           TEXT NOT NULL DEFAULT 'pending_approval'
                    CHECK (state IN (
                        'pending_approval', 'approved', 'declined', 'canceled',
                        'downloading', 'fulfilled', 'failed'
                    )),
    decided_by      TEXT CHECK (decided_by IS NULL OR length(decided_by) BETWEEN 1 AND 200),
    decided_at      TIMESTAMPTZ,
    decline_reason  TEXT CHECK (decline_reason IS NULL OR length(decline_reason) <= 2000),
    -- The download client a release was handed to, and the import following
    -- it. An integration may be removed; the import it made is kept.
    integration_id  UUID REFERENCES integrations (id) ON DELETE SET NULL,
    import_id       UUID REFERENCES import_jobs (id) ON DELETE RESTRICT,
    artifact_id     UUID REFERENCES artifacts (id) ON DELETE RESTRICT,
    error_code      TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail    TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 2000),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A decision names who made it and when, or neither.
    CONSTRAINT requests_decision_is_whole CHECK ((decided_by IS NULL) = (decided_at IS NULL)),
    -- Past approval, somebody decided; only a request withdrawn before then
    -- has no decision.
    CONSTRAINT requests_decided_past_approval CHECK (
        state IN ('pending_approval', 'canceled') OR decided_by IS NOT NULL
    ),
    -- A declined request says why.
    CONSTRAINT requests_declined_says_why CHECK (
        state <> 'declined' OR decline_reason IS NOT NULL
    ),
    -- A download, and how it ended, follows an import.
    CONSTRAINT requests_download_has_import CHECK (
        state NOT IN ('downloading', 'fulfilled', 'failed') OR import_id IS NOT NULL
    ),
    -- A fulfilled request names what the library holds for it.
    CONSTRAINT requests_fulfilled_has_artifact CHECK (
        state <> 'fulfilled' OR artifact_id IS NOT NULL
    )
);

CREATE INDEX requests_requester_idx ON requests (requester_id, id DESC);
CREATE INDEX requests_state_idx ON requests (state, id DESC);
-- An import finishing finds the request it fulfils.
CREATE INDEX requests_import_idx ON requests (import_id) WHERE import_id IS NOT NULL;

CREATE TRIGGER requests_touch BEFORE UPDATE ON requests
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- The web UI shows requests live (API.md 12). The identifier and state only:
-- every signed-in account hears the stream, and a viewer may read only its
-- own requests' titles.
CREATE TRIGGER requests_notify AFTER INSERT OR UPDATE ON requests
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change('requests', 'request.changed', 'id', 'state');
