//! PostgreSQL contract for dispatch obligations (`event_dispatch_queue`): the
//! obligation commits with its event insert, and the recovery claim, retry,
//! hold and completion transitions are fenced.
use crate::event_dispatch::{self, DispatchObligation, MAX_DISPATCH_ATTEMPTS};
use crate::{event, Db, DbConfig};

const TEST_DB_URL: &str = "postgres://buzz:buzz_dev@localhost:5432/buzz";
use buzz_core::CommunityId;
use chrono::{Duration, Utc};
use nostr::{Event, EventBuilder, Keys, Kind};
use sqlx::{PgPool, Row as _};
use uuid::Uuid;

/// A claim takes the community of the earliest due row in the whole database,
/// so the tests that claim run one at a time.
static CLAIMS: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn connect() -> Db {
    let config = DbConfig {
        database_url: std::env::var("BUZZ_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| TEST_DB_URL.to_owned()),
        ..DbConfig::default()
    };
    Db::new(&config).await.expect("connect to test database")
}

async fn create_community(pool: &PgPool) -> CommunityId {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
        .bind(id)
        .bind(format!("dispatch-{}.example", id.simple()))
        .execute(pool)
        .await
        .expect("create disposable community");
    CommunityId::from_uuid(id)
}

fn signed_event(keys: &Keys, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .sign_with_keys(keys)
        .expect("sign test event")
}

fn obligation(event: &Event, actor: &Keys) -> DispatchObligation {
    DispatchObligation::new(
        event.id.to_bytes(),
        Some(actor.public_key().to_bytes().to_vec()),
    )
}

/// `(state, attempts, actor, seconds until next attempt)` for one obligation.
async fn row(
    pool: &PgPool,
    community: CommunityId,
    event: &Event,
) -> Option<(String, i32, Option<Vec<u8>>, f64)> {
    sqlx::query(
        "SELECT state, attempts, actor_pubkey, \
                extract(epoch FROM next_attempt_at - now())::float8 AS due_in \
         FROM event_dispatch_queue WHERE community_id = $1 AND event_id = $2",
    )
    .bind(community.as_uuid())
    .bind(event.id.as_bytes().as_slice())
    .fetch_optional(pool)
    .await
    .expect("read obligation")
    .map(|r| {
        (
            r.get("state"),
            r.get("attempts"),
            r.get("actor_pubkey"),
            r.get("due_in"),
        )
    })
}

/// Make the obligation due ahead of any other in the database: a claim takes
/// the community of the earliest due row, and a shared test database can hold
/// rows other runs left due. A finite time, because `row` subtracts it.
async fn make_due(pool: &PgPool, community: CommunityId, event: &Event) {
    sqlx::query(
        "UPDATE event_dispatch_queue SET next_attempt_at = 'epoch' \
         WHERE community_id = $1 AND event_id = $2",
    )
    .bind(community.as_uuid())
    .bind(event.id.as_bytes().as_slice())
    .execute(pool)
    .await
    .expect("make obligation due");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn obligation_commits_with_its_event_insert_only() {
    let db = connect().await;
    let pool = &db.pool;
    let community = create_community(pool).await;
    let actor = Keys::generate();

    // An insert outside any scope owes nothing.
    let unscoped = signed_event(&actor, "unscoped");
    db.insert_event_with_thread_metadata(community, &unscoped, None, None)
        .await
        .expect("insert unscoped event");
    assert_eq!(row(pool, community, &unscoped).await, None);

    // The scoped insert writes one pending row, due after the grace period.
    let scoped = signed_event(&actor, "scoped");
    let (_, inserted) = obligation(&scoped, &actor)
        .scope(db.insert_event_with_thread_metadata(community, &scoped, None, None))
        .await
        .expect("insert scoped event");
    assert!(inserted);
    let (state, attempts, stored_actor, due_in) =
        row(pool, community, &scoped).await.expect("obligation row");
    assert_eq!((state.as_str(), attempts), ("pending", 0));
    assert_eq!(stored_actor, Some(actor.public_key().to_bytes().to_vec()));
    assert!(
        due_in > event_dispatch::DISPATCH_GRACE_SECS - 5.0,
        "due after the grace period, got {due_in}"
    );

    // A duplicate insert writes no second obligation and keeps the first.
    let (_, inserted) = obligation(&scoped, &actor)
        .scope(db.insert_event_with_thread_metadata(community, &scoped, None, None))
        .await
        .expect("duplicate insert");
    assert!(!inserted);
    assert_eq!(row(pool, community, &scoped).await.map(|r| r.1), Some(0));

    // A scope for another event id does not cover this insert.
    let other = signed_event(&actor, "other");
    let covered = signed_event(&actor, "covered elsewhere");
    obligation(&other, &actor)
        .scope(db.insert_event_with_thread_metadata(community, &covered, None, None))
        .await
        .expect("insert under mismatched scope");
    assert_eq!(row(pool, community, &covered).await, None);

    // A rolled-back insert leaves no obligation behind.
    let rolled_back = signed_event(&actor, "rolled back");
    let mut tx = db
        .begin_transaction()
        .await
        .expect("begin event transaction");
    obligation(&rolled_back, &actor)
        .scope(event::insert_event_with_thread_metadata_tx(
            &mut tx,
            community,
            &rolled_back,
            None,
            None,
        ))
        .await
        .expect("insert in transaction");
    tx.rollback().await.expect("roll back");
    assert_eq!(row(pool, community, &rolled_back).await, None);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn recovery_claims_are_fenced_backed_off_and_held() {
    let _claims = CLAIMS.lock().await;
    let db = connect().await;
    let pool = &db.pool;
    let community = create_community(pool).await;
    let actor = Keys::generate();
    let stored = signed_event(&actor, "needs recovery");
    obligation(&stored, &actor)
        .scope(db.insert_event_with_thread_metadata(community, &stored, None, None))
        .await
        .expect("insert scoped event");

    // Within the grace period nothing is claimable.
    let lease = || Utc::now() + Duration::seconds(60);
    let not_due = db
        .claim_due_dispatch_batch(10, lease())
        .await
        .expect("claim");
    assert!(not_due.is_none_or(|b| b.community != community));

    make_due(pool, community, &stored).await;
    let first = db
        .claim_due_dispatch_batch(10, lease())
        .await
        .expect("claim")
        .expect("due obligation claimed");
    assert_eq!(first.community, community);
    assert_eq!(first.jobs.len(), 1);
    assert_eq!(first.jobs[0].attempt, 1);
    assert_eq!(first.jobs[0].event.event.id, stored.id);
    assert_eq!(
        first.jobs[0].actor_pubkey,
        Some(actor.public_key().to_bytes().to_vec())
    );

    // A live claim is not reclaimed; an expired one is, under a new fence.
    assert!(db
        .claim_due_dispatch_batch(10, lease())
        .await
        .expect("claim")
        .is_none_or(|b| b.community != community));
    sqlx::query(
        "UPDATE event_dispatch_queue SET lease_until = now() - interval '1 second' \
         WHERE community_id = $1",
    )
    .bind(community.as_uuid())
    .execute(pool)
    .await
    .expect("expire lease");
    let second = db
        .claim_due_dispatch_batch(10, lease())
        .await
        .expect("claim")
        .expect("expired claim reclaimed");
    assert_eq!(second.jobs[0].attempt, 2);
    assert_ne!(second.claim_id, first.claim_id);

    // The stale owner cannot release the obligation.
    let id = stored.id.as_bytes().to_vec();
    let failure = vec![(id.clone(), "publish failed".to_owned())];
    db.retry_dispatch_batch(community, first.claim_id, &failure)
        .await
        .expect("stale retry");
    assert_eq!(
        row(pool, community, &stored).await.map(|r| r.0),
        Some("dispatching".into())
    );

    // The owner's retry backs off 5 s * 2^(attempt - 1).
    let held = db
        .retry_dispatch_batch(community, second.claim_id, &failure)
        .await
        .expect("retry");
    assert!(held.is_empty());
    let (state, _, _, due_in) = row(pool, community, &stored).await.expect("row");
    assert_eq!(state, "pending");
    assert!((8.0..=10.5).contains(&due_in), "backoff {due_in}");

    // The final allowed attempt fails into `held`, which is never claimed.
    sqlx::query("UPDATE event_dispatch_queue SET attempts = $2 WHERE community_id = $1")
        .bind(community.as_uuid())
        .bind(MAX_DISPATCH_ATTEMPTS - 1)
        .execute(pool)
        .await
        .expect("spend attempts");
    make_due(pool, community, &stored).await;
    let last = db
        .claim_due_dispatch_batch(10, lease())
        .await
        .expect("claim")
        .expect("last attempt claimed");
    assert_eq!(last.jobs[0].attempt, MAX_DISPATCH_ATTEMPTS);
    let held = db
        .retry_dispatch_batch(community, last.claim_id, &failure)
        .await
        .expect("retry");
    assert_eq!(held, vec![id.clone()]);
    make_due(pool, community, &stored).await;
    assert!(db
        .claim_due_dispatch_batch(10, lease())
        .await
        .expect("claim")
        .is_none_or(|b| b.community != community));

    // Completion deletes it whatever its state.
    assert_eq!(
        db.complete_dispatch(community, &[id])
            .await
            .expect("complete"),
        1
    );
    assert_eq!(row(pool, community, &stored).await, None);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn abandoned_final_attempt_is_held_and_deleted_event_is_dropped() {
    let _claims = CLAIMS.lock().await;
    let db = connect().await;
    let pool = &db.pool;
    let community = create_community(pool).await;
    let actor = Keys::generate();

    // A worker that died on the final attempt leaves a claim nobody may take.
    let abandoned = signed_event(&actor, "abandoned");
    obligation(&abandoned, &actor)
        .scope(db.insert_event_with_thread_metadata(community, &abandoned, None, None))
        .await
        .expect("insert");
    sqlx::query(
        "UPDATE event_dispatch_queue SET state = 'dispatching', attempts = $2, \
                claim_id = gen_random_uuid(), lease_until = now() - interval '1 second', \
                next_attempt_at = now() - interval '1 minute' \
         WHERE community_id = $1",
    )
    .bind(community.as_uuid())
    .bind(MAX_DISPATCH_ATTEMPTS)
    .execute(pool)
    .await
    .expect("abandon final attempt");
    assert!(db.hold_abandoned_dispatches().await.expect("sweep") >= 1);
    assert_eq!(
        row(pool, community, &abandoned).await.map(|r| r.0),
        Some("held".into())
    );

    // An obligation whose event was deleted is dropped, not dispatched.
    let deleted = signed_event(&actor, "deleted before recovery");
    obligation(&deleted, &actor)
        .scope(db.insert_event_with_thread_metadata(community, &deleted, None, None))
        .await
        .expect("insert");
    sqlx::query("UPDATE events SET deleted_at = now() WHERE community_id = $1 AND id = $2")
        .bind(community.as_uuid())
        .bind(deleted.id.as_bytes().as_slice())
        .execute(pool)
        .await
        .expect("soft delete");
    make_due(pool, community, &deleted).await;
    let batch = db
        .claim_due_dispatch_batch(10, Utc::now() + Duration::seconds(60))
        .await
        .expect("claim");
    assert!(batch.is_none_or(|b| b.community != community));
    assert_eq!(row(pool, community, &deleted).await, None);
}
