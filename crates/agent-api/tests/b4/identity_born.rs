//! B4 known-customer identity over real Postgres 18 + Redis, the runtime as `agent_runtime`: a case is born at level 2
//! only for an identified conversation (`identity_source` assertion / staff_pick), never from a channel's own claim,
//! and a connector `case.create` names the customer only from the runtime: the conversation state's `email`, never a
//! value the call (the model) carries. The tenant's systems authorize records; the runner holds no ownership table.
use crate::common::platform;
use std::sync::{Arc, Mutex};

use agent_api::conversations::state;
use agent_api::conversations::store::{self, Conv};
use agent_api::runtime::Env;
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::{OpCall, OpState as ResultState};
use agent_api::runtime::routes::{DEMO_STUB_INSTANCE, ResolvedTarget};
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, Served, outcome,
};
use agent_api::turn::drain::identity_of;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::b4::design::put_target;
use crate::b4::flow::{Fx, current_design};
use crate::redis_env as r;

const ANN_EMAIL: &str = "ann@example.org";

fn conv_with(channel: &str, customer: Option<&str>, source: Option<&str>) -> Conv {
    Conv {
        id: "conv-1".into(),
        channel: channel.into(),
        env: "prod".into(),
        customer_id: customer.map(str::to_owned),
        customer_name: Some("Ann".into()),
        initiator_sub: None,
        identity_source: source.map(str::to_owned),
        customer_email: Some(ANN_EMAIL.into()),
        identity_strength: None,
    }
}

/// A Telegram (or web chat) sender marked verified by its channel (`WireParty.verified` is a wire body field,
/// it is not a conversation column) starts at level 0 and is born with no party: only the persisted
/// `identity_source` counts. Mutation: `identity_of` honouring the channel's verified flag, or `customer_id` alone.
#[test]
fn a_wire_claimed_verified_sender_starts_at_level_0_with_no_party() {
    for channel in ["telegram", "web_chat", "staff"] {
        let conv = conv_with(channel, Some("tg:42"), None);
        assert_eq!(identity_of(&conv), 0, "{channel}");
        assert_eq!(conv.verified(), (None, None), "{channel}");
    }
}

/// The one rule: `customer_id` set AND `identity_source` in (assertion, staff_pick), on any channel, is level 2 with
/// the customer as the case party and the conversation's email. Anything else (no source, an unknown source, a blank
/// or missing customer) is level 0. Mutations: staff-only, either column alone, a source outside the CHECK list.
#[test]
fn an_identified_conversation_starts_at_level_2_with_its_party() {
    for channel in ["web_chat", "staff", "telegram"] {
        for source in ["assertion", "staff_pick"] {
            let conv = conv_with(channel, Some(" C-1 "), Some(source));
            assert_eq!(identity_of(&conv), 2, "{channel}/{source}");
            assert_eq!(
                conv.verified(),
                (Some("C-1".into()), Some(ANN_EMAIL.into())),
                "{channel}/{source}"
            );
        }
    }
    for (customer, source) in [
        (Some("C-1"), None),
        (Some("C-1"), Some("wire")),
        (Some("  "), Some("assertion")),
        (None, Some("assertion")),
    ] {
        let conv = conv_with("web_chat", customer, source);
        assert_eq!(identity_of(&conv), 0, "{customer:?}/{source:?}");
        assert_eq!(conv.verified(), (None, None), "{customer:?}/{source:?}");
    }
    let mut no_email = conv_with("web_chat", Some("C-1"), Some("assertion"));
    no_email.customer_email = Some(" ".into());
    assert_eq!(no_email.verified(), (Some("C-1".into()), None));
}

/// A conversation of `customer` / `source` on web chat, and its case born through the production insert
/// (`store::create_case`, the drain's): the case id.
async fn born_case(
    fx: &Fx,
    conv_id: &str,
    customer: &str,
    source: Option<&str>,
    email: Option<&str>,
) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', $5)",
    )
    .bind(t)
    .bind(j)
    .bind(conv_id)
    .bind(fx.scope.env.as_str())
    .bind(customer)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    if let Some(source) = source {
        crate::common::state::identify_as(&fx.d.e.owner, t, j, conv_id, source, customer, email)
            .await;
    }
    let mut tx = store::begin_job(&fx.d.e.state.db, t, j).await.unwrap();
    let conv = store::conversation_or_404(&mut tx, conv_id).await.unwrap();
    let case = store::create_case(
        &mut tx,
        j,
        &conv,
        "where is my order",
        &current_design(fx).await,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    case
}

/// An identified (assertion) conversation's case is born at level 2 bound to its customer and KYC email; an
/// unidentified one is born at level 0 with no party. Mutation: the case born at 0 for an assertion.
#[tokio::test]
async fn a_case_is_born_at_the_level_its_conversation_is_identified_at() {
    let fx = crate::b4::flow::setup_latest("identity-born", Env::Test).await;
    let born = |case: String| {
        let fx = &fx;
        async move {
            crate::common::state::case_identity(
                &fx.d.e.owner,
                fx.d.f.tenant,
                fx.d.f.job.job_id,
                &case,
            )
            .await
        }
    };
    let known = born_case(&fx, "conv-1", "C-1", Some("assertion"), Some(ANN_EMAIL)).await;
    assert_eq!(
        born(known).await,
        (2, Some("C-1".into()), Some(ANN_EMAIL.into()))
    );
    let anon = born_case(&fx, "conv-2", "tg:42", None, None).await;
    assert_eq!(born(anon).await, (0, None, None));
}

// ---- customer_email of a connector case.create: only the runtime sets it, from the conversation state -------------

/// The instance executor: says which operation serves a call ([`OpExecutor::served`]) and records the inputs of every
/// call that reaches it.
struct ServedBy {
    serves: Mutex<Served>,
    seen: Mutex<Vec<Value>>,
}

impl ServedBy {
    fn new() -> Arc<ServedBy> {
        Arc::new(ServedBy {
            serves: Mutex::new(Served {
                op_key: String::new(),
                needs: Vec::new(),
            }),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn serve(&self, op_key: &str) {
        *self.serves.lock().unwrap() = Served {
            op_key: op_key.to_owned(),
            needs: Vec::new(),
        };
    }

    fn last(&self) -> Value {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or(Value::Null)
    }
}

impl OpExecutor for ServedBy {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push(Value::Object(req.inputs.clone()));
            outcome(
                OpState::Ok,
                None,
                None,
                json!({"case_id": "EXT-1", "status": "open"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
        })
    }

    fn served<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, Result<Option<Served>, String>> {
        let s = self.serves.lock().unwrap().clone();
        Box::pin(std::future::ready(Ok(Some(s))))
    }
}

/// A conversation on web chat and its open case `case`; one inbound customer message, whose id is returned.
async fn case_on(fx: &Fx, case: &str, conv: &str) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', 'C-1') ON CONFLICT DO NOTHING",
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
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'please help', $4, 1) RETURNING message_id",
    )
    .bind(t)
    .bind(j)
    .bind(conv)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    id.to_string()
}

/// Set the conversation state `key` = `value` (plain, produced by the channel).
async fn put_state(fx: &Fx, conv: &str, key: &str, value: &str) {
    let mut tx = store::begin_job(&fx.d.e.state.db, fx.d.f.tenant, fx.d.f.job.job_id)
        .await
        .unwrap();
    state::put_value(&mut tx, conv, key, value, "channel:web_chat", None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// A working `operation_route` of the fixture's job to the stub instance.
async fn route_to_stub(fx: &Fx, object: &str, verb: &str) {
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, updated_by) \
         VALUES ($1, $2, $3, $4, '*', $5, 'instance', $6, 'u-test') \
         ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
         DO UPDATE SET target_kind = 'instance', instance_id = EXCLUDED.instance_id",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(object)
    .bind(verb)
    .bind(fx.scope.env.as_str())
    .bind(DEMO_STUB_INSTANCE)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

fn call(fx: &Fx, case: &str, conv: &str, step: &str, input: Value, confirm: &str) -> OpCall {
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, step, 1),
        "case_id": case, "conversation_id": conv, "phase": 0, "step": step,
        "attempt": 1, "agent_role": "procedure", "operation": "case.create", "variant": null,
        "input": input, "idempotency_key": format!("{case}:0:{step}"), "env": fx.scope.env,
        "release_id": null, "draft_rev": null, "confirm_ref": confirm, "initiator": "procedure@test",
        "identity_level": 2, "deadline_ms": 0
    }))
    .unwrap()
}

/// A connector `case.create` / handover that names `victim@…` goes to the connector with the conversation state's
/// `email`, never the named one. Mutation: keeping the call's `customer_email` raises the ticket as the victim. A
/// conversation whose state holds no `email` sends none: the template's service user answers.
#[tokio::test]
async fn a_connector_ticket_is_raised_as_the_states_email_never_a_named_one() {
    let fx = crate::b4::flow::setup_latest("identity-ticket-email", Env::Test).await;
    put_target(&fx.d).await;
    platform::stub_shop(&fx.d.e, fx.d.f.tenant, DEMO_STUB_INSTANCE).await;
    route_to_stub(&fx, "case", "create").await;
    let by = ServedBy::new();
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let runner = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-identity-born")
        .with_consume(r::fast())
        .with_executors(Executors::default().with_instance(by.clone()));
    let named = json!("victim@example.test");

    // Ann: the state names her email
    let msg = case_on(&fx, "CS-1", "conv-1").await;
    put_state(&fx, "conv-1", state::SUBJECT, "C-1").await;
    put_state(&fx, "conv-1", state::EMAIL, ANN_EMAIL).await;
    by.serve("case.create");
    let res = runner
        .handle(&call(
            &fx,
            "CS-1",
            "conv-1",
            "t1",
            json!({"case_type": "complaint", "subject": "s", "description": "d", "customer_email": named}),
            &msg,
        ))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(by.last()["customer_email"], ANN_EMAIL);
    by.serve("case.create{handover}");
    let mut handover = call(
        &fx,
        "CS-1",
        "conv-1",
        "t2",
        json!({"subject": "help", "customer_email": named}),
        &msg,
    );
    handover.variant = Some("handover".into());
    let res = runner.handle(&handover).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(
        by.last(),
        json!({"subject": "help", "customer_email": ANN_EMAIL})
    );

    // a conversation whose state holds no email: the model's value is dropped, none is sent
    let msg2 = case_on(&fx, "CS-2", "conv-2").await;
    put_state(&fx, "conv-2", state::SUBJECT, "C-2").await;
    let mut handover = call(
        &fx,
        "CS-2",
        "conv-2",
        "u1",
        json!({"subject": "help", "customer_email": named}),
        &msg2,
    );
    handover.variant = Some("handover".into());
    let res = runner.handle(&handover).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(by.last(), json!({"subject": "help"}));
}
