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
  workflow run was claimed. If any step fails, the row stays.
- Every relay runs a recovery worker. It claims rows 30 seconds after commit,
  or when a claim's 60-second lease has expired, and runs the same dispatch
  again. Visibility and membership are looked up fresh.
- A failed attempt backs off (5 s, doubling, capped at 10 minutes). After 8
  attempts the row is **held**: kept for an operator, never retried on its own.
  Rows whose event was deleted are dropped without dispatch.

Repeated dispatch is safe per effect, not exactly-once as a whole:

| Effect | Guarantee | Mechanism |
|---|---|---|
| `event_created` audit entry | at most one per event | checked under the per-community audit lock, served by `idx_audit_log_event_created_object` |
| workflow run | at most one per (workflow, event) | unique `idx_workflow_runs_trigger_event`; creation returns the existing run |
| workflow actions | at most once per run | a run's actions start only after an atomic `pending` → `running` claim |
| Redis publish, local fan-out | at least once while the obligation is pending | clients deduplicate by event id |

A run that crashed after its claim stays `running`; its actions may have
happened, so nothing re-executes it. Recovered fan-out still filters
subscriptions on `since` against the event's `created_at`.

### Workflows on a recovered event

Workflows are matched, and their definition and owner authority checked, when
a run starts. For a recovered event that is recovery time: an event recovered
after a workflow was edited runs the edited definition, a disabled workflow
does not fire, and a workflow whose owner lost authority is skipped. A run that
an earlier dispatch reserved but never started is executed if its workflow
still matches and is cancelled (`error_code = 'not_selected_on_recovery'`)
otherwise. A failed authority lookup is retried, not treated as a denial.

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

`last_error` names the failed stage (`publish`, `audit`, `workflow`,
`serialize`). Fix the cause, then rearm. The next worker poll dispatches the
rows again:

```sql
UPDATE event_dispatch_queue
SET state = 'pending', attempts = 0, next_attempt_at = now(), last_error = NULL
WHERE state = 'held'
  AND community_id = '<community uuid>'
  AND event_id = decode('<event id hex>', 'hex');
```

Deleting a held row abandons that event's remaining effects.

## Deploying

The relay must not serve before 0058 is applied: event inserts write the queue
table, and run creation relies on the new unique index. With
`BUZZ_AUTO_MIGRATE=true` the relay applies it at startup.

0058 bounds lock waits to 1 second and execution to 5 seconds. It builds
`idx_audit_log_event_created_object` unless the index already exists. A plain
build blocks audit writes (not event ingest) while it runs: about 2.2 seconds
for 500,000 `audit_log` rows on a laptop. On a large or busy database, prebuild
the index first:

```sql
CREATE INDEX CONCURRENTLY idx_audit_log_event_created_object
    ON public.audit_log (community_id, object_id) WHERE action = 'event_created';
```

0058 then only verifies that the index is valid and has this exact definition.
It also fails if `workflow_runs` already holds two runs of one workflow for the
same trigger event.

## Rolling back

A relay binary without 0058 in its migration set refuses to start with
`BUZZ_AUTO_MIGRATE=true`: sqlx reports the applied version as missing. Roll back
the binary with `BUZZ_AUTO_MIGRATE=false`, which skips the migrator. Do not
delete the `_sqlx_migrations` row or restore an older database.

The older binary ignores the queue table and both indexes. It writes no
obligations and leaves existing rows alone. Rows still in the table when the
newer binary returns are dispatched then, so their workflows fire late. Inspect
them before rolling forward and delete any you do not want delivered.
