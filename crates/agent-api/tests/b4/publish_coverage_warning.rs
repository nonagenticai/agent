//! Owner ruling 2026-10-22: a design kind the LIVE release does not cover (§2.5 coverage: a kind newer than the
//! release, e.g. `handover_policy`, until `design::coverage::backfill` covers it) must be visible BEFORE publishing —
//! publishing makes the draft value live. Pinned here:
//!
//! - `design::diff` lists the draft's `handover_policy` place with `uncovered = Some(<live release>)` (it used to hide
//!   it: live = draft "never differs");
//! - `GET /api/jobs/{job}/draft` marks the change `uncovered: <live release>` (the J14 release page's warn chip);
//! - the approve + apply still succeeds (never blocked), and afterwards the new release covers the kind: no
//!   uncovered entry and no draft change remains.
//!
//! Mutations this catches: `diff` keeping `same(live, draft) ⇒ continue` for uncovered kinds (no entry: first
//! assert fails); `uncovered` set from `snap.covers` inverted (every covered delta marked, and the uncovered one not);
//! `draft.rs` not filtering by the place kind (`uncovered` on the `selection` change too — asserted null); a gate that
//! refuses an uncovered publish (`cut` asserts 200/201).
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, cut, new_job, publish, release_manager};
use crate::common::{self, TestEnv, get as http_get, send};
use agent_api::design::{self, DraftRef};

fn policy() -> Value {
    let t = |fr: i64, res: i64| json!({"first_response_min": fr, "resolve_min": res});
    json!({
        "targets": {"high": t(5, 120), "normal": t(30, 720), "low": t(120, 2880)},
        "hours": null,
    })
}

/// `design::diff` of `f`'s job, in a job-scoped transaction: `(place key, kind, uncovered)` per delta.
async fn diff_of(e: &TestEnv, f: &ContractJob) -> Vec<(String, String, Option<String>)> {
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let d = design::diff(&mut tx, &e.state.design, &jr).await.unwrap();
    tx.commit().await.unwrap();
    d.into_iter()
        .map(|d| (d.place.key, d.place.kind, d.uncovered))
        .collect()
}

async fn draft_changes(e: &TestEnv, f: &ContractJob, tok: &str) -> Vec<Value> {
    let uri = format!("/api/jobs/{}/draft", f.job.job_key);
    let (s, b) = send(&e.app, http_get(&uri, Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["changes"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn uncovered_kind_is_flagged_before_publish_and_covered_after() {
    let e = common::env().await;
    let f = new_job(&e).await;
    let first = publish(&e, &f).await;

    // the live release as an older build left it: no handover_policy in its snapshot (backfill not run / failed).
    // `job_release.snapshot_kinds` is append-only (trigger `job_release_immutable`, 0045) and `release_snapshot` rows
    // never change, so build that release the legitimate way: a NEW release row = `first` minus the kind, its snapshot
    // rows minus the kind, made current (`first` → `applied_earlier`, as apply does) and pointed at by the job
    // (pattern gap_tests.rs `release_live`). Its id is the next N of `first`'s day, so the later cut mints past it.
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
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&live)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        kinds.iter().all(|k| k != "handover_policy") && !kinds.is_empty(),
        "the live release does not cover handover_policy: {kinds:?}"
    );

    // a draft policy saved after that release
    design_contract::write(&e, &f, "handover_policy", &DraftRef::new(), Some(policy())).await;

    // (1) the diff lists it, marked uncovered by the live release; nothing else is marked
    let d = diff_of(&e, &f).await;
    let hp: Vec<_> = d
        .iter()
        .filter(|(_, k, _)| k == "handover_policy")
        .collect();
    assert_eq!(hp.len(), 1, "the uncovered draft policy is listed: {d:?}");
    assert_eq!(hp[0].2.as_deref(), Some(live.as_str()), "{d:?}");
    assert!(
        d.iter()
            .all(|(_, k, u)| k == "handover_policy" || u.is_none()),
        "only the uncovered kind is marked: {d:?}"
    );

    // (2) the J14 draft summary carries the same mark on that change (and only there)
    let tok = release_manager(&e, &f).await;
    let changes = draft_changes(&e, &f, &tok).await;
    let ch: Vec<&Value> = changes
        .iter()
        .filter(|c| c["kind"] == "handover_policy")
        .collect();
    assert_eq!(ch.len(), 1, "{changes:?}");
    assert_eq!(ch[0]["uncovered"], json!(live), "{changes:?}");
    for c in changes.iter().filter(|c| c["kind"] != "handover_policy") {
        assert_eq!(c["uncovered"], Value::Null, "{c}");
    }

    // (3) a warning, never a block: approve + apply succeeds
    let second = cut(&e, &f, &tok, true).await;
    assert_ne!(second, first);
    assert_ne!(second, live);
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&second)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        kinds.iter().any(|k| k == "handover_policy"),
        "a fresh publish covers the kind: {kinds:?}"
    );

    // (4) after publishing: covered, so no uncovered entry and no change left
    let d = diff_of(&e, &f).await;
    assert!(d.iter().all(|(_, _, u)| u.is_none()), "{d:?}");
    assert!(
        d.iter().all(|(_, k, _)| k != "handover_policy"),
        "live = draft once covered: {d:?}"
    );
    let changes = draft_changes(&e, &f, &tok).await;
    assert!(
        changes.iter().all(|c| c["uncovered"] == Value::Null),
        "{changes:?}"
    );
}
