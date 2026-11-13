//! B4 audit of agent#58: a boot backfill that failed, then an edit of kind K, then a later backfill used to strand the
//! ledger row — the backfill copies the working rows (the draft included) into the live release, so `live == draft`,
//! yet the row `ledger::record` kept for the then-uncovered kind stayed: `unpublished = 1` with nothing to publish.
//! `coverage::backfill` now settles those rows in its own transaction (draft = live, covered, not bundled in an
//! approved cut).
//!
//! Mutation this catches: no `settle_ledger` (the row and the draft change survive the backfill).
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, new_job, publish, release_manager};
use crate::common::{self, get as http_get, send};
use agent_api::design::{DraftRef, coverage};

fn policy() -> Value {
    let t = |fr: i64, res: i64| json!({"first_response_min": fr, "resolve_min": res});
    json!({
        "targets": {"high": t(5, 120), "normal": t(30, 720), "low": t(120, 2880)},
        "hours": null,
    })
}

#[tokio::test]
async fn a_later_backfill_that_copies_the_edited_draft_settles_its_ledger_row() {
    let e = common::env().await;
    let f = new_job(&e).await;
    let first = publish(&e, &f).await;

    // the live release as a boot whose backfill failed left it: no handover_policy (same construction as
    // publish_coverage_warning.rs: a new current release = `first` minus the kind)
    let (day, n) = first.rsplit_once('.').unwrap();
    let live = format!("{day}.{}", n.parse::<u32>().unwrap() + 1);
    sqlx::query(
        "UPDATE job_release SET state = 'applied_earlier' WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&first)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins, \
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at, applied_by, \
               last_applied_at, readiness, snapshot_kinds, rollbacks_seen) \
         SELECT tenant_id, job_id, $3, hash, bundle_hash, bundle, selection, pins, provenance, requires, causes, \
               'current', approved_by, approved_at, released_by, released_at, applied_by, last_applied_at, readiness, \
               array_remove(snapshot_kinds, 'handover_policy'), 0 \
           FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&first)
    .bind(&live)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
         SELECT tenant_id, job_id, $3, key, kind, ref, snap FROM release_snapshot \
          WHERE job_id = $1 AND release_id = $2 AND kind <> 'handover_policy'",
    )
    .bind(f.job.job_id)
    .bind(&first)
    .bind(&live)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(&live)
        .execute(&e.owner)
        .await
        .unwrap();

    // kind K edited while uncovered: the ledger keeps the row (live reads the working rows, so draft = live)
    design_contract::write(&e, &f, "handover_policy", &DraftRef::new(), Some(policy())).await;
    let rows = |kind: &'static str| {
        let (db, job) = (e.owner.clone(), f.job.job_id);
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM draft_ledger WHERE job_id = $1 AND kind = $2",
            )
            .bind(job)
            .bind(kind)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        rows("handover_policy").await,
        1,
        "the uncovered edit is kept"
    );

    // the later backfill copies the working rows (the edit included) into the live release
    let done = coverage::backfill(&e.state.db, &e.state.design)
        .await
        .unwrap();
    assert_eq!(done.failed_tenants, 0);
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&live)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        kinds.iter().any(|k| k == "handover_policy"),
        "the backfill covered the kind: {kinds:?}"
    );

    // live == draft now, so nothing is left to publish for K: no stranded row, no draft change
    assert_eq!(
        rows("handover_policy").await,
        0,
        "the ledger row is settled with the copy"
    );
    let tok = release_manager(&e, &f).await;
    let uri = format!("/api/jobs/{}/draft", f.job.job_key);
    let (s, b) = send(&e.app, http_get(&uri, Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let changes = b["changes"].as_array().cloned().unwrap_or_default();
    assert!(
        changes.iter().all(|c| c["kind"] != "handover_policy"),
        "{changes:?}"
    );
}
