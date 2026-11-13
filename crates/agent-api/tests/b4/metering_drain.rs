//! B4 T11 audit (agent#43): the metering drainer's claim lock, send budget and settle accounting
//! (`agent_claim_metering`, 0129; crate::usage::drainer).
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{self, insert_tenant};
use crate::redis_env as r;
use agent_api::usage::drainer::{self, Limits};

/// `n` ledger rows with their pending outbox rows for a fresh tenant (owner pool: RLS does not apply).
async fn seed_outbox(owner: &PgPool, n: usize) -> (Uuid, Vec<i64>) {
    let tenant = Uuid::new_v4();
    insert_tenant(owner, tenant, &format!("Metering Co {tenant}")).await;
    let mut ids = Vec::new();
    for _ in 0..n {
        let key = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO usage_event (tenant_id, job_id, event_key, source, use, actor, provider_id, funding) \
             VALUES ($1, NULL, $2, 'turn', 'qualify', 'customers:web_chat', 'internal', 'local_gpu')",
        )
        .bind(tenant)
        .bind(key)
        .execute(owner)
        .await
        .unwrap();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO metering_outbox (tenant_id, job_id, event_key, fields) \
             VALUES ($1::uuid, NULL, $2::uuid, jsonb_build_object('event_key', $2::text, 'project', $1::text)) RETURNING id",
        )
        .bind(tenant)
        .bind(key)
        .fetch_one(owner)
        .await
        .unwrap();
        ids.push(id);
    }
    (tenant, ids)
}

const CLAIM: &str = "SELECT id FROM agent_claim_metering($1)";

/// Two drainers claiming at the same instant never get the same row, and the second does not wait for the first:
/// `FOR UPDATE SKIP LOCKED` in 0129. Without the lock clause the second claim blocks on the first's row locks and,
/// once the first commits, re-claims (and would re-send) every row; with plain `FOR UPDATE` it blocks.
#[tokio::test]
async fn concurrent_claims_are_disjoint_and_do_not_block() {
    let e = common::env().await;
    let (_tenant, ids) = seed_outbox(&e.owner, 5).await;
    let db = e.state.db.clone();

    let mut a_tx = db.begin().await.unwrap();
    let mut a: Vec<i64> = sqlx::query_scalar(CLAIM)
        .bind(1000)
        .fetch_all(&mut *a_tx)
        .await
        .unwrap();
    a.sort_unstable();
    assert_eq!(a, ids, "drainer A claims every due row");

    // drainer B claims while A's claim is still uncommitted (A is mid-statement as far as B can tell)
    let b_db = db.clone();
    let b = tokio::spawn(async move {
        sqlx::query_scalar::<_, i64>(CLAIM)
            .bind(1000)
            .fetch_all(&b_db)
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b_done_before_a_commits = b.is_finished();
    a_tx.commit().await.unwrap();
    let b = tokio::time::timeout(Duration::from_secs(10), b)
        .await
        .expect("drainer B finishes once A commits")
        .unwrap();

    let overlap: Vec<i64> = b.iter().copied().filter(|i| a.contains(i)).collect();
    assert!(
        overlap.is_empty(),
        "no row is claimed (and sent) by two drainers: {overlap:?}"
    );
    assert!(
        b_done_before_a_commits,
        "SKIP LOCKED: the second claim skips the first's rows instead of waiting on them"
    );
    // after A's commit the rows are leased, not due
    let again: Vec<i64> = sqlx::query_scalar(CLAIM)
        .bind(1000)
        .fetch_all(&db)
        .await
        .unwrap();
    assert!(again.is_empty(), "a leased row is not due: {again:?}");
}

/// A batch whose send budget has run out sends nothing more: the rows stay pending under their lease (due again for
/// the next claim after it), never sent after the lease could have passed to another drainer.
#[tokio::test]
async fn exhausted_send_budget_defers_and_never_sends() {
    let e = common::env().await;
    let (tenant, ids) = seed_outbox(&e.owner, 4).await;
    let url = r::url().await;
    let stream = format!("{}costs", r::prefix("metering_drain_budget"));
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = redis::aio::ConnectionManager::new(client).await.unwrap();

    let d = drainer::drain_once_with(
        &e.state.db,
        &mut conn,
        &stream,
        Limits {
            send_budget: Duration::ZERO,
            ..Limits::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(d.claimed, ids.len(), "{d:?}");
    assert_eq!(d.sent, 0, "{d:?}");
    assert_eq!(d.failed, 0, "{d:?}");
    assert_eq!(d.deferred, ids.len(), "{d:?}");
    let len: i64 = redis::cmd("XLEN")
        .arg(&stream)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(
        len, 0,
        "nothing reaches the stream once the budget is spent"
    );
    let (pending, leased, attempts): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'pending' AND sent_at IS NULL), \
                count(*) FILTER (WHERE next_attempt_at > now() + interval '60 seconds'), \
                coalesce(sum(attempts), 0) \
           FROM metering_outbox WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(pending, 4, "deferred rows stay pending");
    assert_eq!(
        leased, 4,
        "deferred rows keep their lease (not released early)"
    );
    assert_eq!(attempts, 0, "a deferral is not a failed attempt");

    // the default budget sends them once the lease is over
    sqlx::query("UPDATE metering_outbox SET next_attempt_at = now() WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    let d = drainer::drain_once(&e.state.db, &mut conn, &stream)
        .await
        .unwrap();
    assert_eq!((d.sent, d.deferred, d.settle_lost), (4, 0, 0), "{d:?}");
    let len: i64 = redis::cmd("XLEN")
        .arg(&stream)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(len, 4);
    let _: i64 = redis::cmd("DEL")
        .arg(&stream)
        .query_async(&mut conn)
        .await
        .unwrap();
}

/// The settle is idempotent and says whether it changed a row: a second settle of a sent row (a drainer whose lease
/// another drainer took over) touches nothing and reports it, instead of silently counting it as settled.
#[tokio::test]
async fn settle_is_idempotent_and_reports_a_lost_lease() {
    let e = common::env().await;
    let (tenant, ids) = seed_outbox(&e.owner, 1).await;
    let id = ids[0];
    assert!(
        drainer::settle_sent(&e.state.db, tenant, id).await.unwrap(),
        "the first settle changes the row"
    );
    let sent_at: Option<String> =
        sqlx::query_scalar("SELECT sent_at::text FROM metering_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(sent_at.is_some());
    assert!(
        !drainer::settle_sent(&e.state.db, tenant, id).await.unwrap(),
        "a repeat settle changes nothing and says so"
    );
    let (status, sent_at2): (String, Option<String>) =
        sqlx::query_as("SELECT status, sent_at::text FROM metering_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(status, "sent");
    assert_eq!(sent_at2, sent_at, "a repeat settle does not move sent_at");
    // another tenant's GUC cannot settle it (RLS), and that too reports false rather than success
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, &format!("Other {other}")).await;
    let (t2, ids2) = seed_outbox(&e.owner, 1).await;
    assert!(
        !drainer::settle_sent(&e.state.db, other, ids2[0])
            .await
            .unwrap(),
        "a settle under the wrong tenant touches no row"
    );
    assert!(
        drainer::settle_sent(&e.state.db, t2, ids2[0])
            .await
            .unwrap()
    );
}
