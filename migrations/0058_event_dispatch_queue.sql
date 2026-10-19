-- Durable owner for the post-commit dispatch of an accepted event: the
-- event_created audit entry, Redis publish, local fan-out and workflow runs.
-- The relay writes one row in the event's own insert transaction and deletes it
-- once every effect is done. A recovery worker re-runs dispatch for rows that a
-- crash or a failed effect left behind. Duplicate and rolled-back inserts write
-- no row, and nothing is backfilled, so historical events are never replayed.
--
-- The 0.2.x stable line ships this file unchanged, so it must stay valid on a
-- 0028 schema: no fence, deletion or hash_version references. Main attaches the
-- community write fence in 0059.
SET LOCAL lock_timeout = '1s';
SET LOCAL statement_timeout = '5s';

CREATE TABLE event_dispatch_queue (
    community_id UUID NOT NULL REFERENCES communities(id),
    event_id BYTEA NOT NULL CHECK (length(event_id) = 32),
    -- Audit actor resolved at ingest: the authenticated principal, or the
    -- triggering user for workflow posts. Not derivable from the event.
    actor_pubkey BYTEA,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'dispatching', 'held')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TIMESTAMPTZ NOT NULL,
    lease_until TIMESTAMPTZ,
    claim_id UUID,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, event_id)
);
CREATE INDEX event_dispatch_queue_due
    ON event_dispatch_queue (next_attempt_at) WHERE state = 'pending';
CREATE INDEX event_dispatch_queue_recovery
    ON event_dispatch_queue (lease_until) WHERE state = 'dispatching';

-- The two indexes below follow 0049: startup is bounded, so a large or busy
-- table fails deployment rather than blocking writes for an unbounded build.
-- Operators can prebuild either index CONCURRENTLY with the same definition;
-- the migration then only verifies it. See docs/event-dispatch-recovery.md.

-- At most one run per (workflow, triggering event), so repeated dispatch cannot
-- start a workflow twice. Run creation resolves conflicts on this index. The
-- build fails if the table already holds such duplicates.
DO $$
BEGIN
    IF to_regclass('public.idx_workflow_runs_trigger_event') IS NULL THEN
        CREATE UNIQUE INDEX idx_workflow_runs_trigger_event
            ON public.workflow_runs (community_id, trigger_event_id, workflow_id)
            WHERE trigger_event_id IS NOT NULL;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relname = 'idx_workflow_runs_trigger_event'
          AND i.indisvalid AND i.indisready AND i.indislive
          AND pg_get_indexdef(i.indexrelid) =
              'CREATE UNIQUE INDEX idx_workflow_runs_trigger_event ON public.workflow_runs USING btree (community_id, trigger_event_id, workflow_id) WHERE (trigger_event_id IS NOT NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_workflow_runs_trigger_event invalid or wrong definition; see docs/event-dispatch-recovery.md';
    END IF;
END $$;

-- At most one event_created audit entry per event: the audit writer checks this
-- index under its per-community lock.
DO $$
BEGIN
    IF to_regclass('public.idx_audit_log_event_created_object') IS NULL THEN
        CREATE INDEX idx_audit_log_event_created_object
            ON public.audit_log (community_id, object_id) WHERE action = 'event_created';
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relname = 'idx_audit_log_event_created_object'
          AND i.indisvalid AND i.indisready AND i.indislive
          AND pg_get_indexdef(i.indexrelid) =
              'CREATE INDEX idx_audit_log_event_created_object ON public.audit_log USING btree (community_id, object_id) WHERE ((action)::text = ''event_created''::text)'
    ) THEN
        RAISE EXCEPTION 'idx_audit_log_event_created_object invalid or wrong definition; see docs/event-dispatch-recovery.md';
    END IF;
END $$;
