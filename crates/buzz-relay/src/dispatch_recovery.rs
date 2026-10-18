//! Recovery worker for post-commit dispatch obligations.
//!
//! Every relay replica runs [`run`]. It claims obligations in
//! `event_dispatch_queue` that the in-process dispatch did not discharge — the
//! relay crashed after the event committed, or an effect failed — and runs the
//! dispatch again through the same path. Each effect tolerates a repeat: the
//! audit entry is recorded once, a workflow run starts once per (workflow,
//! event), and live delivery may resend an event that clients deduplicate by
//! id. Membership and channel visibility are looked up fresh.
//!
//! A failed attempt backs off; after the last allowed attempt the obligation
//! is held for an operator (see `docs/event-dispatch-recovery.md`).

use std::{sync::Arc, time::Duration};

use chrono::{TimeDelta, Utc};
use futures_util::future::join_all;
use tracing::{error, warn};

use buzz_core::kind::event_kind_u32;

use crate::handlers::event::{dispatch_persistent_event_inner, AuditStep};
use crate::state::AppState;

/// Claim lease. A pass waits at most 20 s for its audit receipt, so this
/// leaves room for publish and workflow start before another replica may
/// reclaim the obligation.
const LEASE_SECS: i64 = 60;
/// Upper bound on one claimed batch.
const BATCH_LIMIT: i64 = 32;
const IDLE_POLL_FLOOR: Duration = Duration::from_millis(500);
const IDLE_POLL_CEILING: Duration = Duration::from_secs(5);
/// Cadence of the held-obligation sweep and the queue gauges.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Claim and re-dispatch due obligations until the process exits.
pub async fn run(state: Arc<AppState>) {
    let mut idle_delay = IDLE_POLL_FLOOR;
    let mut last_sweep: Option<tokio::time::Instant> = None;
    loop {
        if last_sweep.is_none_or(|at| at.elapsed() >= SWEEP_INTERVAL) {
            sweep(&state).await;
            last_sweep = Some(tokio::time::Instant::now());
        }
        let lease_until = Utc::now() + TimeDelta::seconds(LEASE_SECS);
        match state
            .db
            .claim_due_dispatch_batch(BATCH_LIMIT, lease_until)
            .await
        {
            Ok(Some(batch)) => {
                idle_delay = IDLE_POLL_FLOOR;
                process_batch(&state, batch).await;
            }
            Ok(None) => {
                tokio::time::sleep(idle_delay).await;
                idle_delay = (idle_delay * 2).min(IDLE_POLL_CEILING);
            }
            Err(e) => {
                error!("dispatch recovery claim failed: {e}");
                tokio::time::sleep(IDLE_POLL_CEILING).await;
            }
        }
    }
}

/// Hold obligations whose final attempt lost its worker, and publish the
/// queue size by state.
async fn sweep(state: &AppState) {
    match state.db.hold_abandoned_dispatches().await {
        Ok(0) => {}
        Ok(held) => {
            metrics::counter!("buzz_event_dispatch_held_total").increment(held);
            error!(
                held,
                "dispatch obligations held after their final attempt was abandoned"
            );
        }
        Err(e) => error!("dispatch obligation hold sweep failed: {e}"),
    }
    match state.db.dispatch_queue_counts().await {
        Ok(counts) => {
            for queue_state in ["pending", "dispatching", "held"] {
                let rows = counts
                    .iter()
                    .find(|(s, _)| s == queue_state)
                    .map_or(0, |(_, n)| *n);
                metrics::gauge!("buzz_event_dispatch_queue_rows", "state" => queue_state)
                    .set(rows as f64);
            }
        }
        Err(e) => warn!("dispatch queue count failed: {e}"),
    }
}

/// Re-dispatch one claimed batch, then discharge or release each obligation.
pub(crate) async fn process_batch(
    state: &Arc<AppState>,
    batch: buzz_db::event_dispatch::ClaimedDispatchBatch,
) {
    let community = batch.community;
    let ids = || -> Vec<Vec<u8>> {
        batch
            .jobs
            .iter()
            .map(|job| job.event.event.id.as_bytes().to_vec())
            .collect()
    };
    let tenant = match state.db.lookup_community_host(community).await {
        Ok(Some(host)) => buzz_core::tenant::TenantContext::resolved(community, host),
        Ok(None) => {
            // The community is archived or gone: nobody can receive these.
            warn!(%community, jobs = batch.jobs.len(), "dispatch obligations dropped: community not served");
            metrics::counter!("buzz_event_dispatch_dropped_total", "reason" => "community_not_served")
                .increment(batch.jobs.len() as u64);
            if let Err(e) = state.db.complete_dispatch(community, &ids()).await {
                warn!(%community, "dispatch obligation delete failed: {e}");
            }
            return;
        }
        Err(e) => {
            warn!(%community, "dispatch recovery host lookup failed: {e}");
            release(state, &batch, ids(), "community host lookup failed").await;
            return;
        }
    };

    // Passes run concurrently, so one slow audit receipt cannot stretch the
    // batch past its claim lease, and each runs in its own task, so a panic
    // fails that obligation instead of stopping the worker.
    let passes = batch.jobs.iter().map(|job| {
        let state = Arc::clone(state);
        let tenant = tenant.clone();
        let job = job.clone();
        tokio::spawn(async move {
            let actor_hex = hex::encode(
                job.actor_pubkey
                    .clone()
                    .unwrap_or_else(|| job.event.event.pubkey.to_bytes().to_vec()),
            );
            dispatch_persistent_event_inner(
                &tenant,
                &state,
                &job.event,
                event_kind_u32(&job.event.event),
                &actor_hex,
                AuditStep::Enqueue,
                None,
                true,
            )
            .await
            .failure
        })
    });
    let outcomes = join_all(passes).await;

    let mut done = Vec::new();
    let mut failed = Vec::new();
    for (job, outcome) in batch.jobs.iter().zip(outcomes) {
        let event_id = job.event.event.id.as_bytes().to_vec();
        match outcome.unwrap_or(Some("panic")) {
            None => {
                metrics::counter!("buzz_event_dispatch_recovered_total").increment(1);
                done.push(event_id);
            }
            Some(stage) => {
                metrics::counter!("buzz_event_dispatch_retry_total", "stage" => stage).increment(1);
                warn!(
                    event_id = %job.event.event.id,
                    attempt = job.attempt,
                    stage,
                    "dispatch recovery attempt incomplete"
                );
                failed.push((event_id, format!("{stage} failed")));
            }
        }
    }

    if let Err(e) = state.db.complete_dispatch(community, &done).await {
        // The claims expire and the next pass finds every effect done.
        warn!(%community, "dispatch obligation delete failed: {e}");
    }
    retry(state, &batch, &failed).await;
}

async fn release(
    state: &AppState,
    batch: &buzz_db::event_dispatch::ClaimedDispatchBatch,
    event_ids: Vec<Vec<u8>>,
    reason: &str,
) {
    let failures: Vec<_> = event_ids
        .into_iter()
        .map(|id| (id, reason.to_owned()))
        .collect();
    retry(state, batch, &failures).await;
}

async fn retry(
    state: &AppState,
    batch: &buzz_db::event_dispatch::ClaimedDispatchBatch,
    failures: &[(Vec<u8>, String)],
) {
    match state
        .db
        .retry_dispatch_batch(batch.community, batch.claim_id, failures)
        .await
    {
        Ok(held) if held.is_empty() => {}
        Ok(held) => {
            metrics::counter!("buzz_event_dispatch_held_total").increment(held.len() as u64);
            for event_id in held {
                error!(
                    community = %batch.community,
                    event_id = %hex::encode(event_id),
                    "dispatch obligation held after its final attempt; see docs/event-dispatch-recovery.md"
                );
            }
        }
        // The claims expire and are retried after the lease.
        Err(e) => warn!(community = %batch.community, "dispatch retry release failed: {e}"),
    }
}
