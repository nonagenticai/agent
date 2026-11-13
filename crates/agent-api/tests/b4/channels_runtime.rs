//! B4 T9c (§0 R7 + R3) as amended by D109 (KH T6): customer channels on the runtime, and only there. A web chat
//! message arrives through the edge's `POST /api/internal/channel/inbound` (crate::common::conv: tenant Alpha, job
//! `j1`, binding `wk_test`). For a live job (a current release) whose prod scope a live `interaction` process serves
//! (its heartbeat, written here), the message is a runtime turn — `turn::accept_customer_message` (card data redacted
//! before it is stored) and the drain — and the edge is answered with the line the runtime stored; job `j1` binds no
//! qualifier, so that line is the drain's `TURN_FAILED`. Without a live release it is 409 `job_not_live`; live but not
//! served (no Redis, no heartbeat) it is 503 `unavailable` — never `AppState.interaction` (the staff playground's
//! backend), and no tenant allow-list switches it.
use std::sync::Arc;

use agent_api::interaction::{
    BackendError, BackendEvent, InteractionBackend, InteractionRequest, InteractionResult, Message,
    Party, Role, Scope as JobScope, collect,
};
use agent_api::knowledge::analytics::ReplyGrounding;
use agent_api::runtime::Env;
use agent_api::runtime::backend::RuntimeBackend;
use axum::http::StatusCode;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use serde_json::json;

use crate::common::conv::{self, Ctx, code};
use crate::redis_env as r;

/// A Luhn-valid test PAN: the runtime's accept redacts it before the message is stored; the B12 path would not.
const WITH_CARD: &str = "My card 4111 1111 1111 1111 was charged twice";

fn job_scope(c: &Ctx) -> JobScope {
    JobScope {
        tenant: c.t,
        job_id: c.job,
        job_key: "j1".into(),
    }
}

/// `(role, external_id, text)` of every message of the tenant's job, sorted (never by insertion time).
async fn messages(c: &Ctx) -> Vec<(String, Option<String>, String)> {
    let mut rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT role, external_id, text FROM conversation_message WHERE tenant_id = $1 AND job_id = $2",
    )
    .bind(c.t)
    .bind(c.job)
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    rows.sort();
    rows
}

/// Every grounding row of the tenant.
async fn groundings(c: &Ctx) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM answer_grounding WHERE tenant_id = $1")
        .bind(c.t)
        .fetch_one(&c.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn webchat_turn_runs_through_the_runtime() {
    let c = conv::setup_runtime(r::api_vars("t9c-webchat").await).await;
    let (s, b) = c.inbound("wc-t9c-1", "m-1", WITH_CARD).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "{b}");
    assert_eq!(b["text"], agent_api::turn::TURN_FAILED, "{b}");
    assert_eq!(
        c.backend.calls(),
        0,
        "the playground backend answered a customer"
    );
    let rows = messages(&c).await;
    assert_eq!(
        rows.len(),
        2,
        "one customer message, one agent line: {rows:?}"
    );
    let customer = rows.iter().find(|m| m.0 == "customer").unwrap();
    assert_eq!(
        customer.1.as_deref(),
        Some("m-1"),
        "the provider id is kept"
    );
    assert!(
        customer.2.contains(agent_api::turn::redact::CARD_REMOVED) && !customer.2.contains("4111"),
        "the runtime's accept stored the redacted text: {}",
        customer.2
    );
    let agent = rows.iter().find(|m| m.0 == "agent").unwrap();
    assert_eq!(agent.2, agent_api::turn::TURN_FAILED);
    // a provider retry of the answered message is a duplicate, not a second turn
    let (s, b) = c.inbound("wc-t9c-1", "m-1", WITH_CARD).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "duplicate", "{b}");
    assert_eq!(messages(&c).await.len(), 2);
}

/// `(customer_id, customer_email, identity_source)` of the tenant's one web chat conversation.
async fn conversation_identity(
    c: &Ctx,
    conv: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    use agent_api::conversations::state as st;
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT cv.customer_id, {email}, {source} FROM conversation cv \
           JOIN channel_conversation_refs r ON r.conversation_id = cv.conversation_id \
          WHERE cv.tenant_id = $1 AND r.external_id = $2",
        email = st::sub("cv", st::EMAIL),
        source = st::sub("cv", st::IDENTITY_SOURCE),
    )))
    .bind(c.t)
    .bind(conv)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

/// Open conversation `conv`'s session and, when `customer` is `Some((ref, email))`, identify it the way the core's
/// session create does (`customer_ref` / `customer_email` / `identified_at`), before its first message.
async fn open_session(c: &Ctx, conv: &str, customer: Option<(&str, &str)>) {
    let ip = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(conv.as_bytes()));
    let (id, tok) = c.session(&ip).await.expect("a session opens");
    if let Some((customer_ref, email)) = customer {
        sqlx::query(
            "UPDATE web_chat_session SET customer_ref = $2, customer_email = $3, identified_at = now() \
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .bind(customer_ref)
        .bind(email)
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    c.sessions
        .lock()
        .unwrap()
        .insert(conv.to_string(), (id, tok, ip));
}

/// Give conversation `conv`'s identified session the display name its assertion carried (what the core's create stores).
async fn name_session(c: &Ctx, conv: &str, name: &str) {
    let id = c.sessions.lock().unwrap().get(conv).cloned().unwrap().0;
    sqlx::query("UPDATE web_chat_session SET customer_name = $2 WHERE id = $1::uuid")
        .bind(id)
        .bind(name)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

/// The conversation's `customer_name` (the staff list's title), by wire conversation id.
async fn conversation_name(c: &Ctx, conv: &str) -> Option<String> {
    use agent_api::conversations::state as st;
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {name} FROM conversation cv \
           JOIN channel_conversation_refs r ON r.conversation_id = cv.conversation_id \
          WHERE cv.tenant_id = $1 AND r.external_id = $2",
        name = st::sub("cv", st::NAME),
    )))
    .bind(c.t)
    .bind(conv)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn spoofed_wire_verified_does_not_identify() {
    let c = conv::setup_runtime(r::api_vars("kc-spoof").await).await;
    open_session(&c, "wc-kc-1", None).await;
    // the edge's body claims a verified party; the session is anonymous
    let sess = c.sessions.lock().unwrap().get("wc-kc-1").cloned().unwrap();
    let body = json!({
        "kind": "web_chat", "external_id": "wk_test", "binding_id": c.wc,
        "tenant": c.t, "job_id": c.job, "job_key": "j1",
        "conversation": {"channel": "web_chat", "id": "wc-kc-1", "route": {}},
        "message_id": "m-1",
        "party": {"id": format!("wc:{}", sess.0), "name": "Femke", "verified": true},
        "session_token": sess.1, "ip_hash": sess.2, "text": "Hello",
    });
    let (s, b) = c.internal("/api/internal/channel/inbound", body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (customer_id, email, source) = conversation_identity(&c, "wc-kc-1").await;
    assert!(
        customer_id.as_deref().is_none_or(|i| i.starts_with("wc:")),
        "the wire named no customer: {customer_id:?}"
    );
    assert_eq!(email, None);
    assert!(
        !matches!(
            source.as_deref(),
            Some("assertion" | "staff_pick" | "staff_test")
        ),
        "a spoofed wire claim identifies nobody: {source:?}"
    );
    let refs: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM party p WHERE p.tenant_id = $1 AND {} IS NOT NULL",
        agent_api::party::subject_sql("p"),
    )))
    .bind(c.t)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(refs, 0, "no party got a customer ref from the wire");
}

#[tokio::test]
async fn identified_session_binds_the_conversation() {
    let c = conv::setup_runtime(r::api_vars("kc-bind").await).await;
    open_session(&c, "wc-kc-2", Some(("cust-1001", "ann@example.test"))).await;
    for msg in ["m-1", "m-2"] {
        let (s, b) = c.inbound("wc-kc-2", msg, "Hello").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // idempotent: the same after the first and the second message
        assert_eq!(
            conversation_identity(&c, "wc-kc-2").await,
            (
                Some("cust-1001".into()),
                Some("ann@example.test".into()),
                Some("assertion".into())
            )
        );
    }
    let party: (String,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {subject} FROM party p JOIN conversation cv ON cv.party_id = p.id WHERE cv.tenant_id = $1",
        subject = agent_api::party::subject_sql("p"),
    )))
    .bind(c.t)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(party.0, "cust-1001", "the party's identified customer");
}

/// The session's display name reaches the conversation; a later session of ANOTHER customer on the same conversation
/// neither binds nor renames it (the customer guard).
#[tokio::test]
async fn identified_session_names_the_conversation() {
    let c = conv::setup_runtime(r::api_vars("kc-name").await).await;
    open_session(&c, "wc-kc-3", Some(("cust-1001", "ann@example.test"))).await;
    name_session(&c, "wc-kc-3", "Ann Example").await;
    let (s, b) = c.inbound("wc-kc-3", "m-1", "Hello").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        conversation_name(&c, "wc-kc-3").await.as_deref(),
        Some("Ann Example")
    );
    // another customer's session (same wire conversation) arrives: the conversation keeps its customer and name
    open_session(&c, "wc-kc-3", Some(("cust-2002", "bob@example.test"))).await;
    name_session(&c, "wc-kc-3", "Bob Other").await;
    let (s, b) = c.inbound("wc-kc-3", "m-2", "Hello again").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        conversation_name(&c, "wc-kc-3").await.as_deref(),
        Some("Ann Example")
    );
    assert_eq!(
        conversation_identity(&c, "wc-kc-3").await.0.as_deref(),
        Some("cust-1001")
    );
}

/// Answers [`GROUNDED`] and reports how it used knowledge: a playground backend that must never see a customer.
struct Grounded;

const GROUNDED: &str = "Deliveries take two working days.";

impl InteractionBackend for Grounded {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        stream::iter(vec![
            Ok(BackendEvent::Delta(GROUNDED.into())),
            Ok(BackendEvent::Done { usage: None }),
        ])
        .boxed()
    }

    fn answer(
        &self,
        _req: InteractionRequest,
    ) -> BoxFuture<'static, Result<InteractionResult, BackendError>> {
        Box::pin(async {
            Ok(InteractionResult {
                text: GROUNDED.into(),
                usage: None,
                grounding: Some(ReplyGrounding {
                    knowledge_query: Some("delivery times".into()),
                    used_knowledge: true,
                    citations: vec!["chunk-7".into()],
                    sources: vec![json!({"title": "Delivery"})],
                    dont_know: false,
                }),
            })
        })
    }
}

#[tokio::test]
async fn no_live_release_is_409_job_not_live_and_stores_nothing() {
    let mut c = conv::setup().await;
    c.e.state.interaction = Arc::new(Grounded);
    c.e.app = agent_api::router(c.e.state.clone());
    let (s, b) = c
        .inbound("wc-t9c-2", "m-1", "How long does delivery take?")
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
    assert!(messages(&c).await.is_empty(), "nothing stored");
    assert_eq!(groundings(&c).await, 0, "no reply, no grounding");
}

#[tokio::test]
async fn a_live_job_nobody_serves_is_503_never_the_playground() {
    // no REDIS_URL: no runtime
    let c = conv::setup_live(|_| {}).await;
    assert!(
        RuntimeBackend::serving(&c.e.state, &job_scope(&c), "prod")
            .await
            .is_none()
    );
    let (s, b) = c.inbound("wc-t9c-3", "m-1", "Where is my order?").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{b}"
    );
    assert_eq!(c.backend.calls(), 0);
    assert!(
        messages(&c).await.is_empty(),
        "nothing stored: the edge may resend"
    );

    // Redis, but no process serves the scope yet: still 503
    let vars = r::api_vars("t9c-unserved").await;
    let c = conv::setup_live(|v| v.extend(vars)).await;
    assert!(
        RuntimeBackend::serving(&c.e.state, &job_scope(&c), "prod")
            .await
            .is_none()
    );
    let (s, b) = c.inbound("wc-t9c-4", "m-1", "Where is my order?").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{b}"
    );
    assert_eq!(c.backend.calls(), 0);

    // served: a request without a conversation (an API caller) is a runtime turn too — no fallback backend
    c.serve(Env::Prod).await;
    let rt = RuntimeBackend::serving(&c.e.state, &job_scope(&c), "prod")
        .await
        .expect("the scope is served");
    let mut req = InteractionRequest::new(&job_scope(&c), vec![Message::text(Role::User, "hi")]);
    req.party = Some(Party::customer("api:t9c", None));
    let answer = collect(rt.respond(req)).await.unwrap();
    assert_eq!(answer.text, agent_api::turn::TURN_FAILED);
    assert_eq!(c.backend.calls(), 0);
}

#[tokio::test]
async fn any_tenant_prod_scope_runs_on_the_runtime_once_served() {
    // no allow-list: a served prod scope is the runtime's for every tenant
    let vars = r::api_vars("t9c-any-tenant").await;
    let c = conv::setup_live(|v| v.extend(vars)).await;
    assert!(
        RuntimeBackend::serving(&c.e.state, &job_scope(&c), "prod")
            .await
            .is_none(),
        "not served yet"
    );
    c.serve(Env::Prod).await;
    assert!(
        RuntimeBackend::serving(&c.e.state, &job_scope(&c), "prod")
            .await
            .is_some()
    );
    assert!(
        RuntimeBackend::serving(&c.e.state, &job_scope(&c), "test")
            .await
            .is_none(),
        "no process serves the test scope yet"
    );
    c.serve(Env::Test).await;
    assert!(
        RuntimeBackend::serving(&c.e.state, &job_scope(&c), "test")
            .await
            .is_some()
    );
    let (s, b) = c.inbound("wc-t9c-6", "m-1", "Where is my order?").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["text"], agent_api::turn::TURN_FAILED, "{b}");
    assert_eq!(
        c.backend.calls(),
        0,
        "the runtime answered, not the playground"
    );
}
