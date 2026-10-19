# Event dispatch recovery

An accepted event owes work that runs after its insert commits: the
`event_created` audit entry, the Redis publish, local fan-out and workflow runs.
`OK` from the relay still means the event is stored, not that this work ran.
Migration `0058_event_dispatch_queue.sql` gives that work a durable owner.

## Contract

- Client ingest (the generic and reaction paths) and workflow `send_message`
  write one `event_dispatch_queue` row in the event's own insert transaction. A
  rolled-back or duplicate insert writes none. Existing events get no row, so
  nothing historical is replayed.
- The relay's in-process dispatch deletes the row once the audit entry is
  recorded, Redis publish succeeded, local fan-out ran and every matching
  workflow run was claimed. If any step fails, the row stays. Local fan-out
  fails when a visibility or membership lookup fails for a recipient: that
  recipient gets nothing (delivery still fails closed) and the row stays for a
  later pass. A recipient denied access, or a deleted channel, is not a
  failure. The later pass may run on another relay; it reaches this relay's
  subscribers only through Redis (see the limits below).
- Every relay runs a recovery worker. It claims rows 30 seconds after commit,
  or when a claim's 60-second lease has expired, and runs the same dispatch
  again. Visibility and membership are looked up fresh.
- A failed attempt backs off (5 s, doubling, capped at 10 minutes). After 8
  attempts, about 11 minutes after commit, the row is **held**: kept for an
  operator, never retried on its own. Rows whose event was deleted are dropped
  without dispatch. Rows of an archived community are retried like any other
  failure (`community not served`) and end up held; rearm them if the
  community is unarchived, or delete them.
- Each attempt runs every effect again; there is no per-effect progress. A
  recovery attempt re-queues the audit entry only when it is not recorded yet.

Repeated dispatch is safe per effect, not exactly-once as a whole:

| Effect | Guarantee | Mechanism |
|---|---|---|
| `event_created` audit entry | at most one per event | checked under the per-community audit lock, served by `idx_audit_log_event_created_object` |
| workflow run | at most one per (workflow, event) | unique `idx_workflow_runs_trigger_event`; creation returns the existing run |
| workflow actions | at most once per run | a run's actions start only after an atomic `pending` → `running` claim |
| Redis publish, local fan-out | at least once while the obligation is pending | clients deduplicate by event id |

Known limits:

- A run that crashed after its claim stays `running`; its actions may have
  happened, so nothing re-executes it.
- A successful Redis publish is not an acknowledgment from other relays. A
  replica that receives the event over Redis runs its own access lookups, and
  a failure there drops its local recipients without reaching this
  obligation.
- Recovered fan-out still filters subscriptions on `since` against the
  event's `created_at`.
- A run reserved before a crash stays `pending` if its obligation is dropped
  (event deleted) or held.

### Workflows on a recovered event

Workflows are matched, and their definition and owner authority checked, when
a run starts. For a recovered event that is recovery time: an event recovered
after a workflow was edited runs the edited definition, a disabled workflow
does not fire, and a workflow whose owner lost authority is skipped. A run that
an earlier dispatch reserved but never started is executed if its workflow
still matches and is cancelled otherwise; its `error_message` starts with
`not_selected_on_recovery` (this line has no `error_code` column). A failed authority lookup is retried, not treated as a denial.

A recovery pass reads the channel's workflows from the database, not from the
relay's 10-second workflow cache, which other replicas do not invalidate. It
reads them once, before it claims any run. An edit that commits after that read
is not seen by that pass: a run it starts uses the definition it read. Every
pass that starts after the edit commits sees it. A run that has started is
never restarted. The trigger event and its trigger context are the same on
every pass. Live dispatch still uses the cache, so a live event can match a
definition up to 10 seconds old, as before.

## Operating

Metrics:

- `buzz_event_dispatch_queue_rows{state}`: rows by `pending`, `dispatching`,
  `held`, refreshed every 30 seconds by each relay. Alert on `held > 0`.
- `buzz_event_dispatch_held_total`, `buzz_event_dispatch_recovered_total`,
  `buzz_event_dispatch_retry_total{stage}`,
  `buzz_post_commit_dispatch_incomplete_total{stage}`.

Inspect held obligations:

```sql
SELECT community_id, encode(event_id, 'hex') AS event_id, attempts, last_error,
       created_at
FROM event_dispatch_queue
WHERE state = 'held'
ORDER BY created_at;
```

`last_error` names the failed stage (`publish`, `fanout_access`, `audit`,
`workflow`, `serialize`). Fix the cause, then rearm. The next worker poll dispatches the
rows again:

```sql
UPDATE event_dispatch_queue
SET state = 'pending', attempts = 0, next_attempt_at = now(), last_error = NULL
WHERE state = 'held'
  AND community_id = '<community uuid>'
  AND event_id = decode('<event id hex>', 'hex');
```

To rearm everything held for one cause after fixing it:

```sql
UPDATE event_dispatch_queue
SET state = 'pending', attempts = 0, next_attempt_at = now(), last_error = NULL
WHERE state = 'held' AND last_error = 'publish failed';
```

Deleting a held row abandons that event's remaining effects.

## Deploying

The relay must not serve before 0058 is applied: event inserts write the queue
table, and run creation relies on the new unique index. With
`BUZZ_AUTO_MIGRATE=true` the relay applies it at startup.

0058 bounds lock waits to 1 second and each index build to 5 seconds. It builds
two indexes unless they already exist:

- `idx_audit_log_event_created_object`. A plain build blocks audit writes (not
  event ingest) while it runs: about 2.2 seconds for 500,000 `audit_log` rows on
  a laptop.
- `idx_workflow_runs_trigger_event`, unique. It cannot be built while
  `workflow_runs` holds two runs of one workflow for the same trigger event:

  ```sql
  SELECT community_id, encode(trigger_event_id, 'hex'), workflow_id, count(*)
  FROM workflow_runs WHERE trigger_event_id IS NOT NULL
  GROUP BY 1, 2, 3 HAVING count(*) > 1;
  ```

On a large or busy database, prebuild them first:

```sql
CREATE INDEX CONCURRENTLY idx_audit_log_event_created_object
    ON public.audit_log (community_id, object_id) WHERE action = 'event_created';
CREATE UNIQUE INDEX CONCURRENTLY idx_workflow_runs_trigger_event
    ON public.workflow_runs (community_id, trigger_event_id, workflow_id)
    WHERE trigger_event_id IS NOT NULL;
```

0058 then only verifies that each index is valid and has exactly this
definition. If a bound is hit, the relay exits, 0058 stays unapplied, and the
next start retries it.

## Upgrading to the main line

This file ships unchanged as `0058` on main, so a database that applied it here
keeps a matching checksum. Main's `0029` fences every table with a
`community_id` column, including `event_dispatch_queue`; main's `0059` attaches
the same fence again, which is a no-op.

## Rolling back

A relay binary without 0058 in its migration set refuses to start with
`BUZZ_AUTO_MIGRATE=true`: sqlx reports the applied version as missing. Roll back
the binary with `BUZZ_AUTO_MIGRATE=false`, which skips the migrator. Do not
delete the `_sqlx_migrations` row or restore an older database.

The older binary ignores the queue table and both indexes. It writes no
obligations and leaves existing rows alone. When a newer binary returns, its
worker can claim due pending rows and expired leases immediately; held rows
are excluded. Inspect retained work **before** starting any newer binary.
Delayed dispatch uses the workflow-selection rules above, so review the
current definition, authority, age and prior effects before approving it.

### Hold retained work before roll-forward

Only use this procedure after rollback has stopped **every** newer relay
process, including old replicas of the newer image and their in-flight tasks.
Verify that only the old, queue-unaware binaries serve, with migrations
disabled. Lease expiry alone does not prove a worker stopped. A row update
cannot cancel dispatch already in flight: successful dispatch deletes its
obligation without checking the claim id. If worker quiescence is unknown,
do not change its claim or start roll-forward.

Use a SQL client that stops on errors (for psql, `ON_ERROR_STOP=1`).
For each unapproved pending or leased obligation, save its complete row and
related workflow-run states in a private operator receipt. Then hold the exact
community/event pair. Use the same values in all three statements below. This
transaction fails if the row is absent or already held; inspect that result
instead of broadening the selection. Existing held rows need no update.

```sql
BEGIN;
SET LOCAL lock_timeout = '1s';
SET LOCAL statement_timeout = '5s';
SELECT * FROM event_dispatch_queue
WHERE community_id = '<community uuid>'
  AND event_id = decode('<event id hex>', 'hex')
FOR UPDATE;
DO $hold$
DECLARE affected bigint;
BEGIN
    UPDATE event_dispatch_queue
    SET state = 'held', claim_id = NULL, lease_until = NULL,
        last_error = 'operator hold: rollback review'
    WHERE state IN ('pending', 'dispatching')
      AND community_id = '<community uuid>'
      AND event_id = decode('<event id hex>', 'hex');
    GET DIAGNOSTICS affected = ROW_COUNT;
    IF affected <> 1 THEN
        RAISE EXCEPTION 'expected one pending/dispatching obligation, got %', affected;
    END IF;
END
$hold$;
SELECT * FROM event_dispatch_queue
WHERE community_id = '<community uuid>'
  AND event_id = decode('<event id hex>', 'hex');
COMMIT;
```

Retain the before/after rows and verify the transaction committed. Attempts,
timestamps, event identity and actor remain unchanged; the prior error and
claim are preserved in the operator receipt. The distinct hold reason keeps
the row out of cause-specific rearm sweeps. It counts in the held queue gauge;
record its owner and review requirement without silencing unrelated holds.

Before roll-forward, reconcile the inventory: every unapproved pending or
leased row must now be held, and every row left eligible must have an explicit
dispatch decision. Keep uncertain workflow runs unchanged; holding a dispatch
row does not cancel, reset or prove the outcome of a workflow run.

After approval, use the single-event rearm statement above, adding
`AND last_error = 'operator hold: rollback review'`. Require one affected row
and record the approval and resulting row. Do not rearm all held work as part
of roll-forward. Do not delete an obligation as routine cancellation: deletion
abandons remaining effects and removes the queue evidence.
