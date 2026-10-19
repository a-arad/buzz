-- Durable owner for the post-commit dispatch of an accepted event: the
-- event_created audit entry, Redis publish, local fan-out and workflow runs.
-- Written in the event's insert transaction, deleted once every effect is done,
-- and re-dispatched by the recovery worker otherwise.
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
