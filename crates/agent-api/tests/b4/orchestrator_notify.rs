//! Live HO-15/22/26 (no helpdesk ref) and HO-23–26 (`tool_failed` on every orchestrator handover): every J21 item
//! the orchestrator queues — no procedure (`out_of_scope`), a required operation not offered (`not_offered`), unmet
//! success criteria (`success_criteria_unmet`), a failed case (`tool_failed`) — tells the job's worker channels
//! (`handover::notify_workers`, the fake agent-edge of tests/common/conv) and records its trigger's reason on the
//! item and as the case's `escalation_reason`. The orchestrator's runtime state runs on the `agent_runtime` pool, as
//! the binary's does (`state::optional_runtime`), so the notify's reads and its worker-thread upsert prove 0179's
//! grants. Mutation: dropping a `notify_workers` call leaves `notified` empty for that trigger.
use std::sync::Arc;

use agent_api::config::EdgeConfig;
use agent_api::conversations::edge_link::EdgeLink;
use agent_api::runtime::Env;
use agent_api::runtime::assistant::Assistant;
use agent_api::runtime::domain::{CaseOutcome, OpState, QualifiedRequest};
use agent_api::runtime::orchestrator::{Orchestrator, StaticProcedures};
use agent_api::state::AppState;
use serde_json::{Value, json};

use crate::b4::flow::{
    DELIVERY_AREA, EveryOpOffered, Fx, answer, calls, current_design, delivery_flow_output,
    delivery_request, escalation_reason, handover_items, insert_case, insert_conversation, orch,
    qualified, qualify, request, resolutions, resolutions_in, setup_latest,
};
use crate::common::conv::{EDGE_KEY, Edge, edge};

/// A context space of the job's area that no procedure serves (as tests/b4/assistant.rs).
const FREE_SPACE: &str = "retail:order-support:anything_else";

/// The fake edge, a verified worker binding of the fixture's job, and the orchestrator's runtime state on the
/// `agent_runtime` pool with the edge wired in (attached through a switched-off [`Assistant`], as the binary does).
async fn notifying(fx: &Fx) -> (Edge, Assistant) {
    let (e, st) = notifying_state(fx).await;
    (e, Assistant::new(st))
}

/// The fake edge, a verified worker binding of the fixture's job, and a runtime state on the `agent_runtime` pool
/// with the edge wired in (the orchestrator's through [`notifying`], the runner's as is: tests/b4/runner_notify.rs).
pub(crate) async fn notifying_state(fx: &Fx) -> (Edge, AppState) {
    let e = edge().await;
    sqlx::query(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, settings, created_by, \
                                         verified_at) \
         VALUES ($1, $2, 'helpdesk', 'worker', 'T-staff', 'Staff', '{\"base_url\": \"https://helpdesk.example\", \"service_user\": \"bot@helpdesk.example\"}', 'test', now())",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let cfg = EdgeConfig {
        internal_key: Some(EDGE_KEY.to_owned()),
        deliver_url: Some(e.url.clone()),
        ..EdgeConfig::default()
    };
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    st.edge = EdgeLink::from_config(&cfg, reqwest::Client::new()).map(Arc::new);
    assert!(st.edge.is_some());
    (e, st)
}

/// The case's one item id.
pub(crate) async fn item_id(fx: &Fx, case: &str) -> String {
    sqlx::query_scalar("SELECT id FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2")
        .bind(fx.d.f.tenant)
        .bind(case)
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap()
}

/// The worker threads stored for item `item` (`channel_conversation_refs`, side `worker`): `(channel, external_id)`.
pub(crate) async fn worker_refs(fx: &Fx, item: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT channel, external_id FROM channel_conversation_refs \
          WHERE tenant_id = $1 AND item_id = $2 AND side = 'worker'",
    )
    .bind(fx.d.f.tenant)
    .bind(item)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The edge was told about exactly the case's item, with `reason`, to the job's worker binding, and the thread the
/// worker channel opened is stored; the item and the case carry `reason`, the item `trigger`.
pub(crate) async fn assert_notified(fx: &Fx, e: &Edge, case: &str, reason: &str, trigger: &str) {
    assert_eq!(
        handover_items(fx, case).await,
        [(reason.to_owned(), trigger.to_owned())]
    );
    assert_eq!(escalation_reason(fx, case).await.as_deref(), Some(reason));
    let item = item_id(fx, case).await;
    let sent = e.notified();
    assert_eq!(sent.len(), 1, "one notify per queued item: {sent:?}");
    assert_eq!(sent[0]["tenant"], json!(fx.d.f.tenant), "{sent:?}");
    assert_eq!(sent[0]["job_id"], json!(fx.d.f.job.job_id), "{sent:?}");
    assert_eq!(sent[0]["item"]["id"], json!(item), "{sent:?}");
    assert_eq!(sent[0]["item"]["reason"], json!(reason), "{sent:?}");
    let targets = sent[0]["targets"].as_array().cloned().unwrap_or_default();
    assert_eq!(targets.len(), 1, "{sent:?}");
    assert_eq!(targets[0]["kind"], "helpdesk", "{sent:?}");
    assert_eq!(
        worker_refs(fx, &item).await,
        [("helpdesk".to_owned(), format!("thread-{item}"))],
        "the worker thread is stored on the agent_runtime pool (0179)"
    );
    assert_eq!(e.unauthorized.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// [`orch`] with the notifying runtime state attached.
fn orch_notifying(fx: &Fx, a: Assistant) -> Orchestrator<StaticProcedures> {
    orch(fx, StaticProcedures).with_assistant(Arc::new(a))
}

/// No procedure serves the request: the `out_of_scope` item notifies the worker channels.
#[tokio::test]
async fn a_request_no_procedure_serves_notifies_the_workers_out_of_scope() {
    let fx = setup_latest("orch-notify-out-of-scope", Env::Test).await;
    let (e, a) = notifying(&fx).await;
    let o = orch_notifying(&fx, a);
    insert_conversation(&fx, "conv-CS-1").await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    let mut req = request("CS-1");
    req.context_space = FREE_SPACE.into();
    req.request_type = "other".into();
    o.on_qualified(qualified(&req)).await.unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_notified(
        &fx,
        &e,
        "CS-1",
        "out_of_scope",
        "no procedure serves this request",
    )
    .await;
}

/// A required operation is not offered (the default routes, no demo stub): `not_offered`, before any call.
#[tokio::test]
async fn a_required_operation_not_offered_notifies_the_workers_not_offered() {
    let fx = setup_latest("orch-notify-not-offered", Env::Test).await;
    let (e, a) = notifying(&fx).await;
    let o = orch_notifying(&fx, a);
    insert_conversation(&fx, "conv-CS-1").await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    o.on_qualified(qualified(&delivery_request("CS-1")))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_offered"));
    assert!(calls(&fx, "CS-1").await.is_empty(), "no call is sent");
    let trigger = handover_items(&fx, "CS-1").await[0].1.clone();
    assert!(
        trigger.starts_with("procedure needs ") && trigger.ends_with(", which is not offered"),
        "{trigger}"
    );
    assert_notified(&fx, &e, "CS-1", "not_offered", &trigger).await;
}

/// Every step answered ok but the parcel is a problem: `success_criteria_unmet`, the criterion named in the trigger.
#[tokio::test]
async fn unmet_success_criteria_notify_the_workers_success_criteria_unmet() {
    let fx = setup_latest("orch-notify-success-criteria", Env::Test).await;
    let (e, a) = notifying(&fx).await;
    let o = orch_notifying(&fx, a).with_routes(Arc::new(EveryOpOffered));
    insert_conversation(&fx, "conv-CS-1").await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    o.on_qualified(qualified(&delivery_request("CS-1")))
        .await
        .unwrap();
    for _ in 0..3 {
        let call = calls(&fx, "CS-1").await.pop().unwrap();
        let out = delivery_flow_output(&call, "problem");
        o.on_result(answer(&call, OpState::Ok, "ok", out))
            .await
            .unwrap();
    }
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_notified(
        &fx,
        &e,
        "CS-1",
        "success_criteria_unmet",
        "success criteria unmet: delivery_on_track",
    )
    .await;
}

/// A step that did not answer fails the case: its item (`queue_failed`, committed by `finish`) stays `tool_failed`
/// and notifies the workers too.
#[tokio::test]
async fn a_failed_case_notifies_the_workers_tool_failed() {
    let fx = setup_latest("orch-notify-failed", Env::Test).await;
    let (e, a) = notifying(&fx).await;
    let o = orch_notifying(&fx, a);
    insert_conversation(&fx, "conv-CS-1").await;
    qualify(&fx, &o, "CS-1").await;
    let call = calls(&fx, "CS-1").await[0].clone();
    o.on_result(answer(&call, OpState::Error, "timeout", Value::Null))
        .await
        .unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed, "{rs:?}");
    assert_notified(
        &fx,
        &e,
        "CS-1",
        "tool_failed",
        "case failed: order.read did not answer in time",
    )
    .await;
}

/// No runtime state attached (no [`Assistant`]): the item is queued as before and nothing is sent.
#[tokio::test]
async fn without_the_runtime_state_the_item_is_queued_and_nothing_is_sent() {
    let fx = setup_latest("orch-notify-no-state", Env::Test).await;
    let (e, _a) = notifying(&fx).await;
    let o = orch(&fx, StaticProcedures);
    insert_conversation(&fx, "conv-CS-1").await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    let mut req: QualifiedRequest = request("CS-1");
    req.context_space = FREE_SPACE.into();
    o.on_qualified(qualified(&req)).await.unwrap();
    assert_eq!(handover_items(&fx, "CS-1").await.len(), 1);
    assert!(e.notified().is_empty());
}
