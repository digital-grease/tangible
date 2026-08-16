-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Initial schema.
--
-- Conventions:
--   * every table has a primary key and created_at; updated_at exists only
--     where mutation is allowed, so its absence documents immutability
--   * enums persist as text with a CHECK constraint, never as ordinals: a
--     reordered Rust enum must not silently reinterpret existing rows, and
--     text keeps the database readable
--   * foreign keys state their delete behaviour explicitly
--   * nothing that represents work performed or media consumed cascades away
--
-- Every documented domain constraint is implemented here, with each one
-- annotated by the invariant it protects.

-- =============================================================================
-- Catalog: the abstract work down to one logical disc.
-- =============================================================================

CREATE TABLE titles (
    id              UUID PRIMARY KEY,
    kind            TEXT NOT NULL DEFAULT 'unknown'
                    CHECK (kind IN (
                        'movie', 'television', 'game', 'software',
                        'operating_system', 'music', 'data_archive',
                        'training', 'unknown', 'custom'
                    )),
    display_title   TEXT NOT NULL CHECK (length(display_title) BETWEEN 1 AND 500),
    sort_title      TEXT NOT NULL CHECK (length(sort_title) BETWEEN 1 AND 500),
    original_title  TEXT CHECK (original_title IS NULL OR length(original_title) <= 500),
    description     TEXT CHECK (description IS NULL OR length(description) <= 10000),
    release_year    SMALLINT CHECK (release_year IS NULL OR release_year BETWEEN 1870 AND 2200),
    metadata_status TEXT NOT NULL DEFAULT 'none'
                    CHECK (metadata_status IN ('none', 'manual', 'provider', 'mixed')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX titles_sort_title_idx ON titles (sort_title);
CREATE INDEX titles_kind_idx ON titles (kind);

CREATE TABLE editions (
    id            UUID PRIMARY KEY,
    title_id      UUID NOT NULL REFERENCES titles (id) ON DELETE RESTRICT,
    display_name  TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 500),
    publisher     TEXT CHECK (publisher IS NULL OR length(publisher) <= 300),
    release_date  DATE,
    region        TEXT CHECK (region IS NULL OR length(region) <= 100),
    languages     TEXT[] NOT NULL DEFAULT '{}',
    edition_tags  TEXT[] NOT NULL DEFAULT '{}',
    -- Namespaced provider identifiers, e.g. {"igdb:game": "1234"}. A column
    -- per provider would mean a migration per integration.
    external_ids  JSONB NOT NULL DEFAULT '{}'::jsonb,
    notes         TEXT CHECK (notes IS NULL OR length(notes) <= 10000),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX editions_title_id_idx ON editions (title_id);
CREATE INDEX editions_external_ids_idx ON editions USING gin (external_ids);

CREATE TABLE disc_sets (
    id                  UUID PRIMARY KEY,
    edition_id          UUID NOT NULL REFERENCES editions (id) ON DELETE RESTRICT,
    name                TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 500),
    disc_count_expected SMALLINT CHECK (disc_count_expected IS NULL OR disc_count_expected > 0),
    set_kind            TEXT NOT NULL DEFAULT 'unknown'
                        CHECK (set_kind IN (
                            'single_disc', 'multi_disc', 'season_box',
                            'installation_set', 'feature_and_supplements',
                            'compilation', 'unknown'
                        )),
    notes               TEXT CHECK (notes IS NULL OR length(notes) <= 10000),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX disc_sets_edition_id_idx ON disc_sets (edition_id);

CREATE TABLE discs (
    id                      UUID PRIMARY KEY,
    disc_set_id             UUID NOT NULL REFERENCES disc_sets (id) ON DELETE RESTRICT,
    sequence_number         SMALLINT NOT NULL CHECK (sequence_number > 0),
    display_name            TEXT CHECK (display_name IS NULL OR length(display_name) <= 500),
    media_family            TEXT NOT NULL DEFAULT 'unknown'
                            CHECK (media_family IN (
                                'cd', 'dvd', 'bluray', 'uhd_bluray', 'gd_rom',
                                'proprietary_optical', 'unknown'
                            )),
    region                  TEXT CHECK (region IS NULL OR length(region) <= 100),
    volume_label            TEXT CHECK (volume_label IS NULL OR length(volume_label) <= 255),
    expected_sector_count   BIGINT CHECK (expected_sector_count IS NULL OR expected_sector_count >= 0),
    expected_capacity_bytes BIGINT CHECK (expected_capacity_bytes IS NULL OR expected_capacity_bytes >= 0),
    layer_break_lba         BIGINT CHECK (layer_break_lba IS NULL OR layer_break_lba >= 0),
    topology_summary        JSONB,
    -- Defaults to the claim that promises nothing. Anything stronger must be
    -- backed by evidence, which is how the project avoids implying a burned
    -- disc will satisfy console authentication.
    compatibility_claim     TEXT NOT NULL DEFAULT 'unknown'
                            CHECK (compatibility_claim IN (
                                'unknown', 'data_reproduction_expected',
                                'player_compatibility_expected',
                                'emulator_compatibility_expected',
                                'original_hardware_compatibility_expected',
                                'known_not_reproducible'
                            )),
    compatibility_evidence  JSONB,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Unique disc sequence within its parent set.
    CONSTRAINT discs_sequence_unique_within_set UNIQUE (disc_set_id, sequence_number),

    -- A claim stronger than "unknown" must carry evidence.
    CONSTRAINT discs_claim_requires_evidence CHECK (
        compatibility_claim = 'unknown' OR compatibility_evidence IS NOT NULL
    )
);

CREATE INDEX discs_disc_set_id_idx ON discs (disc_set_id);

-- =============================================================================
-- Content-addressed storage.
-- =============================================================================

-- One row per distinct blob of bytes, keyed by digest. Deduplication happens
-- here: two identical imports reference one object.
CREATE TABLE cas_objects (
    -- Unique SHA-256 object row. The digest is the primary key, which makes
    -- duplication impossible rather than merely detected. Lowercase hex is
    -- enforced so two spellings of one digest cannot become two rows.
    sha256          TEXT PRIMARY KEY CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size_bytes      BIGINT NOT NULL CHECK (size_bytes >= 0),
    -- Incremented and decremented as components attach and detach. Garbage
    -- collection considers only objects at zero.
    reference_count INTEGER NOT NULL DEFAULT 0 CHECK (reference_count >= 0),
    -- A retention lock blocks collection regardless of reference count.
    retention_lock  BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Partial index: garbage collection scans only unreferenced, unlocked objects,
-- which is a tiny fraction of a healthy library.
CREATE INDEX cas_objects_collectable_idx ON cas_objects (created_at)
    WHERE reference_count = 0 AND retention_lock = FALSE;

-- =============================================================================
-- Artifacts.
-- =============================================================================

CREATE TABLE artifacts (
    id                 UUID PRIMARY KEY,
    artifact_kind      TEXT NOT NULL DEFAULT 'unknown'
                       CHECK (artifact_kind IN (
                           'single_file_image', 'multi_file_image',
                           'directory_tree', 'archive_container',
                           'raw_track_set', 'unknown'
                       )),
    format             TEXT NOT NULL DEFAULT 'unknown'
                       CHECK (format IN (
                           'iso', 'cue_bin', 'toc_bin', 'ccd_img_sub',
                           'mds_mdf', 'chd', 'bdmv_directory',
                           'video_ts_directory', 'raw', 'unknown'
                       )),
    origin             TEXT NOT NULL
                       CHECK (origin IN (
                           'imported_original', 'derived', 'physical_dump',
                           'generated', 'external_reference'
                       )),
    manifest_version   TEXT NOT NULL,
    total_bytes        BIGINT NOT NULL CHECK (total_bytes >= 0),
    component_count    INTEGER NOT NULL CHECK (component_count >= 0),
    primary_sha256     TEXT REFERENCES cas_objects (sha256) ON DELETE RESTRICT
                       CHECK (primary_sha256 IS NULL OR primary_sha256 ~ '^[0-9a-f]{64}$'),
    validation_state   TEXT NOT NULL DEFAULT 'pending'
                       CHECK (validation_state IN (
                           'pending', 'valid', 'valid_with_warnings',
                           'invalid', 'unsupported', 'quarantined'
                       )),
    validation_summary JSONB,
    quarantine_state   TEXT NOT NULL DEFAULT 'none'
                       CHECK (quarantine_state IN ('none', 'pending', 'confirmed', 'cleared')),
    retention_lock     BOOLEAN NOT NULL DEFAULT FALSE,
    pending_deletion   BOOLEAN NOT NULL DEFAULT FALSE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Deliberately no updated_at on the immutable identity columns. Only
    -- validation and lifecycle flags change after registration, and those are
    -- tracked by the audit log.
    state_changed_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX artifacts_format_idx ON artifacts (format);
CREATE INDEX artifacts_validation_state_idx ON artifacts (validation_state);
CREATE INDEX artifacts_primary_sha256_idx ON artifacts (primary_sha256);
CREATE INDEX artifacts_pending_deletion_idx ON artifacts (id) WHERE pending_deletion = TRUE;

CREATE TABLE artifact_components (
    id              UUID PRIMARY KEY,
    -- The only cascade in the schema. A component has no meaning apart from
    -- its artifact, and deleting an artifact is already a guarded two-step
    -- operation. Nothing recording work performed or media consumed cascades.
    artifact_id     UUID NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    -- Portable, relative, slash-separated, validated by LogicalPath before it
    -- reaches the database. The CHECK is defence in depth against a path that
    -- arrives by some other route.
    logical_path    TEXT NOT NULL
                    CHECK (
                        length(logical_path) BETWEEN 1 AND 1024
                        AND logical_path !~ '^/'
                        AND logical_path !~ '(^|/)\.\.(/|$)'
                        AND logical_path !~ '(^|/)\.(/|$)'
                        AND logical_path !~ '\\'
                        AND position(E'\\000' IN logical_path) = 0
                    ),
    role            TEXT NOT NULL DEFAULT 'unknown'
                    CHECK (role IN (
                        'primary_image', 'descriptor', 'track_data',
                        'subchannel', 'metadata', 'certificate_tree',
                        'filesystem_file', 'archive', 'unknown'
                    )),
    media_type      TEXT CHECK (media_type IS NULL OR length(media_type) <= 255),
    length_bytes    BIGINT NOT NULL CHECK (length_bytes >= 0),
    -- RESTRICT, not CASCADE: deleting a referenced CAS object is forbidden.
    -- Bytes outlive catalog edits.
    sha256          TEXT NOT NULL REFERENCES cas_objects (sha256) ON DELETE RESTRICT
                    CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    ordinal         INTEGER NOT NULL CHECK (ordinal >= 0),
    source_filename TEXT CHECK (source_filename IS NULL OR length(source_filename) <= 1024),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Unique logical path and unique ordinal within an artifact. Two
    -- components claiming one path would make materialization order-dependent.
    CONSTRAINT artifact_components_path_unique UNIQUE (artifact_id, logical_path),
    CONSTRAINT artifact_components_ordinal_unique UNIQUE (artifact_id, ordinal)
);

CREATE INDEX artifact_components_artifact_id_idx ON artifact_components (artifact_id);
CREATE INDEX artifact_components_sha256_idx ON artifact_components (sha256);

-- No updated_at, and enforced rather than merely documented: components are
-- immutable after registration. An artifact is a fixed set of bytes; changing
-- one would silently invalidate every recorded hash, manifest and burn that
-- referenced it.
CREATE FUNCTION reject_component_mutation() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION
        'artifact_components are immutable after registration (artifact %, path %)',
        OLD.artifact_id, OLD.logical_path
        USING ERRCODE = 'restrict_violation';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER artifact_components_immutable
    BEFORE UPDATE ON artifact_components
    FOR EACH ROW EXECUTE FUNCTION reject_component_mutation();

CREATE TABLE artifact_hashes (
    id           UUID PRIMARY KEY,
    artifact_id  UUID NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    component_id UUID REFERENCES artifact_components (id) ON DELETE CASCADE,
    scope        TEXT NOT NULL CHECK (scope IN ('artifact', 'component')),
    algorithm    TEXT NOT NULL
                 CHECK (algorithm IN ('sha256', 'sha1', 'md5', 'crc32', 'blake3', 'provider_specific')),
    value        TEXT NOT NULL CHECK (length(value) BETWEEN 1 AND 256),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A component-scoped hash must name a component, and an artifact-scoped
    -- one must not.
    CONSTRAINT artifact_hashes_scope_agrees CHECK (
        (scope = 'component' AND component_id IS NOT NULL)
        OR (scope = 'artifact' AND component_id IS NULL)
    ),
    CONSTRAINT artifact_hashes_one_per_algorithm UNIQUE (artifact_id, component_id, algorithm)
);

CREATE INDEX artifact_hashes_lookup_idx ON artifact_hashes (algorithm, value);

CREATE TABLE artifact_disc_links (
    artifact_id   UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    disc_id       UUID NOT NULL REFERENCES discs (id) ON DELETE RESTRICT,
    relationship  TEXT NOT NULL DEFAULT 'unknown'
                  CHECK (relationship IN ('representation_of', 'contains', 'supplement_for', 'unknown')),
    confidence    REAL NOT NULL DEFAULT 0 CHECK (confidence BETWEEN 0 AND 1),
    evidence_json JSONB,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (artifact_id, disc_id)
);

CREATE INDEX artifact_disc_links_disc_id_idx ON artifact_disc_links (disc_id);

CREATE TABLE derivations (
    id                       UUID PRIMARY KEY,
    -- RESTRICT both ways: lineage is evidence. Deleting a parent must not
    -- silently orphan the record of what was derived from it.
    parent_artifact_id       UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    child_artifact_id        UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    transformation           TEXT NOT NULL CHECK (length(transformation) BETWEEN 1 AND 200),
    tool_name                TEXT NOT NULL CHECK (length(tool_name) BETWEEN 1 AND 200),
    tool_version             TEXT NOT NULL CHECK (length(tool_version) BETWEEN 1 AND 200),
    normalized_options_json  JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- Deterministic hash of (parent, transformation, normalized options).
    -- Makes derivative creation idempotent: asking twice for the same
    -- conversion returns the existing child instead of burning CPU.
    command_fingerprint      TEXT NOT NULL CHECK (command_fingerprint ~ '^[0-9a-f]{64}$'),
    loss_character           TEXT NOT NULL DEFAULT 'unknown'
                             CHECK (loss_character IN (
                                 'bit_exact_repack', 'structurally_equivalent',
                                 'semantically_equivalent', 'lossy', 'unknown'
                             )),
    superseded               BOOLEAN NOT NULL DEFAULT FALSE,
    started_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at             TIMESTAMPTZ,

    -- A derivative is never its own parent.
    CONSTRAINT derivations_no_self_reference CHECK (parent_artifact_id <> child_artifact_id)
);

-- Unique derivation per normalized fingerprint where active. Superseded rows
-- stay for lineage but stop blocking a fresh run.
CREATE UNIQUE INDEX derivations_active_fingerprint_unique
    ON derivations (parent_artifact_id, command_fingerprint)
    WHERE superseded = FALSE;

CREATE INDEX derivations_child_idx ON derivations (child_artifact_id);

-- =============================================================================
-- Import.
-- =============================================================================

CREATE TABLE import_jobs (
    id                     UUID PRIMARY KEY,
    source_kind            TEXT NOT NULL
                           CHECK (source_kind IN (
                               'upload', 'watch_folder', 'http_url', 's3_object',
                               'local_path', 'qbittorrent_job', 'sabnzbd_job',
                               'nzbget_job', 'physical_drive', 'instance_peer'
                           )),
    -- Secrets are never stored here; the descriptor references them. It also
    -- never contains a post-processing command, which is what keeps an import
    -- from becoming code execution.
    source_descriptor_json JSONB NOT NULL,
    state                  TEXT NOT NULL DEFAULT 'requested'
                           CHECK (state IN (
                               'requested', 'acquiring', 'staged', 'hashing',
                               'inspecting', 'registering', 'complete',
                               'failed_retryable', 'failed_terminal',
                               'canceled', 'quarantined'
                           )),
    -- Where to resume, so a retry re-enters the last safe stage.
    checkpoint_json        JSONB,
    resume_stage           TEXT CHECK (resume_stage IS NULL OR resume_stage IN (
                               'requested', 'hashing', 'inspecting', 'registering'
                           )),
    bytes_expected         BIGINT CHECK (bytes_expected IS NULL OR bytes_expected >= 0),
    bytes_received         BIGINT NOT NULL DEFAULT 0 CHECK (bytes_received >= 0),
    artifact_id            UUID REFERENCES artifacts (id) ON DELETE SET NULL,
    error_code             TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail           TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 10000),
    lease_owner            TEXT CHECK (lease_owner IS NULL OR length(lease_owner) <= 200),
    lease_expires_at       TIMESTAMPTZ,
    created_by             TEXT NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at           TIMESTAMPTZ,

    -- A completed import has an artifact; an incomplete one does not claim to.
    CONSTRAINT import_jobs_complete_has_artifact CHECK (
        state <> 'complete' OR artifact_id IS NOT NULL
    ),
    -- A lease has an owner and an expiry, or neither.
    CONSTRAINT import_jobs_lease_is_whole CHECK (
        (lease_owner IS NULL) = (lease_expires_at IS NULL)
    )
);

-- Queue claims use SELECT ... FOR UPDATE SKIP LOCKED against this index.
CREATE INDEX import_jobs_claimable_idx ON import_jobs (created_at)
    WHERE state IN ('requested', 'failed_retryable');
CREATE INDEX import_jobs_state_idx ON import_jobs (state);
CREATE INDEX import_jobs_expired_lease_idx ON import_jobs (lease_expires_at)
    WHERE lease_expires_at IS NOT NULL;

-- =============================================================================
-- Workers and drives.
-- =============================================================================

CREATE TABLE workers (
    id                UUID PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 200),
    status            TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN (
                          'pending', 'online', 'offline', 'draining',
                          'revoked', 'incompatible'
                      )),
    software_version  TEXT NOT NULL,
    protocol_version  TEXT NOT NULL,
    capabilities_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- Hashed, never the credential itself.
    credential_hash   TEXT NOT NULL,
    last_seen_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at        TIMESTAMPTZ,

    CONSTRAINT workers_revoked_has_timestamp CHECK (
        (status = 'revoked') = (revoked_at IS NOT NULL)
    )
);

CREATE TABLE drives (
    id                UUID PRIMARY KEY,
    worker_id         UUID NOT NULL REFERENCES workers (id) ON DELETE RESTRICT,
    configured_name   TEXT NOT NULL CHECK (length(configured_name) BETWEEN 1 AND 200),
    -- Worker-local stable alias such as /dev/disc-block, never a host path.
    -- Drive identity is worker plus hardware, not the device node.
    device_alias      TEXT NOT NULL CHECK (length(device_alias) BETWEEN 1 AND 200),
    vendor            TEXT CHECK (vendor IS NULL OR length(vendor) <= 200),
    model             TEXT CHECK (model IS NULL OR length(model) <= 200),
    firmware          TEXT CHECK (firmware IS NULL OR length(firmware) <= 200),
    -- Hashed: a drive serial identifies hardware an operator may not wish to
    -- expose in an export or a support bundle.
    serial_hash       TEXT CHECK (serial_hash IS NULL OR serial_hash ~ '^[0-9a-f]{64}$'),
    capabilities_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    status            TEXT NOT NULL DEFAULT 'unknown'
                      CHECK (status IN (
                          'unknown', 'ready_empty', 'ready_with_media', 'busy',
                          'tray_open', 'missing', 'error', 'disabled'
                      )),
    last_media_json   JSONB,
    last_seen_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT drives_alias_unique_within_worker UNIQUE (worker_id, device_alias)
);

CREATE INDEX drives_worker_id_idx ON drives (worker_id);

-- =============================================================================
-- Burning.
-- =============================================================================

CREATE TABLE burn_jobs (
    id                       UUID PRIMARY KEY,
    disc_id                  UUID NOT NULL REFERENCES discs (id) ON DELETE RESTRICT,
    artifact_id              UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    requested_media_profile  TEXT CHECK (requested_media_profile IS NULL OR length(requested_media_profile) <= 100),
    -- Ordered list of verification steps. Read-back is required for ISO in
    -- the main flow, so an empty list is rejected: skipping verification must
    -- be a deliberate, expert-only act.
    --
    -- cardinality(), not array_length(): array_length returns NULL for an
    -- empty array, NULL >= 1 is NULL, and a CHECK only rejects on FALSE — so
    -- the obvious spelling would have silently permitted '{}'.
    verification_policy      TEXT[] NOT NULL DEFAULT '{full_sector_readback}'
                             CHECK (cardinality(verification_policy) >= 1),
    eject_policy             TEXT NOT NULL DEFAULT 'eject_on_success'
                             CHECK (eject_policy IN ('never', 'eject_on_success', 'always')),
    priority                 SMALLINT NOT NULL DEFAULT 0,
    state                    TEXT NOT NULL DEFAULT 'queued'
                             CHECK (state IN (
                                 'queued', 'leased', 'staging', 'waiting_for_media',
                                 'preflighting', 'ready', 'writing', 'finalizing',
                                 'verifying', 'complete', 'failed', 'canceled',
                                 'needs_attention'
                             )),
    -- Supports Idempotency-Key on burn creation, so a retried POST cannot
    -- silently burn a second disc.
    idempotency_key          TEXT UNIQUE CHECK (idempotency_key IS NULL OR length(idempotency_key) <= 200),
    created_by               TEXT NOT NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at             TIMESTAMPTZ
);

CREATE INDEX burn_jobs_claimable_idx ON burn_jobs (priority DESC, created_at)
    WHERE state = 'queued';
CREATE INDEX burn_jobs_state_idx ON burn_jobs (state);
CREATE INDEX burn_jobs_artifact_idx ON burn_jobs (artifact_id);

CREATE TABLE burn_attempts (
    id                UUID PRIMARY KEY,
    burn_job_id       UUID NOT NULL REFERENCES burn_jobs (id) ON DELETE RESTRICT,
    worker_id         UUID NOT NULL REFERENCES workers (id) ON DELETE RESTRICT,
    drive_id          UUID NOT NULL REFERENCES drives (id) ON DELETE RESTRICT,
    attempt_number    INTEGER NOT NULL CHECK (attempt_number > 0),
    state             TEXT NOT NULL DEFAULT 'claimed'
                      CHECK (state IN (
                          'claimed', 'staging', 'preflighting', 'writing',
                          'written', 'verifying', 'verified',
                          'verification_failed', 'write_failed',
                          'failed_before_write', 'canceled', 'interrupted'
                      )),
    lease_token_hash  TEXT NOT NULL,
    lease_expires_at  TIMESTAMPTZ NOT NULL,
    engine            TEXT NOT NULL CHECK (engine IN ('fake', 'xorriso', 'cdrdao')),
    engine_version    TEXT NOT NULL,
    plan_json         JSONB NOT NULL,
    medium_before_json JSONB,
    write_report_json  JSONB,
    verify_report_json JSONB,
    error_code        TEXT CHECK (error_code IS NULL OR length(error_code) <= 100),
    error_detail      TEXT CHECK (error_detail IS NULL OR length(error_detail) <= 10000),
    started_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at          TIMESTAMPTZ,

    -- Unique attempt number within a job.
    CONSTRAINT burn_attempts_number_unique UNIQUE (burn_job_id, attempt_number)
);

-- At most one active attempt per drive. This is the constraint that stops two
-- jobs writing to one drive, and it is enforced by the database rather than by
-- application care, because the consequence is a physically destroyed disc.
CREATE UNIQUE INDEX burn_attempts_one_active_per_drive
    ON burn_attempts (drive_id)
    WHERE state IN ('claimed', 'staging', 'preflighting', 'writing', 'written', 'verifying');

-- One active attempt per job, so a duplicate claim cannot start a second write
-- for the same intent.
CREATE UNIQUE INDEX burn_attempts_one_active_per_job
    ON burn_attempts (burn_job_id)
    WHERE state IN ('claimed', 'staging', 'preflighting', 'writing', 'written', 'verifying');

CREATE INDEX burn_attempts_job_idx ON burn_attempts (burn_job_id);
CREATE INDEX burn_attempts_expired_lease_idx ON burn_attempts (lease_expires_at)
    WHERE state IN ('claimed', 'staging', 'preflighting', 'writing', 'written', 'verifying');

CREATE TABLE burn_events (
    attempt_id        UUID NOT NULL REFERENCES burn_attempts (id) ON DELETE RESTRICT,
    -- Unique (attempt_id, sequence). This is what makes worker event
    -- submission idempotent: a replayed batch collides instead of duplicating.
    sequence          BIGINT NOT NULL CHECK (sequence >= 0),
    event_type        TEXT NOT NULL CHECK (length(event_type) BETWEEN 1 AND 100),
    stage             TEXT NOT NULL CHECK (length(stage) BETWEEN 1 AND 100),
    progress_fraction REAL CHECK (progress_fraction IS NULL OR progress_fraction BETWEEN 0 AND 1),
    message_code      TEXT NOT NULL CHECK (length(message_code) BETWEEN 1 AND 100),
    -- Bounded: raw tool output is retained separately with its own limits.
    data_json         JSONB NOT NULL DEFAULT '{}'::jsonb
                      CHECK (pg_column_size(data_json) <= 16384),
    worker_timestamp  TIMESTAMPTZ NOT NULL,
    server_received_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- No updated_at: events are immutable history.
    PRIMARY KEY (attempt_id, sequence)
);

CREATE TABLE physical_copies (
    id                 UUID PRIMARY KEY,
    disc_id            UUID NOT NULL REFERENCES discs (id) ON DELETE RESTRICT,
    artifact_id        UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    -- RESTRICT: the attempt is the evidence of how this disc was made.
    burn_attempt_id    UUID NOT NULL UNIQUE REFERENCES burn_attempts (id) ON DELETE RESTRICT,
    status             TEXT NOT NULL DEFAULT 'produced_unverified'
                       CHECK (status IN (
                           'produced_unverified', 'verified', 'verification_failed',
                           'degraded', 'lost', 'destroyed', 'unknown'
                       )),
    media_profile      TEXT NOT NULL CHECK (length(media_profile) BETWEEN 1 AND 100),
    manufacturer_id    TEXT CHECK (manufacturer_id IS NULL OR length(manufacturer_id) <= 200),
    media_serial       TEXT CHECK (media_serial IS NULL OR length(media_serial) <= 200),
    label              TEXT CHECK (label IS NULL OR length(label) <= 500),
    storage_location   TEXT CHECK (storage_location IS NULL OR length(storage_location) <= 500),
    write_speed        TEXT CHECK (write_speed IS NULL OR length(write_speed) <= 50),
    verification_level TEXT NOT NULL DEFAULT 'none'
                       CHECK (verification_level IN (
                           'none', 'tool_verify', 'filesystem_compare',
                           'full_sector_readback', 'track_hash_compare', 'provider_match'
                       )),
    verification_result TEXT NOT NULL DEFAULT 'not_performed'
                       CHECK (verification_result IN ('not_performed', 'passed', 'failed', 'partial')),
    -- Identity of the artifact as it was at burn time, retained so burn
    -- history stays meaningful after payload deletion.
    artifact_sha256_at_burn TEXT CHECK (artifact_sha256_at_burn IS NULL OR artifact_sha256_at_burn ~ '^[0-9a-f]{64}$'),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_checked_at    TIMESTAMPTZ,
    notes              TEXT CHECK (notes IS NULL OR length(notes) <= 10000)
);

CREATE INDEX physical_copies_disc_idx ON physical_copies (disc_id);
CREATE INDEX physical_copies_status_idx ON physical_copies (status);

-- No physical-copy row without a completed write stage. A trigger rather than
-- a CHECK because the condition lives on another table. Without it, a bug
-- could record a disc that was never burned.
CREATE FUNCTION require_completed_write() RETURNS TRIGGER AS $$
DECLARE
    attempt_state TEXT;
BEGIN
    SELECT state INTO attempt_state
    FROM burn_attempts
    WHERE id = NEW.burn_attempt_id;

    IF attempt_state IS NULL THEN
        RAISE EXCEPTION 'burn attempt % does not exist', NEW.burn_attempt_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;

    -- Every state that consumed media qualifies, including the failures: a
    -- ruined disc is still a disc and must be trackable so it can be
    -- destroyed rather than quietly reused.
    IF attempt_state NOT IN (
        'writing', 'written', 'verifying', 'verified',
        'verification_failed', 'write_failed', 'interrupted'
    ) THEN
        RAISE EXCEPTION
            'cannot record a physical copy for attempt % in state %: no media was written',
            NEW.burn_attempt_id, attempt_state
            USING ERRCODE = 'restrict_violation';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER physical_copies_require_write
    BEFORE INSERT ON physical_copies
    FOR EACH ROW EXECUTE FUNCTION require_completed_write();

-- =============================================================================
-- Integrations.
-- =============================================================================

CREATE TABLE integrations (
    id                 UUID PRIMARY KEY,
    kind               TEXT NOT NULL
                       CHECK (kind IN (
                           'romm', 'qbittorrent', 'sabnzbd', 'nzbget',
                           'metadata_provider', 'webhook', 's3', 'peer_instance'
                       )),
    name               TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 200),
    -- Disabled by default: a zero-provider deployment must be fully
    -- functional, which keeps metadata from becoming a prerequisite.
    enabled            BOOLEAN NOT NULL DEFAULT FALSE,
    configuration_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- A reference to a secret, never the secret.
    secret_ref         TEXT CHECK (secret_ref IS NULL OR length(secret_ref) <= 500),
    status             TEXT NOT NULL DEFAULT 'unconfigured'
                       CHECK (status IN ('unconfigured', 'ok', 'degraded', 'failing', 'disabled')),
    last_success_at    TIMESTAMPTZ,
    last_error_code    TEXT CHECK (last_error_code IS NULL OR length(last_error_code) <= 100),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE export_receipts (
    id                     UUID PRIMARY KEY,
    integration_id         UUID NOT NULL REFERENCES integrations (id) ON DELETE RESTRICT,
    artifact_id            UUID NOT NULL REFERENCES artifacts (id) ON DELETE RESTRICT,
    destination_key        TEXT NOT NULL CHECK (length(destination_key) BETWEEN 1 AND 2048),
    representation         TEXT NOT NULL CHECK (length(representation) BETWEEN 1 AND 100),
    materialization_method TEXT NOT NULL
                           CHECK (materialization_method IN ('copy', 'hardlink', 'symlink', 'reflink')),
    content_fingerprint    TEXT NOT NULL CHECK (content_fingerprint ~ '^[0-9a-f]{64}$'),
    state                  TEXT NOT NULL DEFAULT 'pending'
                           CHECK (state IN ('pending', 'present', 'stale', 'removed', 'failed')),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Reconciliation deletes only what this project created, identified by
    -- integration and destination. Without this, an export sweep could remove
    -- an operator's own unmanaged files.
    CONSTRAINT export_receipts_destination_unique UNIQUE (integration_id, destination_key)
);

CREATE INDEX export_receipts_artifact_idx ON export_receipts (artifact_id);

-- =============================================================================
-- Audit.
-- =============================================================================

-- Distinct from burn progress events: this records who did what, not how far
-- along a job is.
CREATE TABLE audit_events (
    id          UUID PRIMARY KEY,
    actor_type  TEXT NOT NULL CHECK (actor_type IN ('user', 'worker', 'integration', 'system')),
    actor_id    TEXT NOT NULL CHECK (length(actor_id) BETWEEN 1 AND 200),
    action      TEXT NOT NULL CHECK (length(action) BETWEEN 1 AND 200),
    target_type TEXT NOT NULL CHECK (length(target_type) BETWEEN 1 AND 100),
    target_id   TEXT CHECK (target_id IS NULL OR length(target_id) <= 200),
    request_id  TEXT CHECK (request_id IS NULL OR length(request_id) <= 200),
    outcome     TEXT NOT NULL CHECK (outcome IN ('success', 'failure', 'denied')),
    -- "Safe" metadata only: redaction happens before insert, and the bound
    -- stops an attacker inflating the audit log.
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb
                CHECK (pg_column_size(metadata) <= 16384),
    -- No updated_at: audit records are immutable.
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_created_at_idx ON audit_events (created_at DESC);
CREATE INDEX audit_events_target_idx ON audit_events (target_type, target_id);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_type, actor_id);

-- =============================================================================
-- updated_at maintenance.
-- =============================================================================

-- Set in the database rather than by each caller, so a repository that forgets
-- cannot leave a stale timestamp.
CREATE FUNCTION touch_updated_at() RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER titles_touch BEFORE UPDATE ON titles
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER editions_touch BEFORE UPDATE ON editions
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER disc_sets_touch BEFORE UPDATE ON disc_sets
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER discs_touch BEFORE UPDATE ON discs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER cas_objects_touch BEFORE UPDATE ON cas_objects
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER import_jobs_touch BEFORE UPDATE ON import_jobs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER workers_touch BEFORE UPDATE ON workers
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER drives_touch BEFORE UPDATE ON drives
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER burn_jobs_touch BEFORE UPDATE ON burn_jobs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER physical_copies_touch BEFORE UPDATE ON physical_copies
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER integrations_touch BEFORE UPDATE ON integrations
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER export_receipts_touch BEFORE UPDATE ON export_receipts
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
