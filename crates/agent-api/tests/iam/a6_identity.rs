//! A6 (owner 2026-11-01, plan 2026-11-01 § A): a tenant IAM procedure turns an anonymous conversation into a known
//! person, at the strength the procedure declares. A `verified` procedure (a one-time code) makes the conversation a
//! verified customer (level 2, `known_sql` true); an `identified` one makes it a known person at level 1 — the
//! verified-only reads (KYC context, party matching) stay closed. Real Postgres 18 + Redis + the API router + the
//! runner; the only fake is the tenant's system on loopback.
//!
//! The catalogue level is a runner gate for the tenant IAM identity (owner 2026-11-01): an `identified` (level 1)
//! person never runs a level-2 op, while a `verified` (level 2) one does. Every other identity source keeps A2's
//! state rule, a machine caller keeps its behaviour, and the identity chain's own steps are exempt.
use std::sync::Arc;

use agent_api::conversations::{state, store};
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::{OP_CALL, OpCall};
use agent_api::runtime::runner::Runner;
use agent_api::runtime::{Env, Envelope, Scope};
use serde_json::{Value, json};

use super::world::*;

/// The serving tool: `party_id` is the conversation's subject.
fn serving_policy() -> Value {
    json!({"inputs": {"party_id": {"source": "state", "key": "subject", "from": "state"}}})
}

/// A two-step sign-in that establishes the person: the CRM lookup asks for the email and writes `subject`/`email`;
/// the verify step asks for the one-time code and produces the token the lookup consumes.
fn signin_to_a_person() -> Value {
    json!([
        {"op": VERIFY, "inputs": {"msisdn": {"source": "state", "key": "one_time_code",
                                            "from": {"ask_user": PROMPT, "secret": true, "retries": 2}}},
         "outputs": {"porting_id": {"key": "auth_token", "kind": "secret", "ttl_s": 3600}}},
        {"op": LOOKUP, "inputs": {
            "email": {"source": "state", "key": "email",
                      "from": {"ask_user": "What is your email address?"}},
            "order_id": {"source": "state", "key": "auth_token",
                         "from": {"tool": VERIFY, "output": "porting_id"}}},
         "outputs": {"party_id": {"key": "subject"}, "mail": {"key": "email"}}},
    ])
}

async fn runner(w: &World) -> Runner {
    let st = &w.e.state;
    let scope = Scope::new(w.f.tenant, w.f.job.job_key.clone(), Env::Test).unwrap();
    let bus = st.redis.clone().expect("REDIS_URL set").scoped(&scope);
    let rt = crate::common::runtime_pool(&w.e.owner).await;
    let mut rs = st.clone();
    rs.db = rt;
    Runner::new(
        bus,
        rs,
        Arc::new(DesignCache::new(std::time::Duration::from_secs(60))),
    )
    .unwrap()
}

/// An anonymous web chat conversation of the job, with its web chat session row (the authority a reload reads).
async fn anonymous_conversation(w: &World, conv: &str, case: &str) -> String {
    let f = &w.f;
    let binding: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, env, kind, side, external_id, created_by, verified_at) \
         VALUES ($1, $2, 'test', 'web_chat', 'customer', 'wk_test', 'u-int', now()) RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    let sid: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO web_chat_session (tenant_id, job_id, binding_id, token_hash, origin, ip_hash, expires_at) \
         VALUES ($1, $2, $3, sha256('t'::bytea), 'https://shop.example', sha256('ip'::bytea), \
                 now() + interval '1 hour') RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(binding)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    let placeholder = format!("wc:{sid}");
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, 'test', 'web_chat', $4)",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(conv)
    .bind(&placeholder)
    .execute(&w.e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE web_chat_session SET conversation_id = $2 WHERE id = $1")
        .bind(sid)
        .bind(conv)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let draft_rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, \
                                 qualified_draft_rev, status) \
         VALUES ($1, $2, $3, $4, 'web_chat', 'test', $5, $5, 'open')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(case)
    .bind(conv)
    .bind(draft_rev)
    .execute(&w.e.owner)
    .await
    .unwrap();
    placeholder
}

fn call(case: &str, conv: &str, step: &str, input: Value) -> OpCall {
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, step, 1),
        "case_id": case, "conversation_id": conv, "phase": 0, "step": step, "attempt": 1,
        "agent_role": "assistant", "operation": READ, "variant": null,
        "input": input, "idempotency_key": format!("{case}:0:{step}"),
        "env": serde_json::to_value(Env::Test).unwrap(), "release_id": null, "draft_rev": null,
        "confirm_ref": null, "initiator": "assistant@test", "identity_level": 0, "deadline_ms": 0
    }))
    .unwrap()
}

/// `(state, outcome code, detail)` of a call's row.
async fn outcome(w: &World, call_id: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, outcome_code, detail FROM operation_call WHERE call_id = $1")
        .bind(call_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap()
}

async fn put_state(w: &World, conv: &str, key: &str, value: &str) {
    let mut tx = store::begin_job(&w.e.state.db, w.f.tenant, w.f.job.job_id)
        .await
        .unwrap();
    state::put_value(&mut tx, conv, key, value, "test:user", None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn attach_signin(w: &World, strength: &str) -> String {
    let (s, p) = w
        .tenant_call(
            "POST",
            "/api/iam-procedures",
            json!({"name": format!("Acme sign-in ({strength})"), "strength": strength,
                   "steps": signin_to_a_person()}),
        )
        .await;
    assert_eq!(s, 201, "{p}");
    let id = p["id"].as_str().unwrap().to_owned();
    assert_eq!(p["strength"], strength, "{p}");
    let (s, r) = w
        .call(&w.f, "PUT", &format!("iam-procedures/{id}"), json!({}))
        .await;
    assert!(s.is_success(), "{s} {r}");
    id
}

/// Run the serving call through the chain: the first call parks on the email question, the second on the code, the
/// third runs the whole chain and the serving tool. Returns the three call ids in order.
async fn sign_in(w: &World, conv: &str, case: &str) -> Vec<String> {
    let runner = runner(w).await;
    let mut ids = Vec::new();
    let first = call(case, conv, "s1", json!({}));
    runner
        .on_call(
            Envelope::new(
                &format!("order-support.anything_else.{OP_CALL}"),
                "orchestrator",
                case,
                &first,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let (state, code, detail) = outcome(w, &first.call_id).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("error", Some("input_required")),
        "{detail:?}"
    );
    ids.push(first.call_id);
    put_state(w, conv, state::EMAIL, CUSTOMER_EMAIL).await;

    let second = call(case, conv, "s2", json!({}));
    runner
        .on_call(
            Envelope::new(
                &format!("order-support.anything_else.{OP_CALL}"),
                "orchestrator",
                case,
                &second,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let (state, code, detail) = outcome(w, &second.call_id).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("error", Some("input_required")),
        "{detail:?}"
    );
    ids.push(second.call_id);
    put_state(w, conv, "one_time_code", CODE).await;

    let third = call(case, conv, "s3", json!({}));
    runner
        .on_call(
            Envelope::new(
                &format!("order-support.anything_else.{OP_CALL}"),
                "orchestrator",
                case,
                &third,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    ids.push(third.call_id);
    ids
}

/// A verified procedure makes an anonymous web chat a verified customer (level 2): the chain asks the email and the
/// one-time code, the two steps run, the identity is state, the web chat session row is filled and the serving tool
/// runs. Mutation: `known_sql` accepting an `identified` IAM source, or the upgrade never writing the strength.
#[tokio::test]
async fn a_verified_procedure_makes_an_anonymous_web_chat_a_verified_customer() {
    let w = World::build("iam-a6-verified", say_only(ANSWER), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    w.policy_ok(f, READ, serving_policy()).await;
    attach_signin(&w, "verified").await;
    let placeholder = anonymous_conversation(&w, "conv-a6", "CS-961").await;

    let ids = sign_in(&w, "conv-a6", "CS-961").await;
    let (state, code, detail) = outcome(&w, &ids[2]).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("ok", None),
        "the serving call answered: {detail:?}"
    );
    assert_eq!(
        w.sys.paths(),
        ["/porting/read", "/party/list", "/order/list"]
    );

    // the conversation is a verified customer, at level 2
    let (level, party, email) =
        crate::common::state::case_identity(&w.e.owner, f.tenant, f.job.job_id, "CS-961").await;
    assert_eq!(
        (level, party.as_deref(), email.as_deref()),
        (2, Some(CUSTOMER), Some(CUSTOMER_EMAIL))
    );
    let mut tx = store::begin_job(&w.e.state.db, f.tenant, f.job.job_id)
        .await
        .unwrap();
    let conv = store::conversation_or_404(&mut tx, "conv-a6")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(conv.identity_strength.as_deref(), Some("verified"));
    assert_eq!(conv.known_customer(), Some(CUSTOMER));
    assert_eq!(conv.identity_level(), 2);

    // the web chat session row holds the identity, and the wire column follows it
    let (ref_, mail, identified): (Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT customer_ref, customer_email, identified_at IS NOT NULL FROM web_chat_session \
          WHERE conversation_id = 'conv-a6'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        (ref_.as_deref(), mail.as_deref(), identified),
        (Some(CUSTOMER), Some(CUSTOMER_EMAIL), true)
    );
    // the effective customer id is the state subject (the columns COALESCE it over the wire column); the anonymous
    // party column itself stays, which is the party row the conversation attached
    assert_eq!(conv.customer_id.as_deref(), Some(CUSTOMER));
    let column: String = sqlx::query_scalar(
        "SELECT customer_id FROM conversation WHERE conversation_id = 'conv-a6'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        column, placeholder,
        "the wire column keeps the anonymous party"
    );

    // one audit row, with the procedure, strength and step — never the person
    let details: String = sqlx::query_scalar(
        "SELECT details::text FROM audit_event WHERE action = 'conversation.identity_upgraded'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(details.contains("\"strength\": \"verified\""), "{details}");
    assert!(
        details.contains("\"previous_source\": \"none\""),
        "{details}"
    );
    assert!(
        !details.contains(CUSTOMER) && !details.contains(CUSTOMER_EMAIL),
        "{details}"
    );

    // the steps ran as the runtime's resolve sub-calls, the serving tool as the assistant's own
    let calls = w.calls(f).await;
    for op in [VERIFY, LOOKUP] {
        assert!(
            calls
                .iter()
                .any(|c| c.0 == op && c.1 == "ok" && c.3 == "resolve"),
            "{calls:?}"
        );
    }
    assert!(
        calls
            .iter()
            .any(|c| c.0 == READ && c.1 == "ok" && c.3 != "resolve"),
        "{calls:?}"
    );
}

/// The same procedure declared `identified` makes a known person at level 1 — not a verified customer: the KYC
/// context stays closed (`known_sql` false, `known_customer` none) while `subject` is there for the job's
/// declarations. Mutation: `level_sql` returning 2 for `identified`, or `known_sql` accepting it.
#[tokio::test]
async fn an_identified_procedure_makes_a_known_person_but_not_a_verified_customer() {
    let w = World::build("iam-a6-identified", say_only(ANSWER), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    w.policy_ok(f, READ, serving_policy()).await;
    attach_signin(&w, "identified").await;
    anonymous_conversation(&w, "conv-a6i", "CS-962").await;

    let ids = sign_in(&w, "conv-a6i", "CS-962").await;
    let (state, code, detail) = outcome(&w, &ids[2]).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("denied", Some("identity_required")),
        "a level-2 tool is refused to an identified (level 1) person: {detail:?}"
    );
    assert!(
        !w.sys.paths().iter().any(|p| p == "/order/list"),
        "the refused serving tool never reached the system: {:?}",
        w.sys.paths()
    );

    let (level, party, email) =
        crate::common::state::case_identity(&w.e.owner, f.tenant, f.job.job_id, "CS-962").await;
    assert_eq!(
        (level, party, email),
        (1, None, None),
        "level 1, not a verified customer"
    );
    let mut tx = store::begin_job(&w.e.state.db, f.tenant, f.job.job_id)
        .await
        .unwrap();
    let conv = store::conversation_or_404(&mut tx, "conv-a6i")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(conv.identity_strength.as_deref(), Some("identified"));
    // the person IS known at level 1: the subject is there and the level says so
    let mut tx = store::begin_job(&w.e.state.db, f.tenant, f.job.job_id)
        .await
        .unwrap();
    let state_now = state::load(&mut tx, "conv-a6i").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(state::identified_subject(&state_now), Some(CUSTOMER));
    assert_eq!(state::known_subject(&state_now), None);
    assert_eq!(conv.identity_level(), 1);
    assert_eq!(conv.identified_subject(), Some(CUSTOMER));
    assert_eq!(conv.known_customer(), None);

    // the web chat session keeps the person (a reload is a known person, not a stranger)
    let ref_: Option<String> = sqlx::query_scalar(
        "SELECT customer_ref FROM web_chat_session WHERE conversation_id = 'conv-a6i'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(ref_.as_deref(), Some(CUSTOMER));
}

/// The gate is the conversation's IAM strength, not the tool policy: an IAM-`identified` person is refused a level-2
/// op even when the policy declares no `requires` (A2's state rule alone would let it through). A `verified` person
/// runs the same call. Mutation: the gate skipped for a policy with no `requires`, or taken from the call instead of
/// the conversation's state.
#[tokio::test]
async fn an_identified_person_cannot_run_a_verified_tool_without_requires() {
    let w = World::build("iam-a6-level", say_only(ANSWER), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    w.policy_ok(f, READ, json!({"requires": []})).await;
    anonymous_conversation(&w, "conv-a6l", "CS-963").await;
    put_state(&w, "conv-a6l", state::SUBJECT, CUSTOMER).await;
    put_state(&w, "conv-a6l", state::IDENTITY_SOURCE, state::SOURCE_IAM).await;
    put_state(
        &w,
        "conv-a6l",
        state::IDENTITY_STRENGTH,
        state::STRENGTH_IDENTIFIED,
    )
    .await;

    let rn = runner(&w).await;
    let denied = call("CS-963", "conv-a6l", "l1", json!({}));
    rn.on_call(
        Envelope::new(
            &format!("order-support.anything_else.{OP_CALL}"),
            "orchestrator",
            "CS-963",
            &denied,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let (state, code, detail) = outcome(&w, &denied.call_id).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("denied", Some("identity_required")),
        "{detail:?}"
    );
    assert!(
        w.sys.paths().is_empty(),
        "a refused call never reaches the system: {:?}",
        w.sys.paths()
    );

    // verified: the same call runs (with the input the connector needs: `party_id` from state)
    put_state(
        &w,
        "conv-a6l",
        state::IDENTITY_STRENGTH,
        state::STRENGTH_VERIFIED,
    )
    .await;
    w.policy_ok(f, READ, serving_policy()).await;
    let rn = runner(&w).await;
    let allowed = call("CS-963", "conv-a6l", "l2", json!({}));
    rn.on_call(
        Envelope::new(
            &format!("order-support.anything_else.{OP_CALL}"),
            "orchestrator",
            "CS-963",
            &allowed,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let (state, code, detail) = outcome(&w, &allowed.call_id).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("ok", None),
        "{detail:?}"
    );
    assert_eq!(w.sys.paths(), ["/order/list"]);
}

/// One conversation is one person (owner 2026-11-01): a procedure that establishes a DIFFERENT subject on a
/// conversation that already has one writes nothing, fails the call `identity_conflict` (so the agent hands over
/// instead of proceeding as the wrong person) and records no identity upgrade. Mutation: overwriting the subject.
#[tokio::test]
async fn a_second_person_is_refused_and_nothing_changes() {
    let w = World::build("iam-a6-conflict", say_only(ANSWER), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    attach_signin(&w, "verified").await;
    let conv = w.start_chat(f).await; // the staff test chat: known as CUSTOMER_EMAIL (staff_test), level 2
    put_state(&w, &conv, state::SUBJECT, "C-1").await;
    put_state(&w, &conv, state::IDENTITY_SOURCE, state::SOURCE_ASSERTION).await;

    // the procedure's lookup step, called directly: it produces the subject CUST-4711 (a different person)
    let runner = runner(&w).await;
    let mut step = call(
        "CS-conf",
        &conv,
        "conf",
        json!({"email": CUSTOMER_EMAIL, "order_id": TOKEN}),
    );
    step.operation = LOOKUP.to_owned();
    step.agent_role = "procedure".to_owned();
    step.identity_level = 2;
    let id = step.call_id.clone();
    runner
        .on_call(
            Envelope::new(
                &format!("order-support.anything_else.{OP_CALL}"),
                "orchestrator",
                "CS-conf",
                &step,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let (state, code, detail) = outcome(&w, &id).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("error", Some("identity_conflict")),
        "{detail:?}"
    );

    let mut tx = store::begin_job(&w.e.state.db, f.tenant, f.job.job_id)
        .await
        .unwrap();
    let after = store::conversation_or_404(&mut tx, &conv).await.unwrap();
    let state_now = state::load(&mut tx, &conv).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        after.customer_id.as_deref(),
        Some("C-1"),
        "the subject did not change"
    );
    assert_eq!(state::known_subject(&state_now), Some("C-1"));
    assert_eq!(
        state_now.get(state::IDENTITY_STRENGTH),
        None,
        "no strength was written"
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'conversation.identity_upgraded'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 0);
}

/// One conversation is one person, so a job has one identity procedure (owner 2026-11-01): attaching a second
/// procedure that also establishes the subject is refused `identity_already_produced` and nothing is attached.
/// Mutation: the attach route accepting both (the design could then not say which strength wrote the person).
#[tokio::test]
async fn a_job_has_one_identity_procedure() {
    let w = World::build("iam-a6-one-procedure", say_only(ANSWER), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    let one = attach_signin(&w, "identified").await;
    let (s, p) = w
        .tenant_call(
            "POST",
            "/api/iam-procedures",
            json!({"name": "Acme sign-in (second)", "strength": "verified", "steps": signin_to_a_person()}),
        )
        .await;
    assert_eq!(s, 201, "{p}");
    let two = p["id"].as_str().unwrap().to_owned();
    let (s, r) = w
        .call(f, "PUT", &format!("iam-procedures/{two}"), json!({}))
        .await;
    assert_eq!(
        (s.as_u16(), r["error"]["code"].as_str()),
        (422, Some("identity_already_produced")),
        "{r}"
    );
    // the first is still the only one attached
    let (s, list) = w.call(f, "GET", "iam-procedures", json!({})).await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list, json!([one]));
    // detached first, the second attaches
    let (s, r) = w
        .call(f, "DELETE", &format!("iam-procedures/{one}"), json!({}))
        .await;
    assert!(s.is_success(), "{s} {r}");
    let (s, r) = w
        .call(f, "PUT", &format!("iam-procedures/{two}"), json!({}))
        .await;
    assert!(s.is_success(), "{s} {r}");
}
