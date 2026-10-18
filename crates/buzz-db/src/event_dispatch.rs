//! Durable post-commit dispatch obligations (`event_dispatch_queue`).
//!
//! An accepted event owes work that runs after its insert commits: the
//! `event_created` audit entry, the Redis publish, local fan-out and workflow
//! runs. A relay path that owes this work runs its event insert inside
//! [`DispatchObligation::scope`]. The insert then writes one queue row in the
//! same transaction (see [`enqueue_for_admitted_insert`]), so the obligation
//! commits or rolls back with the event, and a duplicate insert writes none.
//!
//! The relay deletes the row once every effect is done ([`complete_dispatch`]).
//! A row that a crash or a failed effect left behind becomes due after
//! [`DISPATCH_GRACE_SECS`] and is claimed by a recovery worker
//! ([`claim_due_dispatch_batch`]) under a claim id and lease, as
//! `push_match_queue` is. A row whose last allowed attempt fails is `held`: it
//! stays for an operator to inspect and rearm, and is never retried on its own.

use std::future::Future;

use buzz_core::{CommunityId, StoredEvent};
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row as _};
use uuid::Uuid;

use crate::error::Result;
use crate::Db;

/// Seconds after commit before the recovery worker may claim an obligation.
/// The in-process dispatch normally completes well inside this window.
pub const DISPATCH_GRACE_SECS: f64 = 30.0;

/// Recovery attempts before an obligation is held for an operator.
pub const MAX_DISPATCH_ATTEMPTS: i32 = 8;

tokio::task_local! {
    static DISPATCH_OBLIGATION: DispatchObligation;
}

/// The post-commit dispatch an event insert owes.
#[derive(Debug, Clone)]
pub struct DispatchObligation {
    event_id: [u8; 32],
    actor_pubkey: Option<Vec<u8>>,
}

impl DispatchObligation {
    /// An obligation to dispatch `event_id`, audited as `actor_pubkey`.
    pub fn new(event_id: [u8; 32], actor_pubkey: Option<Vec<u8>>) -> Self {
        Self {
            event_id,
            actor_pubkey,
        }
    }

    /// Run `insert` with this obligation in scope. Only an insert of this
    /// obligation's event id writes a queue row, so other events stored in the
    /// same scope (side-effect snapshots, for example) are unaffected.
    pub async fn scope<F: Future>(self, insert: F) -> F::Output {
        DISPATCH_OBLIGATION.scope(self, insert).await
    }
}

/// Write the queue row for a just-inserted event when its insert runs inside a
/// matching [`DispatchObligation::scope`]. Called by every event insert that a
/// dispatching relay path reaches, inside the insert transaction, after the
/// INSERT affected a row; an error rejects the event.
pub(crate) async fn enqueue_for_admitted_insert(
    conn: &mut PgConnection,
    community: CommunityId,
    event_id: &[u8],
) -> Result<()> {
    let Ok(Some(actor_pubkey)) = DISPATCH_OBLIGATION.try_with(|obligation| {
        (obligation.event_id.as_slice() == event_id).then(|| obligation.actor_pubkey.clone())
    }) else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO event_dispatch_queue (community_id, event_id, actor_pubkey, next_attempt_at) \
         VALUES ($1, $2, $3, now() + make_interval(secs => $4)) \
         ON CONFLICT DO NOTHING",
    )
    .bind(community.as_uuid())
    .bind(event_id)
    .bind(actor_pubkey)
    .bind(DISPATCH_GRACE_SECS)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One claimed obligation with its stored event.
#[derive(Debug, Clone)]
pub struct ClaimedDispatch {
    /// The event to dispatch.
    pub event: StoredEvent,
    /// Audit actor recorded at ingest.
    pub actor_pubkey: Option<Vec<u8>>,
    /// Attempt number of this claim (1-based).
    pub attempt: i32,
}

/// A fenced batch of due obligations from one community.
#[derive(Debug)]
pub struct ClaimedDispatchBatch {
    /// Community every obligation in the batch belongs to.
    pub community: CommunityId,
    /// Fence for [`retry_dispatch_batch`].
    pub claim_id: Uuid,
    /// Obligations whose events are still stored.
    pub jobs: Vec<ClaimedDispatch>,
}

/// Claim due obligations from one community: pending rows past their
/// `next_attempt_at`, and claims whose lease expired. Obligations for events
/// that were deleted since they were stored are dropped, never dispatched.
pub async fn claim_due_dispatch_batch(
    pool: &PgPool,
    limit: i64,
    lease_until: DateTime<Utc>,
) -> Result<Option<ClaimedDispatchBatch>> {
    let claim_id = Uuid::new_v4();
    let mut connection = pool.acquire().await?;
    let rows = sqlx::query(
        r#"
        WITH target AS (
            SELECT community_id
            FROM event_dispatch_queue
            WHERE attempts < $3
              AND next_attempt_at <= now()
              AND (state = 'pending' OR (state = 'dispatching' AND lease_until < now()))
            ORDER BY next_attempt_at, created_at
            LIMIT 1
        ),
        candidates AS (
            SELECT q.community_id, q.event_id
            FROM event_dispatch_queue q
            JOIN target t ON q.community_id = t.community_id
            WHERE q.attempts < $3
              AND q.next_attempt_at <= now()
              AND (q.state = 'pending' OR (q.state = 'dispatching' AND q.lease_until < now()))
            ORDER BY q.next_attempt_at, q.created_at
            FOR UPDATE OF q SKIP LOCKED
            LIMIT $4
        )
        UPDATE event_dispatch_queue q
        SET state = 'dispatching', claim_id = $1, lease_until = $2, attempts = q.attempts + 1
        FROM candidates c
        WHERE q.community_id = c.community_id AND q.event_id = c.event_id
        RETURNING q.community_id, q.event_id, q.actor_pubkey, q.attempts
        "#,
    )
    .bind(claim_id)
    .bind(lease_until)
    .bind(MAX_DISPATCH_ATTEMPTS)
    .bind(limit)
    .fetch_all(&mut *connection)
    .await?;
    // Release the claim connection before the event load acquires its own,
    // as the push matcher does, so a size-one writer pool cannot self-starve.
    drop(connection);
    if rows.is_empty() {
        return Ok(None);
    }
    let community = CommunityId::from_uuid(rows[0].try_get("community_id")?);
    let mut claimed = std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let event_id: Vec<u8> = row.try_get("event_id")?;
        let actor_pubkey: Option<Vec<u8>> = row.try_get("actor_pubkey")?;
        let attempt: i32 = row.try_get("attempts")?;
        claimed.insert(event_id, (actor_pubkey, attempt));
    }
    let ids: Vec<&[u8]> = claimed.keys().map(Vec::as_slice).collect();
    let events = crate::event::get_events_by_ids(pool, community, &ids).await?;
    let mut jobs = Vec::with_capacity(events.len());
    for event in events {
        if let Some((actor_pubkey, attempt)) = claimed.remove(event.event.id.as_bytes().as_slice())
        {
            jobs.push(ClaimedDispatch {
                event,
                actor_pubkey,
                attempt,
            });
        }
    }
    let gone: Vec<Vec<u8>> = claimed.into_keys().collect();
    if !gone.is_empty() {
        complete_dispatch(pool, community, &gone).await?;
    }
    if jobs.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClaimedDispatchBatch {
        community,
        claim_id,
        jobs,
    }))
}

/// Delete obligations whose dispatch finished. Not fenced by claim: a finished
/// dispatch discharges the obligation whichever owner ran it, and a later
/// retry or completion of a concurrent claim then affects no row.
pub async fn complete_dispatch(
    pool: &PgPool,
    community: CommunityId,
    event_ids: &[Vec<u8>],
) -> Result<u64> {
    if event_ids.is_empty() {
        return Ok(0);
    }
    let mut connection = pool.acquire().await?;
    Ok(sqlx::query(
        "DELETE FROM event_dispatch_queue WHERE community_id = $1 AND event_id = ANY($2)",
    )
    .bind(community.as_uuid())
    .bind(event_ids)
    .execute(&mut *connection)
    .await?
    .rows_affected())
}

/// Release a failed claim. The obligation becomes due again after an
/// exponential backoff (5 s doubling, capped at 10 minutes), or is held when
/// this was its last allowed attempt. Returns the event ids that were held.
pub async fn retry_dispatch_batch(
    pool: &PgPool,
    community: CommunityId,
    claim_id: Uuid,
    failures: &[(Vec<u8>, String)],
) -> Result<Vec<Vec<u8>>> {
    if failures.is_empty() {
        return Ok(Vec::new());
    }
    let (event_ids, errors): (Vec<Vec<u8>>, Vec<String>) = failures.iter().cloned().unzip();
    let mut connection = pool.acquire().await?;
    let rows = sqlx::query(
        r#"
        UPDATE event_dispatch_queue q
        SET state = CASE WHEN q.attempts >= $4 THEN 'held' ELSE 'pending' END,
            claim_id = NULL,
            lease_until = NULL,
            next_attempt_at = now()
                + least(interval '10 minutes', interval '5 seconds' * power(2, q.attempts - 1)),
            last_error = f.error
        FROM unnest($3::bytea[], $5::text[]) AS f(event_id, error)
        WHERE q.community_id = $1 AND q.claim_id = $2 AND q.state = 'dispatching'
          AND q.event_id = f.event_id
        RETURNING q.event_id, q.state
        "#,
    )
    .bind(community.as_uuid())
    .bind(claim_id)
    .bind(&event_ids)
    .bind(MAX_DISPATCH_ATTEMPTS)
    .bind(&errors)
    .fetch_all(&mut *connection)
    .await?;
    let mut held = Vec::new();
    for row in rows {
        if row.try_get::<String, _>("state")? == "held" {
            held.push(row.try_get("event_id")?);
        }
    }
    Ok(held)
}

/// Hold obligations whose final attempt lost its worker: the lease expired and
/// no attempt remains, so the claim query will never pick them up again.
pub async fn hold_abandoned_dispatches(pool: &PgPool) -> Result<u64> {
    let mut connection = pool.acquire().await?;
    Ok(sqlx::query(
        "UPDATE event_dispatch_queue \
         SET state = 'held', claim_id = NULL, lease_until = NULL, \
             last_error = coalesce(last_error, 'worker lost on final attempt') \
         WHERE state = 'dispatching' AND lease_until < now() AND attempts >= $1",
    )
    .bind(MAX_DISPATCH_ATTEMPTS)
    .execute(&mut *connection)
    .await?
    .rows_affected())
}

/// Row counts by state, for the queue gauges.
pub async fn dispatch_queue_counts(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let mut connection = pool.acquire().await?;
    let rows = sqlx::query("SELECT state, count(*) AS n FROM event_dispatch_queue GROUP BY state")
        .fetch_all(&mut *connection)
        .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("state")?, row.try_get("n")?)))
        .collect()
}

impl Db {
    /// Claim due dispatch obligations from one community.
    pub async fn claim_due_dispatch_batch(
        &self,
        limit: i64,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<ClaimedDispatchBatch>> {
        claim_due_dispatch_batch(&self.pool, limit, lease_until).await
    }

    /// Delete dispatch obligations whose dispatch finished.
    pub async fn complete_dispatch(
        &self,
        community: CommunityId,
        event_ids: &[Vec<u8>],
    ) -> Result<u64> {
        complete_dispatch(&self.pool, community, event_ids).await
    }

    /// Release failed dispatch claims for retry, holding exhausted ones.
    pub async fn retry_dispatch_batch(
        &self,
        community: CommunityId,
        claim_id: Uuid,
        failures: &[(Vec<u8>, String)],
    ) -> Result<Vec<Vec<u8>>> {
        retry_dispatch_batch(&self.pool, community, claim_id, failures).await
    }

    /// Hold obligations whose final attempt lost its worker.
    pub async fn hold_abandoned_dispatches(&self) -> Result<u64> {
        hold_abandoned_dispatches(&self.pool).await
    }

    /// Dispatch queue row counts by state.
    pub async fn dispatch_queue_counts(&self) -> Result<Vec<(String, i64)>> {
        dispatch_queue_counts(&self.pool).await
    }
}
