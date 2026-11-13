//! B4 T16: the handover remainder next to B12's queue (tests/handover.rs stays B12's) — the `handover_policy` design
//! kind (contract, draft-not-live save, 422 `invalid_policy`), SLA due times stamped from the live policy (prod) or
//! the draft (test) when an item enters the queue, the feedback verdict and the draft-proposal refusals, the queue
//! order (`handover::next_unassigned`) and concurrent Takes (SW T2), the customer's answer moving `waiting_customer → assigned`, and the discovery signals.
//! Real Postgres 18 (one container for the `b4` binary, a fresh database per test).
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::conv::{self, Ctx, code};
use crate::common::{self, design_contract};
use agent_api::conversations::{handover, store};
use agent_api::design::DraftRef;
use agent_api::handover::signals;

fn targets(fr: [i64; 3], res: [i64; 3]) -> Value {
    json!({
        "high": {"first_response_min": fr[0], "resolve_min": res[0]},
        "normal": {"first_response_min": fr[1], "resolve_min": res[1]},
        "low": {"first_response_min": fr[2], "resolve_min": res[2]},
    })
}

/// A conversation on web chat `wc` whose customer asked for a person: (conversation id, item id). A prod item.
async fn queued(c: &Ctx, wc: &str) -> (String, String) {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    (
        b["conversation_id"].as_str().unwrap().to_string(),
        b["item_id"].as_str().unwrap().to_string(),
    )
}

async fn post(c: &Ctx, path: &str, sub: &str, body: Value) -> (StatusCode, Value) {
    c.call("POST", path, sub, body).await
}

async fn save_policy(c: &Ctx, t: Value) -> (StatusCode, Value) {
    c.call(
        "PUT",
        "/api/jobs/j1/handover/policy",
        "u-mgr",
        json!({"targets": t}),
    )
    .await
}

/// `(first_response_due − queued_at, resolve_due − queued_at)` of item `id`, in minutes (`None` = no due time).
async fn due_minutes(db: &PgPool, tenant: Uuid, id: &str) -> (Option<i64>, Option<i64>) {
    sqlx::query_as(
        "SELECT (extract(epoch FROM first_response_due - queued_at) / 60)::bigint, \
                (extract(epoch FROM resolve_due - queued_at) / 60)::bigint \
           FROM agent_handover_items WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(id.to_owned())
    .fetch_one(db)
    .await
    .unwrap()
}

/// Release `cfg-2026-10-21.1` of job j1 as its live release, whose snapshot holds handover policy `t` (owner pool).
async fn release_with_policy(c: &Ctx, t: Value) {
    let rel = "cfg-2026-10-21.1";
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $4, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'current', 'u-rm', now(),
               'u-rm', now(), ARRAY['handover_policy'])"#,
    )
    .bind(c.t)
    .bind(c.job)
    .bind(rel)
    .bind(format!("sha256:{}", "b".repeat(64)))
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
         VALUES ($1, $2, $3, 'handover_policy', 'handover_policy', '{}', $4)",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(rel)
    .bind(json!({"v": {"targets": t, "hours": null}}))
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(c.job)
        .bind(rel)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

/// A test-env item on job j1 (the staff test chat's env), through B12's writer as agent_app: (case id, item id).
async fn test_item(c: &Ctx, conv_id: &str) -> (String, String) {
    test_item_with(c, conv_id, "customer_asked").await
}

/// [`test_item`] with handover `reason`.
async fn test_item_with(c: &Ctx, conv_id: &str, reason: &str) -> (String, String) {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let conv = store::Conv {
        id: conv_id.to_owned(),
        channel: "web_chat".into(),
        env: "test".into(),
        customer_id: Some("cust-1".into()),
        customer_name: None,
        initiator_sub: None,
        identity_source: None,
        customer_email: None,
        identity_strength: None,
    };
    let d = store::design_of(&mut tx, c.job, "j1", &conv).await.unwrap();
    let case_id = store::create_case(&mut tx, c.job, &conv, "Help", &d)
        .await
        .unwrap();
    let item = handover::create(
        &mut tx,
        handover::NewItem {
            job_id: c.job,
            case_id: &case_id,
            conv: &conv,
            reason,
            trigger: "test chat",
            subject: "Help",
            actor: "agent",
            design: &d,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (case_id, item)
}

// ---- the design kind ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn handover_policy_design_contract() {
    let e = common::env().await;
    let samples = vec![(
        DraftRef::new(),
        json!({"targets": targets([15, 60, 240], [240, 1440, 4320]), "hours": null}),
    )];
    design_contract::run_all(&e, "handover_policy", &samples, Some("no SLA")).await;
}

#[tokio::test]
async fn save_policy_is_a_draft_not_live() {
    let c = conv::setup().await;
    let (s, v) = c.get("/api/jobs/j1/handover/policy", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        json!({"job": "j1", "live": null, "draft": null, "drafted": false,
               "hours": {"live": null, "draft": null}})
    );
    let t = targets([15, 60, 240], [240, 1440, 4320]);
    let (s, v) = save_policy(&c, t.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        json!({"job": "j1", "live": null, "draft": t, "drafted": true,
               "hours": {"live": null, "draft": null}})
    );
    let (_, read) = c.get("/api/jobs/j1/handover/policy", "u-obs").await;
    assert_eq!(read, v, "a member reads the same view");
    // the ledger row and the audit of a tracked write (D54), never live on save
    assert_eq!(
        c.texts("SELECT kind || ':' || key FROM draft_ledger WHERE kind = 'handover_policy'")
            .await,
        ["handover_policy:handover_policy"]
    );
    assert_eq!(
        c.texts(
            "SELECT details->>'kind' FROM audit_event WHERE action = 'job.draft_change' AND resource_type = 'handover'"
        )
        .await,
        ["handover_policy"]
    );
    // a prod item runs the live policy: none, so no SLA
    let (_, item) = queued(&c, "wc-1").await;
    assert_eq!(due_minutes(&c.e.owner, c.t, &item).await, (None, None));
}

#[tokio::test]
async fn invalid_policy_is_422() {
    let c = conv::setup().await;
    let mut missing = targets([15, 60, 240], [240, 1440, 4320]);
    missing.as_object_mut().unwrap().remove("low");
    for (t, msg) in [
        (missing, "low: missing"),
        (
            targets([0, 60, 240], [240, 1440, 4320]),
            "high: minutes must be whole, 1 to 43200",
        ),
        (
            targets([15, 60, 240], [240, 1440, 43_201]),
            "low: minutes must be whole, 1 to 43200",
        ),
        (
            targets([15, 90, 240], [240, 60, 4320]),
            "normal: first response must come before resolve",
        ),
        (json!("15 minutes"), "high: missing"),
    ] {
        let (s, b) = save_policy(&c, t).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_policy"),
            "{b}"
        );
        assert_eq!(b["error"]["message"], msg, "{b}");
    }
    assert!(
        c.texts("SELECT job_id::text FROM handover_policy")
            .await
            .is_empty(),
        "nothing was written"
    );
    // support_manager only (tests/roles.rs walks every route; this is the direct refusal)
    let (s, b) = c
        .call(
            "PUT",
            "/api/jobs/j1/handover/policy",
            "u-agent",
            json!({"targets": targets([15, 60, 240], [240, 1440, 4320])}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
}

// ---- due times (plan R8) ------------------------------------------------------------------------------------------

#[tokio::test]
async fn draft_policy_does_not_change_live_items() {
    let c = conv::setup().await;
    let live = targets([10, 30, 120], [100, 300, 1200]);
    release_with_policy(&c, live.clone()).await;
    let draft = targets([5, 20, 60], [50, 200, 600]);
    let (s, v) = save_policy(&c, draft.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["live"], live, "{v}");
    assert_eq!(v["draft"], draft, "{v}");
    assert_eq!(v["drafted"], true);

    // prod items are stamped from the live policy, test items from the draft (priority normal)
    let (_, prod) = queued(&c, "wc-1").await;
    assert_eq!(
        due_minutes(&c.e.owner, c.t, &prod).await,
        (Some(30), Some(300))
    );
    let (_, test) = test_item(&c, "conv-test-1").await;
    assert_eq!(
        due_minutes(&c.e.owner, c.t, &test).await,
        (Some(20), Some(200))
    );

    // a later draft moves no stamped item, and the next prod item still runs live
    let (s, _) = save_policy(&c, targets([1, 2, 3], [4, 5, 6])).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        due_minutes(&c.e.owner, c.t, &prod).await,
        (Some(30), Some(300))
    );
    assert_eq!(
        due_minutes(&c.e.owner, c.t, &test).await,
        (Some(20), Some(200))
    );
    let (_, prod2) = queued(&c, "wc-2").await;
    assert_eq!(
        due_minutes(&c.e.owner, c.t, &prod2).await,
        (Some(30), Some(300))
    );
    let (_, test2) = test_item(&c, "conv-test-2").await;
    assert_eq!(
        due_minutes(&c.e.owner, c.t, &test2).await,
        (Some(2), Some(5)),
        "the next test item runs the new draft"
    );
    // the queue shows the stamped due times
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let row = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == prod.as_str())
        .unwrap();
    assert!(row["first_response_due"].is_string(), "{row}");
    assert!(row["resolve_due"].is_string(), "{row}");
}

// ---- queue order, Take and the state machine ------------------------------------------------------------------------

/// The unassigned queued item to take next (`handover::next_unassigned`).
async fn next_unassigned(c: &Ctx) -> Option<handover::Item> {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let it = handover::next_unassigned(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    it
}

#[tokio::test]
async fn take_next_order_is_breached_priority_due_age() {
    let c = conv::setup().await;
    let mut ids = Vec::new();
    for wc in ["wc-1", "wc-2", "wc-3", "wc-4"] {
        ids.push(queued(&c, wc).await.1);
    }
    // a: normal, due in 10 min · b: low, first response overdue · c: high, no SLA · d: normal, due in 5 min
    for (id, priority, due_min) in [
        (&ids[0], "normal", Some(10)),
        (&ids[1], "low", Some(-5)),
        (&ids[2], "high", None),
        (&ids[3], "normal", Some(5)),
    ] {
        sqlx::query(
            "UPDATE agent_handover_items SET priority = $3, \
                    first_response_due = now() + make_interval(mins => $4::int) \
              WHERE tenant_id = $1 AND id = $2",
        )
        .bind(c.t)
        .bind(id.as_str())
        .bind(priority)
        .bind(due_min)
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    let mut taken = Vec::new();
    for _ in 0..4 {
        let it = next_unassigned(&c).await.expect("an unassigned item");
        let (s, b) = c.take(&it.conversation_id, "u-agent", "queue").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["item_id"], it.id.as_str(), "{b}");
        taken.push(it.id);
    }
    let want = vec![
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
        ids[0].clone(),
    ];
    assert_eq!(
        taken, want,
        "breached, then priority, then the earliest due, then age"
    );
    assert!(next_unassigned(&c).await.is_none(), "the queue is empty");
}

#[tokio::test]
async fn two_concurrent_takes_of_different_items_both_win() {
    let c = conv::setup().await;
    let (conv_a, a) = queued(&c, "wc-1").await;
    let (conv_b, b) = queued(&c, "wc-2").await;
    let ((s1, x), (s2, y)) = tokio::join!(
        c.take(&conv_a, "u-agent", "queue"),
        c.take(&conv_b, "u-agent2", "queue"),
    );
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{x} {y}");
    assert_eq!(
        (x["item_id"].as_str(), y["item_id"].as_str()),
        (Some(a.as_str()), Some(b.as_str())),
        "each taker got its own item"
    );
    // and on one item exactly one taker wins
    let (conv_c, _) = queued(&c, "wc-3").await;
    let ((s1, x), (s2, y)) = tokio::join!(
        c.take(&conv_c, "u-agent", "queue"),
        c.take(&conv_c, "u-agent2", "queue"),
    );
    let mut got = [s1, s2];
    got.sort();
    assert_eq!(got, [StatusCode::OK, StatusCode::CONFLICT], "{x} {y}");
}

#[tokio::test]
async fn customer_reply_moves_waiting_to_assigned() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .send_message(
            &conv_id,
            "u-agent",
            "reply",
            "Which order is it about?",
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        c.texts("SELECT state || ':' || (first_response_at IS NOT NULL) FROM agent_handover_items")
            .await,
        ["waiting_customer:true"],
        "the owner's reply waits for the customer"
    );
    let (s, b) = c.inbound("wc-1", "wc-1-2", "Order 1042, thanks").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        c.texts("SELECT state FROM agent_handover_items").await,
        ["assigned"],
        "the customer's answer hands the item back to its assignee"
    );
}

// ---- feedback and the draft proposal -------------------------------------------------------------------------------

async fn feedback(c: &Ctx, item: &str, sub: &str, verdict: &str) -> (StatusCode, Value) {
    post(
        c,
        &format!("/api/jobs/j1/handover/{item}/feedback"),
        sub,
        json!({"verdict": verdict, "note": "  should know opening hours  "}),
    )
    .await
}

async fn draft(c: &Ctx, item: &str) -> (StatusCode, Value) {
    post(
        c,
        &format!("/api/jobs/j1/handover/{item}/feedback/draft"),
        "u-admin",
        json!({}),
    )
    .await
}

async fn set_proposal(c: &Ctx, item: &str, p: Value) {
    sqlx::query(
        "UPDATE agent_handover_feedback SET proposal = $3 WHERE tenant_id = $1 AND item_id = $2",
    )
    .bind(c.t)
    .bind(item.to_owned())
    .bind(p)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn feedback_attaches_the_hint_or_the_knowledge_gap_default() {
    let c = conv::setup().await;
    let (_, a) = queued(&c, "wc-1").await;
    let (_, b) = queued(&c, "wc-2").await;
    let (_, d) = queued(&c, "wc-3").await;
    // no hint: the knowledge_gap default, linking the case
    let (s, v) = feedback(&c, &a, "u-agent", "agent_should_handle").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let f = &v["feedback"];
    assert_eq!(f["verdict"], "agent_should_handle");
    assert_eq!(f["note"], "should know opening hours");
    assert_eq!(f["by"], "u-agent");
    assert_eq!(f["drafted_at"], Value::Null);
    assert_eq!(f["proposal"]["kind"], "knowledge_gap");
    assert_eq!(
        f["proposal"]["href"],
        format!("/admin/jobs/j1/cases/{}", v["case_ref"].as_str().unwrap())
    );
    // the item's hint wins
    let hint = json!({"kind": "enable_activity", "activity": "opening_hours", "stage": "support",
                      "label": "Opening hours", "detail": "Enable the hidden activity.",
                      "href": "/admin/jobs/j1/journey"});
    sqlx::query(
        "UPDATE agent_handover_items SET proposal_hint = $3 WHERE tenant_id = $1 AND id = $2",
    )
    .bind(c.t)
    .bind(b.as_str())
    .bind(&hint)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (_, v) = feedback(&c, &b, "u-agent", "agent_should_handle").await;
    assert_eq!(v["feedback"]["proposal"], hint);
    // right to hand over: no proposal
    let (_, v) = feedback(&c, &d, "u-agent2", "right_to_hand_over").await;
    assert_eq!(v["feedback"]["verdict"], "right_to_hand_over");
    assert_eq!(v["feedback"]["proposal"], Value::Null);
    // one verdict per item (SW T4): a second is refused and the first stays
    let (s, v) = feedback(&c, &d, "u-agent2", "agent_should_handle").await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "feedback_exists"),
        "{v}"
    );
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let kept = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == d.as_str())
        .unwrap()["feedback"]
        .clone();
    assert_eq!(kept["verdict"], "right_to_hand_over", "{kept}");
    // audited with the verdict, never the note
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'handover.feedback' ORDER BY id",
    )
    .bind(c.t)
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(details.len(), 3);
    assert_eq!(details[0]["verdict"], "agent_should_handle");
    assert_eq!(details[1]["proposal"], "enable_activity");
    assert_eq!(details[2]["proposal"], Value::Null);
    assert!(
        details
            .iter()
            .all(|d| !d.to_string().contains("opening hours")),
        "{details:?}"
    );
    // unknown item
    let (s, v) = feedback(&c, "HO-99", "u-agent", "right_to_hand_over").await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "case_not_found"));
}

#[tokio::test]
async fn draft_proposal_codes() {
    let c = conv::setup().await;
    let (_, a) = queued(&c, "wc-1").await;
    let (_, b) = queued(&c, "wc-2").await;
    // no feedback, and a right_to_hand_over verdict: no proposal
    let (s, v) = draft(&c, &a).await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, "no_proposal"), "{v}");
    feedback(&c, &b, "u-agent", "right_to_hand_over").await;
    let (s, v) = draft(&c, &b).await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, "no_proposal"), "{v}");
    // a knowledge_gap proposal is made on its own page
    feedback(&c, &a, "u-agent", "agent_should_handle").await;
    let (s, v) = draft(&c, &a).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_draftable_here"),
        "{v}"
    );
    // a draftable proposal is written to the draft (B8 T7 registered the activity kind): a J4 extension
    set_proposal(
        &c,
        &a,
        json!({"kind": "tenant_extension", "activity": "gift_cards", "stage": "support", "label": "Gift cards",
               "detail": "", "href": "/admin/jobs/j1/journey"}),
    )
    .await;
    let (s, v) = draft(&c, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["feedback"]["drafted_at"].is_string(), "{v}");
    assert_eq!(
        c.texts("SELECT key FROM draft_ledger").await,
        ["journey:gift_cards"]
    );
    // a drafted proposal: already_drafted, and one verdict per item
    let (s, v) = draft(&c, &a).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "already_drafted"),
        "{v}"
    );
    let (s, v) = feedback(&c, &a, "u-agent", "right_to_hand_over").await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "feedback_exists"),
        "{v}"
    );
    // unknown item; and job_designer only
    let (s, v) = draft(&c, "HO-99").await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "case_not_found"));
    let (s, v) = post(
        &c,
        &format!("/api/jobs/j1/handover/{a}/feedback/draft"),
        "u-agent",
        json!({}),
    )
    .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "forbidden"), "{v}");
}

#[tokio::test]
async fn draft_handover_proposal_writes_activity_draft() {
    let c = conv::setup().await;
    let (_, a) = queued(&c, "wc-1").await;
    let (_, b) = queued(&c, "wc-2").await;

    // tenant_extension: the tenant's own activity in the draft, never live (D54)
    feedback(&c, &a, "u-agent", "agent_should_handle").await;
    set_proposal(
        &c,
        &a,
        json!({"kind": "tenant_extension", "activity": "support:gift_cards", "stage": "support",
               "label": "Gift cards", "detail": "", "href": "/admin/jobs/j1/journey"}),
    )
    .await;
    let (s, v) = draft(&c, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["feedback"]["drafted_at"].is_string(), "{v}");
    let (kind, item): (String, Value) = sqlx::query_as(
        "SELECT kind, item FROM job_overlay_row WHERE object = 'activity' AND key = 'support:gift_cards'",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(kind, "extension");
    assert_eq!(item["label"], "Gift cards");
    assert_eq!(item["stage"], "support");
    assert_eq!(item["actor"], "customer");
    assert!(
        item["trigger"]
            .as_str()
            .is_some_and(|t| t.starts_with(&format!("handover {a}"))),
        "{item}"
    );
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT key, kind, page FROM draft_ledger ORDER BY key")
            .fetch_all(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [(
            "journey:support:gift_cards".to_string(),
            "activity".to_string(),
            "J4".to_string()
        )]
    );
    let (s, v) = draft(&c, &a).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "already_drafted"),
        "{v}"
    );

    // enable_activity of an activity that is not hidden has nothing to enable; an extension key that exists is
    // already extended
    feedback(&c, &b, "u-agent", "agent_should_handle").await;
    set_proposal(
        &c,
        &b,
        json!({"kind": "enable_activity", "activity": "support:gift_cards", "stage": "support",
               "label": "Gift cards", "detail": "", "href": "/admin/jobs/j1/journey"}),
    )
    .await;
    let (s, v) = draft(&c, &b).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "already_enabled"),
        "{v}"
    );
    set_proposal(
        &c,
        &b,
        json!({"kind": "tenant_extension", "activity": "support:gift_cards", "stage": "support",
               "label": "Gift cards", "detail": "", "href": "/admin/jobs/j1/journey"}),
    )
    .await;
    let (s, v) = draft(&c, &b).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "already_extended"),
        "{v}"
    );
    assert_eq!(
        c.texts(&format!(
            "SELECT coalesce(drafted_at::text, 'none') FROM agent_handover_feedback WHERE item_id = '{b}'"
        ))
        .await,
        ["none"],
        "a refused draft marks nothing"
    );
}

// ---- discovery signals (B8) ----------------------------------------------------------------------------------------

#[tokio::test]
async fn discovery_signals_report_prod_items_and_verdicts() {
    let c = conv::setup().await;
    let (_, a) = queued(&c, "wc-1").await;
    let (_, b) = queued(&c, "wc-2").await;
    test_item(&c, "conv-test-1").await;
    feedback(&c, &a, "u-agent", "agent_should_handle").await;
    sqlx::query(
        "UPDATE agent_handover_items SET first_response_due = now() - interval '1 minute' \
          WHERE tenant_id = $1 AND id = $2",
    )
    .bind(c.t)
    .bind(b.as_str())
    .execute(&c.e.owner)
    .await
    .unwrap();
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let got = signals::discovery_signals(&mut tx, since).await.unwrap();
    tx.commit().await.unwrap();
    let ids: Vec<&str> = got.iter().map(|s| s.item_id.as_str()).collect();
    assert_eq!(ids, [a.as_str(), b.as_str()], "prod items only");
    assert_eq!(got[0].reason, "customer_asked");
    assert_eq!(got[0].verdict.as_deref(), Some("agent_should_handle"));
    assert_eq!(got[0].proposal.as_ref().unwrap()["kind"], "knowledge_gap");
    assert!(!got[0].sla_breached && !got[0].drafted);
    assert_eq!(got[1].verdict, None);
    assert!(got[1].sla_breached, "an open first response past due");
}

// ---- the AgentHandoverBreached gauge -------------------------------------------------------------------------------

#[tokio::test]
async fn breached_gauge_counts_prod_items_only() {
    let c = conv::setup().await;
    let (_, prod) = queued(&c, "wc-g1").await;
    test_item(&c, "conv-test-g1").await;
    sqlx::query(
        "UPDATE agent_handover_items SET first_response_due = now() - interval '1 minute' \
          WHERE tenant_id = $1",
    )
    .bind(c.t)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let n = agent_api::bus_admin::count_breached(&c.e.state.db, c.t)
        .await
        .unwrap();
    assert_eq!(n, 1, "the past-due Test-env item is not a customer waiting");

    // closing the prod item leaves nothing breached, though the test item is still open and past due
    sqlx::query(
        "UPDATE agent_handover_items SET state = 'resolved', closed_at = now() \
          WHERE tenant_id = $1 AND id = $2",
    )
    .bind(c.t)
    .bind(prod.as_str())
    .execute(&c.e.owner)
    .await
    .unwrap();
    let n = agent_api::bus_admin::count_breached(&c.e.state.db, c.t)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// B4 known customer: a step that needs identity level 3 hands over with `step_up_required` (0183). The writer
/// (agent_app) takes the reason, the support inbox labels it in plain words, and it is one of the state machine's
/// reasons. Mutation: the reason missing from `handover::REASONS` / the CHECK / `names::reason_label`.
#[tokio::test]
async fn a_step_up_handover_item_is_taken_and_labelled() {
    let c = conv::setup().await;
    let (case_id, item) = test_item_with(&c, "conv-step-up", "step_up_required").await;
    assert!(handover::REASONS.contains(&"step_up_required"));
    let reason: String = sqlx::query_scalar(
        "SELECT reason FROM agent_handover_items WHERE tenant_id = $1 AND id = $2 AND case_id = $3",
    )
    .bind(c.t)
    .bind(item.as_str())
    .bind(&case_id)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(reason, "step_up_required");
    assert_eq!(
        agent_api::support::names::reason_label(&reason),
        "Step-up needed"
    );
}
