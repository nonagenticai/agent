//! B4 known customer, seed v16 intent: a contact change (`contact_point.update`) is a contract op at catalogue level 3,
//! and a known customer's case is born at level 2. A2 replaced the level comparison with conversation STATE: a known
//! customer (`subject` in state) passes step 3, so what is left to prove is that a customer's own contact point
//! changes through the runner once confirmed. The catalogue is the REAL operation contract (`contracts/opdefs.json`,
//! pinned through the base layer). Real Postgres 18 + Redis, the runtime as `agent_runtime`.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_api::connectors::tenant_templates::op_defs;
use agent_api::runtime::Env;
use agent_api::runtime::catalog::OperationCatalog;
use agent_api::runtime::design::{DesignCache, RuntimeDesign};
use agent_api::runtime::domain::{OpCall, OpState as ResultState};
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_model::kinds::catalog::IdentityLevel;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::b4::design::put_target;
use crate::b4::flow::{Fx, current_design};
use crate::redis_env as r;

pub(crate) const OP: &str = "contact_point.update";
const CONTACT_OWN: &str = "CP-1";

/// `contact_point.update` as the system of record: counts every call that reaches it.
pub(crate) struct Contact {
    pub(crate) hits: AtomicUsize,
    pub(crate) inputs: Mutex<Vec<Value>>,
}

impl OpExecutor for Contact {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.inputs
                .lock()
                .unwrap()
                .push(Value::Object(req.inputs.clone()));
            outcome(
                OpState::Ok,
                None,
                None,
                json!({"verified": true}).as_object().cloned().unwrap(),
            )
        })
    }
}

/// The fixture's design as the runtime loads it: the catalogue is the operation contract (`contracts/opdefs.json`)
/// authored on the base layer and pinned by the job, NOT adjusted by this test. `contact_point.update` is at level 3
/// because the contract says so; a contract entry that lost its level fails here.
async fn design_from_contract(fx: &Fx) -> RuntimeDesign {
    let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
    let d = RuntimeDesign::load(
        &mut tx,
        &fx.d.e.state.design,
        &fx.d.f.job_ref(),
        &agent_api::design::DesignOf::Draft,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let contract = op_defs()
        .iter()
        .find(|o| o.object == "contact_point" && o.verb == "update" && o.variant == "*")
        .expect("contact_point.update is in the operation contract");
    assert_eq!(
        contract.identity_level,
        IdentityLevel::StepUp,
        "the contract says a contact change needs step-up"
    );
    assert_eq!(
        d.catalog.get(OP).map(|o| o.identity_level),
        Some(IdentityLevel::StepUp),
        "the pinned catalogue carries the contract's level for {OP}"
    );
    d
}

pub(crate) struct World {
    pub(crate) fx: Fx,
    pub(crate) contact: Arc<Contact>,
    pub(crate) runner: Runner,
}

async fn world(test: &str) -> World {
    world_with(test, |_| ()).await
}

/// [`world`] with the loaded design adjusted by `adjust` first (a test that needs a catalogue the platform no longer
/// writes, e.g. a row stored before the contract floor existed).
pub(crate) async fn world_with(test: &str, adjust: impl FnOnce(&mut RuntimeDesign)) -> World {
    let fx = crate::b4::flow::setup_latest(test, Env::Test).await;
    put_target(&fx.d).await;
    let cache = Arc::new(DesignCache::default());
    let mut design = design_from_contract(&fx).await;
    adjust(&mut design);
    cache.prime(&fx.scope, Arc::new(design));
    let contact = Arc::new(Contact {
        hits: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
    });
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let runner = Runner::new(fx.bus.clone(), st, cache)
        .unwrap()
        .with_consumer("runner-step-up")
        .with_consume(r::fast())
        .with_executors(Executors::default().register(OP, contact.clone()));
    World {
        fx,
        contact,
        runner,
    }
}

/// Ann's case on web chat, bound to her (C-1) by conversation state; the inbound message id (the write's `confirm_ref`).
/// A case's level is derived from state (2 for her); the level-3 cases of the tests below are raised by the CALL's own
/// level (`change_call(.., 3, ..)`), which the runner never lowers, so `_level` only documents the case.
pub(crate) async fn case_as_ann(fx: &Fx, case: &str, conv: &str, _level: i16) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', 'tg:42')",
    )
    .bind(t)
    .bind(j)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let draft_rev = match current_design(fx).await {
        agent_api::design::CaseDesign::Draft { draft_rev } => Some(draft_rev),
        agent_api::design::CaseDesign::Live { .. } => None,
    };
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                                 qualified_release_id, qualified_draft_rev, status) \
         VALUES ($1, $2, $3, $4, 'web_chat', $5, NULL, $6, NULL, $6, 'open')",
    )
    .bind(t)
    .bind(j)
    .bind(case)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .bind(draft_rev)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    crate::common::state::identify(&fx.d.e.owner, t, j, conv, "C-1", Some("ann@example.org")).await;
    let msg: Uuid = sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'please change my phone number', $4, 1) RETURNING message_id",
    )
    .bind(t)
    .bind(j)
    .bind(conv)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    msg.to_string()
}

/// The case's customer-context snapshot (what the context phase writes): Ann's own contact point is listed, so the
/// ownership rule of `contact_point.update` (`Owned::Listed` on `contact_point.list.contact_point_id`) passes.
pub(crate) async fn put_contacts(fx: &Fx, case: &str) {
    let ctx = json!({"party": "C-1", "orders": [], "payments": [], "deliveries": [], "cases": [],
                     "degraded": [], "records": {"contact_point.list": [{"contact_point_id": CONTACT_OWN}]}});
    let key = agent_api::runtime::case::CaseStore::new(fx.bus.clone()).key(case);
    let _: () = redis::cmd("HSET")
        .arg(key)
        .arg("ctx")
        .arg(ctx.to_string())
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
}

fn change_input() -> Value {
    json!({"contact_point_id": CONTACT_OWN, "value": "+31 6 12345678"})
}

pub(crate) fn change_call(fx: &Fx, case: &str, conv: &str, identity: u8, confirm: &str) -> OpCall {
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, "change", 1),
        "case_id": case, "conversation_id": conv, "phase": 0, "step": "change",
        "attempt": 1, "agent_role": "procedure", "operation": OP, "variant": null,
        "input": change_input(), "idempotency_key": format!("{case}:0:change"), "env": fx.scope.env,
        "release_id": null, "draft_rev": null, "confirm_ref": confirm, "initiator": "procedure@test",
        "identity_level": identity, "deadline_ms": 0
    }))
    .unwrap()
}

// The former `a_level_3_contact_change_by_a_level_2_customer_hands_over_and_never_asks` is deleted by design (A2):
// the runner no longer compares a case identity LEVEL with the catalogue's 3. Step 3 requires conversation STATE keys
// (the op's `requires`, else `subject` for a level-1+ op), so a known customer (`subject` in state) passes it and a
// contact change is gated by confirmation; there is no step-up refusal (`identity_required`) to hand over on unless
// a job's policy requires a key.

/// A known customer's confirmed change of her own contact point reaches the system of record once, with her own
/// contact point (same snapshot, same input, same confirm as before A2; the call's level 3 is only documentation now).
#[tokio::test]
async fn a_level_3_case_changes_its_own_contact_point() {
    let w = world("step-up-contact-l3").await;
    let fx = &w.fx;
    let msg = case_as_ann(fx, "CS-3", "conv-3", 3).await;
    put_contacts(fx, "CS-3").await;
    let res = w
        .runner
        .handle(&change_call(fx, "CS-3", "conv-3", 3, &msg))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(res.outcome_code, None, "{res:?}");
    assert_eq!(w.contact.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        w.contact.inputs.lock().unwrap()[0]["contact_point_id"],
        CONTACT_OWN
    );
}
