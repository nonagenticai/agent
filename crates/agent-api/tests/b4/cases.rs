//! B4 T12: the case reads (`cases`, `caseload`, `caseDetail`) and the manual hand-over's `operation_call` row
//! (`case.create{handover}`, route `fallback`, instance `agent_cases`, reason `staff_decision`). Cases come from the
//! B12 fixture (crate::common::conv `customer_case`): every new web chat conversation opens `CS-n` (Production, never
//! released). Real Postgres 18; a fresh database per test.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::conv::{self, Ctx, code};
use crate::common::sqlstate;

/// One web chat conversation per `n`, each opening its case (`CS-1`, `CS-2`, …) with the customer's message.
async fn cases(c: &Ctx, n: usize) {
    for i in 1..=n {
        let (_, case_id) = c
            .customer_case(
                &format!("wc-{i}"),
                &format!("m{i}"),
                "Where is my parcel XYZZY?",
            )
            .await;
        assert_eq!(case_id, format!("CS-{i}"));
    }
}

async fn hand_over(c: &Ctx, case_id: &str, reason: &str) -> (StatusCode, Value) {
    c.call(
        "POST",
        &format!("/api/jobs/j1/cases/{case_id}/handover"),
        "u-agent",
        json!({ "reason": reason }),
    )
    .await
}

#[tokio::test]
async fn handover_of_closed_case_is_409() {
    let c = conv::setup().await;
    cases(&c, 1).await;
    sqlx::query(
        "UPDATE agent_case SET status = 'resolved', resolved_at = now() WHERE case_id = 'CS-1'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = hand_over(&c, "CS-1", "x").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "case_closed"));
    assert!(
        c.texts("SELECT call_id FROM operation_call")
            .await
            .is_empty(),
        "a refused handover records no call"
    );
    let (s, b) = hand_over(&c, "CS-9", "x").await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "case_not_found"));
}

#[tokio::test]
async fn second_handover_is_already_handed_over() {
    let c = conv::setup().await;
    cases(&c, 1).await;
    let (s, b) = hand_over(&c, "CS-1", "first").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["op_calls"], 1, "{b}");
    let (s, b) = hand_over(&c, "CS-1", "second").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "already_handed_over"));
    // the refused second handover rolled back with its call row and events
    assert_eq!(
        c.texts("SELECT operation FROM operation_call").await,
        ["case.create{handover}"]
    );
    assert_eq!(
        c.texts("SELECT op_calls::text FROM agent_case WHERE case_id = 'CS-1'")
            .await,
        ["1"]
    );
    assert_eq!(
        c.texts("SELECT kind FROM case_event WHERE case_id = 'CS-1' AND kind IN ('operation', 'state') ORDER BY seq")
            .await,
        ["operation", "state"]
    );
}

#[tokio::test]
async fn manual_handover_reason_is_staff_decision() {
    let c = conv::setup().await;
    cases(&c, 1).await;
    let (s, b) = hand_over(&c, "CS-1", "angry customer PLUGH").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "handover");
    assert_eq!(
        c.texts("SELECT reason FROM agent_handover_items WHERE case_id = 'CS-1'")
            .await,
        ["staff_decision"]
    );
    assert_eq!(
        c.texts(
            "SELECT concat_ws('|', operation, route, instance_id, env, state, outcome_code, initiator, attempt::text, \
                    coalesce(release_id, '-'), coalesce(draft_rev::text, '-'), (finished_at IS NOT NULL)::text) \
               FROM operation_call WHERE case_id = 'CS-1'"
        )
        .await,
        ["case.create{handover}|fallback|agent_cases|prod|ok|ok|u-agent|1|-|-|true"]
    );
    let detail = c.texts("SELECT detail FROM operation_call").await;
    assert!(
        detail[0].starts_with("manual handover by ") && !detail[0].contains("PLUGH"),
        "the call names the caller, never the reason: {detail:?}"
    );
    // the audit row links the call
    assert_eq!(
        c.texts(
            "SELECT (details->>'call_id' = (SELECT call_id FROM operation_call))::text FROM audit_event \
              WHERE action = 'case.handover'"
        )
        .await,
        ["true"]
    );
    // the reason CHECK (0063, 0179, 0183) takes all 8 D64/D85/D94 reasons, the orchestrator's two, 0183's `step_up_required`,
    // and nothing else
    for reason in [
        "identity_failed",
        "gate_blocked",
        "customer_asked",
        "tool_failed",
        "unknown_outcome",
        "out_of_scope",
        "staff_decision",
        "gate_unavailable",
        "not_offered",
        "success_criteria_unmet",
        "step_up_required",
    ] {
        sqlx::query("UPDATE agent_handover_items SET reason = $1 WHERE case_id = 'CS-1'")
            .bind(reason)
            .execute(&c.e.owner)
            .await
            .unwrap_or_else(|e| panic!("{reason}: {e}"));
    }
    let e = sqlx::query("UPDATE agent_handover_items SET reason = 'manual' WHERE case_id = 'CS-1'")
        .execute(&c.e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23514"), "{e}");
}

#[tokio::test]
async fn assign_to_member_without_support_agent_422_not_assignable() {
    let c = conv::setup().await;
    cases(&c, 1).await;
    let (s, b) = hand_over(&c, "CS-1", "x").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    // D88: only support_agent holders are assignable — not an observer, not an integrations manager
    for who in ["u-obs", "u-int"] {
        let (s, b) = c.assign(&conv_id, "u-mgr", who, "queue").await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "not_assignable"),
            "{who}: {b}"
        );
    }
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn take_sets_assignee_and_event() {
    // SW T2: Take replaces Claim; a case not handed over is agent-owned, so the Take expecting the queue is refused
    let c = conv::setup().await;
    cases(&c, 1).await;
    let conv_id = c
        .texts("SELECT conversation_id FROM agent_case WHERE case_id = 'CS-1'")
        .await
        .remove(0);
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    assert_eq!(b["error"]["owner"]["owner"], "agent", "{b}");
    hand_over(&c, "CS-1", "x").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent2");
    assert_eq!(
        c.texts(
            "SELECT e.to_state || ':' || e.assignee || ':' || e.actor FROM agent_handover_events e \
              WHERE e.assignee IS NOT NULL"
        )
        .await,
        ["assigned:u-agent2:u-agent2"]
    );
    let (s, d) = c.get("/api/jobs/j1/cases/CS-1", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["case"]["assignee"], "u-agent2");
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
}

#[tokio::test]
async fn envelope_has_no_message_text() {
    let c = conv::setup().await;
    cases(&c, 1).await;
    let (s, b) = hand_over(&c, "CS-1", "angry customer PLUGH").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, d) = c.get("/api/jobs/j1/cases/CS-1", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["case"]["case_id"], "CS-1");
    assert_eq!(d["case"]["status"], "handover");
    let env = &d["envelope"];
    let text = env.to_string();
    for leaked in ["XYZZY", "PLUGH", "Femke", "manual handover by"] {
        assert!(!text.contains(leaked), "{leaked} in the envelope: {text}");
    }
    assert_eq!(env["case_id"], "CS-1");
    assert_eq!(env["env"], "prod");
    assert_eq!(env["status"], "handover");
    assert_eq!(env["escalation_reason"], "staff_decision");
    assert_eq!(env["route_hint"], Value::Null);
    assert_eq!(env["op_calls"][0]["op"], "case.create{handover}");
    assert_eq!(env["op_calls"][0]["route"], "fallback");
    assert_eq!(env["op_calls"][0]["outcome"], "ok");
    // the timeline is the case's events as TimelineEvents: the operation completed from its call row
    let tl = d["timeline"].as_array().unwrap();
    let op = tl
        .iter()
        .find(|e| e["kind"] == "operation")
        .expect("operation event");
    assert_eq!(op["op"], "case.create{handover}");
    assert_eq!(op["instance"], "agent_cases");
    assert_eq!(op["state"], "ok");
    assert!(
        op["at"].is_string() && op["call_id"].as_str().unwrap().starts_with("opc_"),
        "{op}"
    );
    let st = tl
        .iter()
        .find(|e| e["kind"] == "state")
        .expect("state event");
    assert_eq!(st["status"], "handover");
    assert!(st["by"].is_string(), "{st}");
    let (s, b) = c.get("/api/jobs/j1/cases/CS-99", "u-obs").await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "case_not_found"));
}

#[tokio::test]
async fn cases_page_newest_first_with_filters() {
    let c = conv::setup().await;
    cases(&c, 3).await;
    let ids = |p: &Value| -> Vec<String> {
        p["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["case_id"].as_str().unwrap().to_string())
            .collect()
    };
    let (s, p) = c.get("/api/jobs/j1/cases?limit=2", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(ids(&p), ["CS-3", "CS-2"]);
    assert_eq!(p["next"], "2");
    assert_eq!(p["items"][0]["job"], "j1");
    let (_, p) = c.get("/api/jobs/j1/cases?limit=2&before=2", "u-obs").await;
    assert_eq!(ids(&p), ["CS-1"]);
    assert_eq!(p["next"], Value::Null);
    let (_, p) = c.get("/api/jobs/j1/cases?q=cs-2", "u-obs").await;
    assert_eq!(ids(&p), ["CS-2"]);
    hand_over(&c, "CS-1", "x").await;
    let (_, p) = c
        .get("/api/jobs/j1/cases?status=handover&env=all", "u-obs")
        .await;
    assert_eq!(ids(&p), ["CS-1"]);
    let (_, p) = c.get("/api/jobs/j1/cases?env=test", "u-obs").await;
    assert!(ids(&p).is_empty(), "{p}");
    for bad in ["status=closed", "env=dev", "before=x"] {
        let (s, b) = c.get(&format!("/api/jobs/j1/cases?{bad}"), "u-obs").await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
    }
}

#[tokio::test]
async fn caseload_counts_and_fleet() {
    let c = conv::setup().await;
    cases(&c, 3).await;
    sqlx::query(
        "UPDATE agent_case SET status = 'resolved', resolved_at = now() WHERE case_id = 'CS-2'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    // closed, but an unknown outcome a person must check keeps it on the caseload
    sqlx::query(
        "UPDATE agent_case SET status = 'failed', alert = 'unknown_outcome' WHERE case_id = 'CS-3'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, l) = c.get("/api/jobs/j1/caseload", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let mut active: Vec<&str> = l["active"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["case_id"].as_str().unwrap())
        .collect();
    active.sort_unstable();
    assert_eq!(active, ["CS-1", "CS-3"]);
    assert_eq!(
        l["closed_7d"],
        json!({"resolved": 1, "not_eligible": 0, "cancelled_offboard": 0})
    );
    assert_eq!(l["env_counts"], json!({"prod": 2, "test": 0}));
    assert!(l["as_of"].is_string());
    let f = &l["fleet"];
    assert_eq!(f["production"]["release"], Value::Null, "{f}");
    assert!(
        f["test"]["rev"].is_i64() && f["test"]["unpublished"].is_i64(),
        "{f}"
    );
    assert_eq!(f["approved_waiting"], Value::Null);
    assert_eq!(f["follow_s"], 35);
    // the env filter narrows `active`, never `env_counts`
    let (_, l) = c.get("/api/jobs/j1/caseload?env=test", "u-obs").await;
    assert_eq!(l["active"], json!([]));
    assert_eq!(l["env_counts"], json!({"prod": 2, "test": 0}));
}

#[tokio::test]
async fn b8_signals_count_production_cases() {
    use agent_api::cases::signals;
    use agent_api::design::JobRef;

    let c = conv::setup().await;
    cases(&c, 3).await;
    // CS-1 contained, CS-2 handed over, CS-3 another activity; a test-env case would not count (none here)
    sqlx::query(
        "UPDATE agent_case SET activity = CASE WHEN case_id = 'CS-3' THEN 'support:returns' ELSE 'support:track' END, \
                outcome = 'ok', duration_ms = 1000",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE agent_case SET status = 'resolved', resolved_at = now() WHERE case_id = 'CS-1'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = hand_over(&c, "CS-2", "x").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let job = JobRef {
        id: c.job,
        key: "j1".into(),
    };
    let mut tx = agent_api::tenancy::begin_scoped(&c.e.state.db, c.t)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(c.job.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let rows = signals::activity_outcomes(&mut tx, &job, 30).await.unwrap();
    let got: Vec<(String, i64, i64, i64)> = rows
        .into_iter()
        .map(|r| (r.activity, r.conversations, r.contained, r.escalated))
        .collect();
    assert_eq!(
        got,
        [
            ("support:track".to_string(), 2, 1, 1),
            ("support:returns".to_string(), 1, 0, 0)
        ]
    );
    let live = signals::use_case_live(&mut tx, &job, "support:track", 30)
        .await
        .unwrap();
    let l = live.live.expect("live numbers");
    assert_eq!(
        (l.cases, l.contained, l.escalated, l.p50_ms),
        (2, 1, 1, Some(1000))
    );
    assert_eq!(live.outcomes.len(), 1);
    assert_eq!(
        (live.outcomes[0].code.as_str(), live.outcomes[0].count),
        ("ok", 2)
    );
    assert!(
        signals::use_case_live(&mut tx, &job, "support:none", 30)
            .await
            .unwrap()
            .live
            .is_none()
    );
    let recent = signals::recent_for_activity(&mut tx, &job, "support:track", 10)
        .await
        .unwrap();
    let ids: Vec<&str> = recent.iter().map(|r| r.case_id.as_str()).collect();
    assert_eq!(ids, ["CS-2", "CS-1"]);
    // the manual handover's call is routed (`fallback`), so nothing is unrouted; no reply said "I don't know"
    assert!(
        signals::unrouted_calls(&mut tx, &job, 30)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        signals::knowledge_misses(&mut tx, &job, 30)
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
}
