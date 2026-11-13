//! B4 T4 / §0 R4 — `release::follow::follow_live` bumps `jobs.draft_rev` once when an apply moved the draft under
//! the job (a working row rewritten to the new live value, or a ledger row flagged `live_moved`), so a test case
//! stamped `CaseDesign::Draft{draft_rev}` before the move never claims the moved draft. A plain apply that only
//! publishes its own bundle moves nothing and bumps nothing.
use agent_api::design::{CaseDesign, DesignOf};
use serde_json::json;

use crate::b4::design::{
    apply, approve, draft_rev, load, put_languages, put_target, put_target_seen, setup,
};

#[tokio::test]
async fn follow_live_no_bump_when_nothing_moved() {
    let fx = setup(None).await;
    let at = put_target(&fx).await;
    put_languages(&fx, json!(["en", "de"])).await;
    let r1 = approve(&fx, &at, false).await;
    let rev = draft_rev(&fx).await;
    // the apply publishes exactly what the draft holds: no working row changes, no ledger row is flagged
    assert_eq!(apply(&fx, &r1).await, "apply");
    assert_eq!(draft_rev(&fx).await, rev, "nothing moved under the draft");
    let draft = load(&fx, DesignOf::Draft).await;
    assert_eq!(draft.case, CaseDesign::Draft { draft_rev: rev });
}

#[tokio::test]
async fn follow_live_bumps_draft_rev_when_a_place_moves() {
    let fx = setup(None).await;
    let at = put_target(&fx).await;
    put_languages(&fx, json!(["en", "de"])).await;
    let r1 = approve(&fx, &at, true).await;
    put_languages(&fx, json!(["en", "de", "fr"])).await;
    let r2 = approve(&fx, &put_target_seen(&fx).await, false).await;
    assert_eq!(apply(&fx, &r2).await, "apply");
    let before = load(&fx, DesignOf::Draft).await;
    let rev = draft_rev(&fx).await;
    assert_eq!(before.case, CaseDesign::Draft { draft_rev: rev });

    // the rollback moves live [en, de, fr] → [en, de]; no unpublished change sits there, so the working row follows
    assert_eq!(apply(&fx, &r1).await, "rollback");
    assert_eq!(
        draft_rev(&fx).await,
        rev + 1,
        "one bump for the moved draft"
    );
    let after = load(&fx, DesignOf::Draft).await;
    assert_eq!(after.languages, ["en", "de"]);
    assert_eq!(after.case, CaseDesign::Draft { draft_rev: rev + 1 });

    // a moved place under an unpublished change: flagged `live_moved`, the draft keeps its value, one bump again
    put_languages(&fx, json!(["en", "nl"])).await;
    let rev = draft_rev(&fx).await;
    assert_eq!(apply(&fx, &r2).await, "reapply");
    assert_eq!(
        draft_rev(&fx).await,
        rev + 1,
        "a flagged ledger row bumps too"
    );
    let kept = load(&fx, DesignOf::Draft).await;
    assert_eq!(
        kept.languages,
        ["en", "nl"],
        "the unpublished change keeps its draft value"
    );
    let flagged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM draft_ledger WHERE job_id = $1 AND key = 'languages' AND live_moved IS NOT NULL",
    )
    .bind(fx.f.job.job_id)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(flagged, 1);
}
