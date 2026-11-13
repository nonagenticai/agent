//! B4 T8 — the interaction daemon (crate::runtime::interaction): one reply per case however often the resolution
//! is delivered, the reply + grounding + armed CSAT request in one transaction (§0 R3), withholding while staff hold the
//! conversation or a handover item is open, and the test-env reply trace. Real Postgres 18 (as `agent_runtime`, the
//! daemon's role) + Redis, a job created from the template through the real router.
use std::pin::Pin;
use std::time::Duration;

use agent_api::design;
use agent_api::runtime::interaction::{
    ChatFrame, Delivery, Interaction, REPLIED_TTL_SECS, replied_key,
};
use agent_api::runtime::trace::{Delta, exercised_and_differs};
use agent_api::runtime::{Bus, Env, Envelope, Scope};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv};
use crate::redis_env as r;

const CASE: &str = "CS-1";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    bus: Bus,
    conv: String,
    daemon: Interaction,
}

/// A job, one conversation + case `CS-1` with `status` in `env`, and the daemon of that scope on `pool` (None =
/// `agent_runtime`).
async fn setup_with(test: &str, e: TestEnv, f: ContractJob, env: Env, status: &str) -> Fx {
    let scope = Scope::new(f.tenant, f.job.job_key.clone(), env).unwrap();
    let bus = r::root(test).await.scoped(&scope);
    let conv = format!("conv-{}", Uuid::new_v4().simple());
    let draft_rev: Option<i64> = match env {
        Env::Test => Some(
            sqlx::query_scalar::<_, i64>("SELECT draft_rev FROM jobs WHERE id = $1")
                .bind(f.job.job_id)
                .fetch_one(&e.owner)
                .await
                .unwrap(),
        ),
        Env::Prod => None,
    };
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
         VALUES ($1, $2, $3, $4, 'staff')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(&conv)
    .bind(env.as_str())
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, status) \
         VALUES ($1, $2, $3, $4, 'staff', $5, $6, $7)",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(CASE)
    .bind(&conv)
    .bind(env.as_str())
    .bind(draft_rev)
    .bind(status)
    .execute(&e.owner)
    .await
    .unwrap();
    let pool = common::runtime_pool(&e.owner).await;
    let daemon = Interaction::new(pool, e.state.design.clone(), bus.clone()).unwrap();
    Fx {
        e,
        f,
        bus,
        conv,
        daemon,
    }
}

async fn setup(test: &str, env: Env, status: &str) -> Fx {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    setup_with(test, e, f, env, status).await
}

/// A `request.resolved` envelope of `CS-1` (a completed return) with the daemon's extras `extra`.
fn resolved(fx: &Fx, extra: Value) -> Envelope {
    resolved_case(fx, CASE, extra)
}

/// [`resolved`] for case `case_id` (see [`add_case`] for a second one).
fn resolved_case(fx: &Fx, case_id: &str, extra: Value) -> Envelope {
    let mut p = json!({
        "case_id": case_id,
        "conversation_id": fx.conv,
        "context_space": "retail:order-support:returns",
        "request_type": "return",
        "outcome": "resolved",
        "facts": {
            "order": {"item": "kettle"},
            "rma": {"rma_id": "RMA-7", "carrier": "DHL"},
            "window": {"rule": "RET-01", "outcome": "eligible", "code": "eligible"},
        },
    });
    if let (Some(p), Some(x)) = (p.as_object_mut(), extra.as_object()) {
        p.extend(x.clone());
    }
    Envelope::new(
        "order-support.returns.request.resolved",
        "orchestrator",
        case_id,
        p,
    )
    .unwrap()
}

async fn deliver(fx: &Fx, env: &Envelope) -> anyhow::Result<Delivery> {
    Ok(fx
        .daemon
        .handle(env)
        .await?
        .expect("request.resolved is handled"))
}

type Frames = Pin<Box<dyn Stream<Item = redis::Msg>>>;

/// Subscribe the scope's `chat` channel (before the delivery, so no frame is missed).
async fn listen(bus: &Bus) -> Frames {
    let mut ps = bus.client().get_async_pubsub().await.unwrap();
    ps.subscribe(bus.streams.chat()).await.unwrap();
    Box::pin(ps.into_on_message())
}

/// Every frame that arrives until the channel is quiet for `quiet`.
async fn frames(s: &mut Frames, quiet: Duration) -> Vec<ChatFrame> {
    let mut out = Vec::new();
    while let Ok(Some(m)) = tokio::time::timeout(quiet, s.next()).await {
        let text: String = m.get_payload().unwrap();
        out.push(serde_json::from_str(&text).unwrap());
    }
    out
}

async fn count(db: &PgPool, sql: &'static str, tenant: Uuid, key: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(tenant)
        .bind(key)
        .fetch_one(db)
        .await
        .unwrap()
}

/// The agent's own replies (not the CSAT prompt) in the conversation.
async fn replies(fx: &Fx) -> i64 {
    count(
        &fx.e.owner,
        "SELECT count(*) FROM conversation_message \
          WHERE tenant_id = $1 AND conversation_id = $2 AND role = 'agent' AND author = 'agent'",
        fx.f.tenant,
        &fx.conv,
    )
    .await
}

async fn events(fx: &Fx, kind: &'static str) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT body FROM case_event WHERE tenant_id = $1 AND case_id = $2 AND kind = $3 ORDER BY seq",
    )
    .bind(fx.f.tenant)
    .bind(CASE)
    .bind(kind)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap()
}

async fn claimed(fx: &Fx) -> bool {
    redis::cmd("EXISTS")
        .arg(replied_key(&fx.bus, CASE))
        .query_async::<i64>(&mut fx.bus.conn())
        .await
        .unwrap()
        == 1
}

#[tokio::test]
async fn redelivered_resolution_replies_once() {
    let fx = setup("t8-redeliver", Env::Prod, "open").await;
    let mut chat = listen(&fx.bus).await;
    let env = resolved(&fx, json!({}));
    let first = deliver(&fx, &env).await.unwrap();
    let Delivery::Replied {
        message_id,
        written,
        relayed,
        ..
    } = first
    else {
        panic!("{first:?}")
    };
    assert!(written && relayed, "{first:?}");
    let again = deliver(&fx, &env).await.unwrap();
    assert_eq!(
        again,
        Delivery::Replied {
            message_id,
            written: false,
            csat: false,
            relayed: false
        }
    );
    assert_eq!(replies(&fx).await, 1);
    assert_eq!(events(&fx, "reply").await.len(), 1);
    let got = frames(&mut chat, Duration::from_millis(500)).await;
    assert_eq!(got.len(), 1, "one chat frame: {got:?}");
    assert_eq!(got[0].event, "message");
    assert_eq!(got[0].conversation_id, fx.conv);
    assert_eq!(got[0].data["message_id"], json!(message_id));
    assert_eq!(
        got[0].data["text"],
        "Return approved for your kettle. We'll send the prepaid DHL label here as soon as it's ready."
    );
}

#[tokio::test]
async fn replied_key_ttl_is_seven_days() {
    let fx = setup("t8-ttl", Env::Prod, "open").await;
    deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(replied_key(&fx.bus, CASE))
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    assert_eq!(REPLIED_TTL_SECS, 604_800);
    assert!((604_790..=604_800).contains(&ttl), "ttl {ttl}");
}

/// The claim only gates the publish: a delivery that claimed `replied:{case}` and died before its transaction
/// committed leaves no reply, and the redelivery still writes it (it does not publish twice).
#[tokio::test]
async fn crash_after_claim_still_has_reply() {
    let fx = setup("t8-crash", Env::Prod, "open").await;
    let _: () = redis::cmd("SET")
        .arg(replied_key(&fx.bus, CASE))
        .arg("crashed")
        .arg("EX")
        .arg(600)
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(
        matches!(
            d,
            Delivery::Replied {
                written: true,
                relayed: false,
                ..
            }
        ),
        "{d:?}"
    );
    assert_eq!(
        replies(&fx).await,
        1,
        "the reply is stored despite the claim"
    );
    assert_eq!(events(&fx, "reply").await.len(), 1);
}

#[tokio::test]
async fn takeover_withholds_agent_reply() {
    let fx = setup("t8-takeover", Env::Prod, "open").await;
    sqlx::query(
        "INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub) \
         VALUES ($1, $2, $3, 'staff', 'u-staff')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let mut chat = listen(&fx.bus).await;
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert_eq!(
        d,
        Delivery::Withheld {
            reason: "staff_control"
        }
    );
    assert_eq!(replies(&fx).await, 0);
    assert!(events(&fx, "reply").await.is_empty());
    let states = events(&fx, "state").await;
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["note"], "agent reply withheld");
    assert_eq!(states[0]["reason"], "staff_control");
    assert!(!claimed(&fx).await, "nothing claimed");
    assert!(
        frames(&mut chat, Duration::from_millis(300))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn open_handover_item_withholds_agent_reply() {
    let fx = setup("t8-handover", Env::Prod, "handover").await;
    sqlx::query(
        "INSERT INTO agent_handover_items (tenant_id, job_id, id, case_id, conversation_id, env, reason) \
         VALUES ($1, $2, 'HO-1', $3, $4, 'prod', 'customer_asked')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(CASE)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    queue_owns(&fx).await;
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert_eq!(
        d,
        Delivery::Withheld {
            reason: "open_handover"
        }
    );
    assert_eq!(replies(&fx).await, 0);
}

/// SW-1, the one silence rule: the owner decides, not the item. An open item under an agent owner (no writer leaves
/// that state; here a raw insert) does not withhold the reply.
#[tokio::test]
async fn the_owner_not_an_item_decides_silence() {
    let fx = setup("t8-owner-rule", Env::Prod, "open").await;
    sqlx::query(
        "INSERT INTO agent_handover_items (tenant_id, job_id, id, case_id, conversation_id, env, reason) \
         VALUES ($1, $2, 'HO-1', $3, $4, 'prod', 'customer_asked')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(CASE)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(
        matches!(d, Delivery::Replied { written: true, .. }),
        "{d:?}"
    );
    assert_eq!(replies(&fx).await, 1);
}

/// The queue owns the fixture's conversation (SW-1: what `handover::create` writes with an item; a raw item insert
/// does not). The one silence rule reads the owner, not the item.
async fn queue_owns(fx: &Fx) {
    sqlx::query(
        "INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub) \
         VALUES ($1, $2, $3, 'queue', NULL) \
         ON CONFLICT (tenant_id, job_id, conversation_id) DO UPDATE SET mode = 'queue', by_sub = NULL",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

/// Queue an open handover item `id` of `case_id` in the fixture's conversation with `reason` (the case row must
/// exist: the item's foreign key), and make the queue its owner.
async fn open_item(fx: &Fx, id: &str, case_id: &str, reason: &str) {
    sqlx::query(
        "INSERT INTO agent_handover_items (tenant_id, job_id, id, case_id, conversation_id, env, reason) \
         VALUES ($1, $2, $3, $4, $5, 'prod', $6)",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(id)
    .bind(case_id)
    .bind(&fx.conv)
    .bind(reason)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    queue_owns(fx).await;
}

/// The orchestrator queues (and commits) a case's own handover item before it publishes the `handover`
/// resolution: that item must not swallow the case's "passed to a colleague" reply. Mutation: `withheld` ignoring
/// the resolution (any open item withholds) answers `Withheld { open_handover }` here.
#[tokio::test]
async fn own_handover_resolution_replies_despite_its_open_item() {
    let fx = setup("t8-own-handover", Env::Prod, "handover").await;
    open_item(&fx, "HO-1", CASE, "tool_failed").await;
    let d = deliver(&fx, &resolved(&fx, json!({"outcome": "handover"})))
        .await
        .unwrap();
    let Delivery::Replied { written, .. } = d else {
        panic!("{d:?}")
    };
    assert!(written, "the handover reply is stored");
    assert_eq!(replies(&fx).await, 1);
    assert_eq!(events(&fx, "reply").await.len(), 1);
    assert!(events(&fx, "state").await.is_empty(), "nothing withheld");
}

/// A `failed` case queues its own item (`tool_failed`) before it publishes the resolution: that item must not
/// swallow the case's "a colleague will follow up" reply. Mutation: the exemption keyed on `handover` alone answers
/// `Withheld { open_handover }` here.
#[tokio::test]
async fn own_failed_resolution_replies_despite_its_open_item() {
    let fx = setup("t8-own-failed", Env::Prod, "handover").await;
    open_item(&fx, "HO-1", CASE, "tool_failed").await;
    let d = deliver(
        &fx,
        &resolved(
            &fx,
            json!({"outcome": "failed", "code": "timeout", "reason": "order.read did not answer in time"}),
        ),
    )
    .await
    .unwrap();
    let Delivery::Replied { written, .. } = d else {
        panic!("{d:?}")
    };
    assert!(written, "the failure reply is stored");
    assert_eq!(replies(&fx).await, 1);
    assert!(events(&fx, "state").await.is_empty(), "nothing withheld");
}

/// The exemption is for the case's `handover` reply only: a `resolved` resolution of the same case, its own item
/// open, is still withheld (staff hold it). Mutation: the exemption dropping its outcome check replies here.
#[tokio::test]
async fn own_open_item_still_withholds_a_resolved_reply() {
    let fx = setup("t8-own-resolved", Env::Prod, "resolved").await;
    open_item(&fx, "HO-1", CASE, "tool_failed").await;
    let d = deliver(&fx, &resolved(&fx, json!({"outcome": "resolved"})))
        .await
        .unwrap();
    assert_eq!(
        d,
        Delivery::Withheld {
            reason: "open_handover"
        }
    );
    assert_eq!(replies(&fx).await, 0);
}

/// Another case's open item still holds the conversation for staff, even when this case ended `handover`.
/// Mutation: the exemption dropping its `case_id` match replies here.
#[tokio::test]
async fn other_cases_open_item_withholds_a_handover_resolution() {
    let fx = setup("t8-other-handover", Env::Prod, "handover").await;
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, status) \
         VALUES ($1, $2, 'CS-2', $3, 'staff', 'prod', NULL, 'handover')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    open_item(&fx, "HO-1", "CS-2", "tool_failed").await;
    let d = deliver(&fx, &resolved(&fx, json!({"outcome": "handover"})))
        .await
        .unwrap();
    assert_eq!(
        d,
        Delivery::Withheld {
            reason: "open_handover"
        }
    );
    assert_eq!(replies(&fx).await, 0);
}

/// The customer asked for a person: the request already stored the handover notice for the case, so the case's own
/// `handover` reply is withheld (no second notice). Mutation: the exemption ignoring the stored notice replies here.
#[tokio::test]
async fn a_stored_handover_notice_withholds_the_cases_handover_reply() {
    let fx = setup("t8-asked-handover", Env::Prod, "handover").await;
    open_item(&fx, "HO-1", CASE, "customer_asked").await;
    sqlx::query(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, author, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'agent', 'handover_notice', 'A colleague will take it from here.', $4, 1)",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(&fx.conv)
    .bind(CASE)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let d = deliver(&fx, &resolved(&fx, json!({"outcome": "handover"})))
        .await
        .unwrap();
    assert_eq!(
        d,
        Delivery::Withheld {
            reason: "open_handover"
        }
    );
    assert_eq!(replies(&fx).await, 0);
}

/// A procedure's own `case.create{handover}` queues with the default reason `customer_asked` and stores no notice:
/// the case's `handover` reply still reaches the customer. Mutation: keying the exemption on the reason withholds.
#[tokio::test]
async fn a_procedure_handover_without_a_notice_replies() {
    let fx = setup("t8-proc-handover", Env::Prod, "handover").await;
    open_item(&fx, "HO-1", CASE, "customer_asked").await;
    let d = deliver(&fx, &resolved(&fx, json!({"outcome": "handover"})))
        .await
        .unwrap();
    assert!(matches!(d, Delivery::Replied { .. }), "{d:?}");
    assert_eq!(replies(&fx).await, 1);
}

/// R3: the grounding is recorded in the reply's transaction. Mutation: a refusing `answer_grounding` insert rolls
/// the reply back with it (no message, no timeline event, no claim); once it is lifted the redelivery writes all.
#[tokio::test]
async fn answer_grounding_written_per_reply_in_the_reply_txn() {
    let fx = setup("t8-grounding", Env::Prod, "open").await;
    for sql in [
        "CREATE FUNCTION t8_refuse_grounding() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 't8_grounding_refused'; END $$",
        "CREATE TRIGGER t8_refuse BEFORE INSERT ON answer_grounding \
         FOR EACH ROW EXECUTE FUNCTION t8_refuse_grounding()",
    ] {
        sqlx::query(sql).execute(&fx.e.owner).await.unwrap();
    }
    let knowledge = json!({"knowledge": {"query": "return window", "citations": ["c-1"],
                                         "sources": [{"document_id": "d-1", "title": "Returns"}]}});
    let env = resolved(&fx, knowledge);
    assert!(
        deliver(&fx, &env).await.is_err(),
        "a failing grounding fails the delivery"
    );
    assert_eq!(
        replies(&fx).await,
        0,
        "the reply rolled back with its grounding"
    );
    assert!(events(&fx, "reply").await.is_empty());
    assert!(!claimed(&fx).await);

    sqlx::query("DROP TRIGGER t8_refuse ON answer_grounding")
        .execute(&fx.e.owner)
        .await
        .unwrap();
    let d = deliver(&fx, &env).await.unwrap();
    let Delivery::Replied { message_id, .. } = d else {
        panic!("{d:?}")
    };
    let row: (bool, Vec<String>, String, Option<String>) = sqlx::query_as(
        "SELECT used_knowledge, citations, grounding, knowledge_query FROM answer_grounding WHERE message_id = $1",
    )
    .bind(message_id)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            true,
            vec!["c-1".to_string()],
            "grounded".to_string(),
            Some("return window".to_string())
        )
    );
    assert_eq!(
        events(&fx, "reply").await[0]["message_id"],
        json!(message_id)
    );
}

/// The fixture's conversation arrived on `channel` (the fixture itself is `staff`, a channel CSAT never asks on).
async fn on_channel(fx: &Fx, channel: &str) {
    sqlx::query(
        "UPDATE conversation SET channel = $2 WHERE tenant_id = $1 AND conversation_id = $3",
    )
    .bind(fx.f.tenant)
    .bind(channel)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

/// The `state` of the conversation's CSAT request, when there is one.
async fn csat_state(fx: &Fx) -> Option<String> {
    sqlx::query_scalar("SELECT state FROM case_csat WHERE tenant_id = $1 AND conversation_id = $2")
        .bind(fx.f.tenant)
        .bind(&fx.conv)
        .fetch_optional(&fx.e.owner)
        .await
        .unwrap()
}

/// A CLOSED handover item (it owns nothing) of the conversation, raised for `reason` / `trigger` on its own case.
async fn closed_item(fx: &Fx, reason: &str, trigger: &str) {
    add_case(fx, "CS-90").await;
    sqlx::query(
        "INSERT INTO agent_handover_items \
            (tenant_id, job_id, id, case_id, conversation_id, env, reason, trigger, state, closed_at) \
         VALUES ($1, $2, $3, 'CS-90', $4, 'prod', $5, $6, 'resolved', now())",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(format!("HO-{}", 9000 + Uuid::new_v4().as_u128() % 900))
    .bind(&fx.conv)
    .bind(reason)
    .bind(trigger)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn resolved_agent_case_arms_csat_once() {
    let fx = setup("t8-csat", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    let mut chat = listen(&fx.bus).await;
    let env = resolved(&fx, json!({}));
    let d = deliver(&fx, &env).await.unwrap();
    assert!(
        matches!(
            d,
            Delivery::Replied {
                csat: false,
                relayed: true,
                ..
            }
        ),
        "armed, not asked: {d:?}"
    );
    let again = deliver(&fx, &env).await.unwrap();
    assert!(
        matches!(
            again,
            Delivery::Replied {
                written: false,
                relayed: false,
                csat: false,
                ..
            }
        ),
        "{again:?}"
    );
    let csat_rows = count(
        &fx.e.owner,
        "SELECT count(*) FROM case_csat WHERE tenant_id = $1 AND case_id = $2",
        fx.f.tenant,
        CASE,
    )
    .await;
    assert_eq!(csat_rows, 1);
    assert_eq!(
        csat_state(&fx).await.as_deref(),
        Some("awaiting_confirmation")
    );
    let asked_by: String =
        sqlx::query_scalar("SELECT asked_by FROM case_csat WHERE tenant_id = $1 AND case_id = $2")
            .bind(fx.f.tenant)
            .bind(CASE)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
    assert_eq!(asked_by, "agent");
    let prompts = count(
        &fx.e.owner,
        "SELECT count(*) FROM conversation_message WHERE tenant_id = $1 AND conversation_id = $2 \
            AND role = 'agent' AND author = 'csat'",
        fx.f.tenant,
        &fx.conv,
    )
    .await;
    assert_eq!(prompts, 0, "no prompt line until the customer acknowledges");
    let got = frames(&mut chat, Duration::from_millis(500)).await;
    let names: Vec<&str> = got.iter().map(|f| f.event.as_str()).collect();
    assert_eq!(names, ["message"], "{got:?}");
}

#[tokio::test]
async fn a_complaint_conversation_is_not_armed() {
    let fx = setup("t8-csat-complaint", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    closed_item(&fx, "staff_decision", "case.create{complaint}").await;
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 0);
}

#[tokio::test]
async fn a_customer_asked_handover_conversation_is_not_armed() {
    let fx = setup("t8-csat-asked", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    closed_item(&fx, "customer_asked", "").await;
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 0);
}

#[tokio::test]
async fn an_api_conversation_is_never_armed() {
    let fx = setup("t8-csat-api", Env::Prod, "resolved").await;
    // `mcp` is an interaction channel, not a conversation row's (the `conversation_channel_check` list has no `mcp`):
    // its rule is the same predicate the stored channels use
    assert!(!agent_api::conversations::csat::channel_asks("mcp"));
    for (i, channel) in ["api", "voice", "staff"].into_iter().enumerate() {
        on_channel(&fx, channel).await;
        let case = format!("CS-{}", 10 + i);
        add_case(&fx, &case).await;
        let d = deliver(&fx, &resolved_case(&fx, &case, json!({})))
            .await
            .unwrap();
        assert!(
            matches!(d, Delivery::Replied { csat: false, .. }),
            "{channel}: {d:?}"
        );
        assert_eq!(csat_rows(&fx).await, 0, "{channel}");
    }
    // the same conversation on a channel a person reads is armed
    on_channel(&fx, "web_chat").await;
    deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert_eq!(csat_rows(&fx).await, 1);
}

#[tokio::test]
async fn open_case_requests_no_csat() {
    let fx = setup("t8-nocsat", Env::Prod, "open").await;
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    let csat_rows = count(
        &fx.e.owner,
        "SELECT count(*) FROM case_csat WHERE tenant_id = $1 AND case_id = $2",
        fx.f.tenant,
        CASE,
    )
    .await;
    assert_eq!(csat_rows, 0);
}

#[tokio::test]
async fn prod_reply_has_no_trace() {
    let fx = setup("t8-prodtrace", Env::Prod, "open").await;
    let mut chat = listen(&fx.bus).await;
    let trace = json!({"trace": {"procedure": "return@1", "read": ["languages"]}});
    deliver(&fx, &resolved(&fx, trace)).await.unwrap();
    let reply = events(&fx, "reply").await;
    assert_eq!(reply.len(), 1);
    assert!(reply[0].get("trace").is_none(), "{}", reply[0]);
    let got = frames(&mut chat, Duration::from_millis(500)).await;
    assert_eq!(got.len(), 1);
    assert!(got[0].data.get("trace").is_none(), "{}", got[0].data);
}

/// `route` places are keyed as the draft ledger keys them (`route:<op>:<env>`, connectors::places), so the route diff
/// is proven on the pure mapping the daemon applies to B2's diff: the test-env turn reads its TEST route, so a
/// differing test route it read is a `route` diff keyed by its change; the same op's prod route is not exercised.
#[test]
fn trace_lists_route_diff_when_draft_changed_a_route() {
    let deltas = vec![
        Delta {
            place: "route:order.read:test".into(),
            kind: "route".into(),
            change: Some("route:order.read:test".into()),
        },
        Delta {
            place: "route:order.read:prod".into(),
            kind: "route".into(),
            change: Some("route:order.read:prod".into()),
        },
    ];
    let read = vec!["route:order.read:test".to_string()];
    let changed = vec![
        "route:order.read:test".to_string(),
        "route:order.read:prod".to_string(),
    ];
    let (exercised, differs) = exercised_and_differs(&read, &changed, &deltas);
    assert_eq!(exercised, vec!["route:order.read:test".to_string()]);
    assert_eq!(differs.len(), 1);
    assert_eq!(differs[0].kind, "route");
    assert_eq!(differs[0].key.as_deref(), Some("route:order.read:test"));
}

/// A test-env reply carries the trace: the case's draft revision, the draft fingerprint of the change keys, the
/// unpublished change the turn read (`languages`, R1 [en, de] → draft [en, de, fr]) and Live's differing value.
/// Read as `agent_runtime`, the daemon's role (`draft_ledger` SELECT, 0123).
#[tokio::test]
async fn test_reply_trace_carries_draft_fp_exercised_and_differs() {
    let dx = crate::b4::design::setup(None).await;
    let at = crate::b4::design::put_target(&dx).await;
    crate::b4::design::put_languages(&dx, json!(["en", "de"])).await;
    crate::b4::design::approve(&dx, &at, true).await;
    crate::b4::design::put_languages(&dx, json!(["en", "de", "fr"])).await;
    let fx = setup_with("t8-trace", dx.e, dx.f, Env::Test, "open").await;
    let rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    let keys = {
        let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
        let k = design::ledger::keys(&mut tx, &fx.f.job_ref())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        k
    };
    let mut chat = listen(&fx.bus).await;
    let input = json!({"trace": {"procedure": "return@1", "read": ["languages", "setting:stance"],
                                 "ops": [{"operation": "order.read", "state": "ok", "code": "ok"}]}});
    deliver(&fx, &resolved(&fx, input)).await.unwrap();
    let body = events(&fx, "reply").await.remove(0);
    let t = &body["trace"];
    assert_eq!(t["env"], "test");
    assert_eq!(t["design"], "draft");
    assert_eq!(t["draft_rev"], json!(rev));
    assert!(t.get("unavailable").is_none(), "{t}");
    assert_eq!(t["draft_fp"], json!(design::fingerprint(&keys)));
    assert_eq!(t["exercised"], json!(["languages"]));
    assert_eq!(t["differs"][0]["kind"], "label", "{t}");
    assert_eq!(t["procedure"], "return@1");
    assert_eq!(t["ops"][0]["operation"], "order.read");
    assert_eq!(t["checks"][0]["id"], "RET-01");
    assert_eq!(t["checks"][0]["verdict"], "pass");
    let got = frames(&mut chat, Duration::from_millis(500)).await;
    assert_eq!(got[0].data["trace"], *t, "the frame carries the same trace");
}

/// Another `resolved` case of the fixture's conversation: each customer turn is its own case.
async fn add_case(fx: &Fx, case_id: &str) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, status) \
         VALUES ($1, $2, $3, $4, 'staff', 'prod', 'resolved')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(case_id)
    .bind(&fx.conv)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

async fn csat_rows(fx: &Fx) -> i64 {
    count(
        &fx.e.owner,
        "SELECT count(*) FROM case_csat WHERE tenant_id = $1 AND conversation_id = $2",
        fx.f.tenant,
        &fx.conv,
    )
    .await
}

#[tokio::test]
async fn two_resolved_turns_of_one_conversation_arm_once() {
    let fx = setup("t8-csat-conv", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    add_case(&fx, "CS-2").await;
    let first = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(
        matches!(first, Delivery::Replied { csat: false, .. }),
        "{first:?}"
    );
    assert_eq!(csat_rows(&fx).await, 1, "the first turn armed it");
    let second = deliver(&fx, &resolved_case(&fx, "CS-2", json!({})))
        .await
        .unwrap();
    assert!(
        matches!(second, Delivery::Replied { csat: false, .. }),
        "{second:?}"
    );
    assert_eq!(csat_rows(&fx).await, 1);
    assert_eq!(replies(&fx).await, 2, "both turns are still answered");
}

#[tokio::test]
async fn a_reply_that_offers_a_colleague_is_not_rated() {
    let fx = setup("t8-csat-offer", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    // a resolved case whose reply is the no-shipment text, which offers a colleague
    let d = deliver(
        &fx,
        &resolved(&fx, json!({"outcome": "handover", "facts": {}})),
    )
    .await
    .unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 0);
    // and the offer does not use up the conversation's one request
    add_case(&fx, "CS-2").await;
    let d = deliver(&fx, &resolved_case(&fx, "CS-2", json!({})))
        .await
        .unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 1, "now armed");
}

#[tokio::test]
async fn a_reply_ending_in_a_question_is_not_rated() {
    let fx = setup("t8-csat-question", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    let d = deliver(
        &fx,
        &resolved(
            &fx,
            json!({
                "outcome": "confirmation_required",
                "code": "choice_required",
                "reason": "Which order do you mean, the rug or the lamp?",
            }),
        ),
    )
    .await
    .unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 0);
}

#[tokio::test]
async fn a_binding_with_csat_disabled_is_never_asked() {
    let fx = setup("t8-csat-off", Env::Prod, "resolved").await;
    on_channel(&fx, "web_chat").await;
    let binding: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, verified_at, settings) \
         VALUES ($1, $2, 'web_chat', 'customer', $3, 'test', now(), '{\"csat_enabled\": false}') RETURNING id",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(format!("wk_{}", Uuid::new_v4().simple()))
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_conversation_refs (tenant_id, job_id, conversation_id, side, channel, external_id, route) \
         VALUES ($1, $2, $3, 'customer', 'web_chat', 'ext-1', $4)",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(&fx.conv)
    .bind(json!({"binding_id": binding.to_string()}))
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let d = deliver(&fx, &resolved(&fx, json!({}))).await.unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 0);
    // switched back on: the same conversation is asked
    sqlx::query("UPDATE connector_bindings SET settings = '{}' WHERE id = $1")
        .bind(binding)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    add_case(&fx, "CS-2").await;
    let d = deliver(&fx, &resolved_case(&fx, "CS-2", json!({})))
        .await
        .unwrap();
    assert!(matches!(d, Delivery::Replied { csat: false, .. }), "{d:?}");
    assert_eq!(csat_rows(&fx).await, 1, "now armed");
}
