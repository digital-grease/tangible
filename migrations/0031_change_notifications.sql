-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Change notifications for the event stream (API.md 12,
-- docs/plans/event-stream.md). Each table whose changes the web UI shows live
-- notifies channel `tangible_events` with a small JSON document: a topic, an
-- event type, and the row's identifiers and state. PostgreSQL delivers a
-- notification only when its transaction commits, so a listener never hears
-- of a change that was rolled back, and hears of every one that was made,
-- whichever code path made it.
--
-- Only the columns named are sent: identifiers, states, stages, progress and
-- message codes. Never a credential, a hash of one, a path or a manifest.
--
-- Additive: the previous version listens to nothing, and a notification
-- nobody listens for costs next to nothing.

-- Notify with the topic and type given as the first two arguments and the
-- columns named by the rest.
CREATE FUNCTION tangible_notify_change() RETURNS TRIGGER AS $$
DECLARE
    whole JSONB := to_jsonb(NEW);
    data  JSONB := '{}'::jsonb;
    i     INTEGER;
BEGIN
    FOR i IN 2 .. TG_NARGS - 1 LOOP
        data := data || jsonb_build_object(TG_ARGV[i], whole -> TG_ARGV[i]);
    END LOOP;
    PERFORM pg_notify(
        'tangible_events',
        jsonb_build_object('topic', TG_ARGV[0], 'type', TG_ARGV[1], 'data', data)::text
    );
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- A burn's progress, with the job it belongs to, which the event alone does
-- not name.
CREATE FUNCTION tangible_notify_burn_event() RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'tangible_events',
        jsonb_build_object(
            'topic', 'burns',
            'type', 'burn.progress',
            'data', jsonb_build_object(
                'attempt_id', NEW.attempt_id,
                'burn_job_id', (SELECT burn_job_id FROM burn_attempts WHERE id = NEW.attempt_id),
                'sequence', NEW.sequence,
                'stage', NEW.stage,
                'progress', NEW.progress_fraction,
                'message_code', NEW.message_code
            )
        )::text
    );
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- A rip's progress, in the same shape as a burn's.
CREATE FUNCTION tangible_notify_rip_event() RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'tangible_events',
        jsonb_build_object(
            'topic', 'rips',
            'type', 'rip.progress',
            'data', jsonb_build_object(
                'rip_id', NEW.rip_id,
                'sequence', NEW.sequence,
                'progress', NEW.progress_fraction,
                'message_code', NEW.message_code
            )
        )::text
    );
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER burn_jobs_notify AFTER INSERT OR UPDATE ON burn_jobs
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change('burns', 'burn_job.changed', 'id', 'state');
CREATE TRIGGER burn_attempts_notify AFTER INSERT OR UPDATE ON burn_attempts
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change(
        'burns', 'burn_attempt.changed', 'id', 'burn_job_id', 'state');
CREATE TRIGGER burn_events_notify AFTER INSERT ON burn_events
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_burn_event();

CREATE TRIGGER import_jobs_notify AFTER INSERT OR UPDATE ON import_jobs
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change('imports', 'import.changed', 'id', 'state');

CREATE TRIGGER rip_jobs_notify AFTER INSERT OR UPDATE ON rip_jobs
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change(
        'rips', 'rip.changed', 'id', 'drive_id', 'state');
CREATE TRIGGER rip_events_notify AFTER INSERT ON rip_events
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_rip_event();

CREATE TRIGGER media_erasures_notify AFTER INSERT OR UPDATE ON media_erasures
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change(
        'erasures', 'erasure.changed', 'id', 'drive_id', 'state');

CREATE TRIGGER derivation_jobs_notify AFTER INSERT OR UPDATE ON derivation_jobs
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change(
        'derivations', 'derivation.changed', 'id', 'parent_artifact_id', 'state');

CREATE TRIGGER artifacts_notify AFTER INSERT OR UPDATE ON artifacts
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change('library', 'artifact.changed', 'id');

-- A worker's heartbeat updates its row every twenty seconds; only a change
-- of status is news.
CREATE TRIGGER workers_notify_insert AFTER INSERT ON workers
    FOR EACH ROW EXECUTE FUNCTION tangible_notify_change('workers', 'worker.changed', 'id', 'status');
CREATE TRIGGER workers_notify_update AFTER UPDATE ON workers
    FOR EACH ROW WHEN (
        OLD.status IS DISTINCT FROM NEW.status
        OR OLD.credential_rotation_requested_at IS DISTINCT FROM NEW.credential_rotation_requested_at
        OR OLD.credential_issued_at IS DISTINCT FROM NEW.credential_issued_at
    )
    EXECUTE FUNCTION tangible_notify_change('workers', 'worker.changed', 'id', 'status');
