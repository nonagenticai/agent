//! B3 T11: answer feedback and knowledge analytics (plan §5.7, §4 0122 (ex-0055), §0 R10): rating validation and
//! the one-rating-per-person upsert of an agent reply in B12's `conversation_message` log, the newest-first list with
//! `by_name` and the message's conversation / case / text / question, the analytics window, production answers only
//! and ratings from every surface, the port of `web/src/lib/grounding/grounding.ts` `knowledgeAnalyticsOf` (its vitest
//! cases), zeros before the runtime records any grounding, never-cited over the bound TENANT leaves of the live design
//! (packs left out), platform sources labelled `platform:<key>@<v>`, and the 0122 grounding CHECK, agent-reply
//! trigger and 1:1 key. Real Postgres 18 (one container for this binary, a fresh database per test) and the
//! in-process strata fakes.
use crate::common;

use crate::common::design_contract::{
    self, ContractJob, TEMPLATE, contract_job, cuttable_selection,
};
use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json, person, send, sqlstate, super_admin,
};
use agent_api::design::{DesignOf, DraftRef, JobRef};
use agent_api::knowledge::analytics::{
    self, AnswerRow, DocRow, FeedbackRow, Input, ReplyGrounding, Source, StoreNode,
};
use agent_api::knowledge::kinds;
use agent_api::strata::StrataStoreId;
use agent_api::tenancy::begin_scoped;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chrono::{DateTime, TimeDelta, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    f: ContractJob,
    /// (sub, token) of a member without roles, display name "Lena Hoek".
    lena: (String, String),
    /// (sub, token) of a second member without roles.
    mara: (String, String),
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> (String, String) {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    let tok = person(&sub, &[]);
    (sub, tok)
}

/// A tenant (pinned in the strata fakes) with a job created through `POST /api/jobs` (B2's creation hook ran) and a
/// complete selection, so it cuts.
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("T11 grounding {tenant}")).await;
    let mut r = json(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", TEMPLATE.0, TEMPLATE.1)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
    let (s, b) = send(&e.app, r).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().unwrap().to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let f = contract_job(tenant, id, &key);
    design_contract::write(
        &e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::publishable(&e, &f).await;
    let lena = member(&e, tenant, "tenant_user").await;
    sqlx::query("UPDATE users SET display_name = 'Lena Hoek' WHERE sub = $1")
        .bind(&lena.0)
        .execute(&e.owner)
        .await
        .unwrap();
    let mara = member(&e, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        f,
        lena,
        mara,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn send_raw(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let resp = app.clone().oneshot(req).await.expect("router");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, v)
}

/// A valid rating body (`AnswerFeedbackInput`) of agent reply `answer` with `over` merged in. The web also sends
/// the conversation, question, answer text and grounding; the API reads those from the message, so the stale values
/// here must never come back.
fn rating(answer: Uuid, over: Value) -> Value {
    let mut v = json!({
        "surface": "test_chat",
        "conversation_id": "not-read",
        "answer_id": answer,
        "case_id": null,
        "question": "not read",
        "answer": "not read",
        "vote": "up",
        "grounding": "grounded",
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn at_days_ago(days: i64) -> DateTime<Utc> {
    Utc::now() - TimeDelta::days(days)
}

/// One grounded agent reply (as the runtime will record it): its own conversation in `env`, a customer question
/// and the reply `days_ago`, then [`analytics::record_grounding`] as `agent_app`.
struct Turn {
    env: &'static str,
    days_ago: i64,
    question: &'static str,
    query: Option<&'static str>,
    used_knowledge: bool,
    citations: Vec<&'static str>,
    sources: Value,
    dont_know: bool,
}

impl Default for Turn {
    fn default() -> Self {
        Self {
            env: "prod",
            days_ago: 1,
            question: "q",
            query: Some("q"),
            used_knowledge: true,
            citations: vec![],
            sources: json!([]),
            dont_know: false,
        }
    }
}

impl W {
    fn job_uri(&self, tail: &str) -> String {
        format!("/api/jobs/{}/{tail}", self.f.job.job_key)
    }

    async fn rate(&self, tok: &str, body: Value) -> (StatusCode, Value) {
        send(
            &self.e.app,
            json("POST", &self.job_uri("answer-feedback"), Some(tok), body),
        )
        .await
    }

    async fn list(&self, tok: &str, query: &str) -> (StatusCode, Value) {
        let uri = format!("{}{query}", self.job_uri("answer-feedback"));
        send(&self.e.app, get(&uri, Some(tok))).await
    }

    async fn analytics(&self, days: &str) -> (StatusCode, HeaderMap, Value) {
        let uri = format!("{}?days={days}", self.job_uri("analytics/knowledge"));
        send_raw(&self.e.app, get(&uri, Some(&self.lena.1))).await
    }

    /// A conversation `id` of the job in `env` (as the owner).
    async fn conversation(&self, id: &str, env: &str) {
        sqlx::query(
            "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
             VALUES ($1, $2, $3, $4, 'web_chat')",
        )
        .bind(self.tenant)
        .bind(self.f.job.job_id)
        .bind(id)
        .bind(env)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A message of conversation `conv` at `at` (as the owner); its id.
    async fn message(
        &self,
        conv: &str,
        role: &str,
        text: &str,
        case_id: Option<&str>,
        at: DateTime<Utc>,
    ) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO conversation_message \
                (message_id, tenant_id, job_id, conversation_id, role, author, text, case_id, at_ms) \
             VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(self.tenant)
        .bind(self.f.job.job_id)
        .bind(conv)
        .bind(role)
        .bind(text)
        .bind(case_id)
        .bind(at.timestamp_millis())
        .execute(&self.e.owner)
        .await
        .unwrap();
        id
    }

    /// A new `prod` conversation `conv` with a customer question and the agent's reply (no grounding); the reply's id.
    async fn reply(&self, conv: &str, question: &str, case_id: Option<&str>) -> Uuid {
        self.conversation(conv, "prod").await;
        let at = Utc::now();
        self.message(
            conv,
            "customer",
            question,
            case_id,
            at - TimeDelta::seconds(1),
        )
        .await;
        self.message(conv, "agent", "I don't know.", case_id, at)
            .await
    }

    /// Record `g` for reply `message_id` through the runtime's hook, as `agent_app`; whether it was stored.
    async fn ground(&self, message_id: Uuid, g: &ReplyGrounding) -> bool {
        let mut tx = begin_scoped(&self.e.state.db, self.tenant).await.unwrap();
        let ok = analytics::record_grounding(&mut tx, self.f.job.job_id, message_id, g)
            .await
            .is_ok();
        if ok {
            tx.commit().await.unwrap();
        }
        ok
    }

    async fn turn(&self, t: Turn) {
        let conv = format!("conv-{}", Uuid::new_v4().simple());
        self.conversation(&conv, t.env).await;
        let at = at_days_ago(t.days_ago);
        self.message(
            &conv,
            "customer",
            t.question,
            None,
            at - TimeDelta::seconds(1),
        )
        .await;
        let reply = self.message(&conv, "agent", "an answer", None, at).await;
        let g = ReplyGrounding {
            knowledge_query: t.query.map(str::to_string),
            used_knowledge: t.used_knowledge,
            citations: t.citations.iter().map(|c| c.to_string()).collect(),
            sources: t.sources.as_array().cloned().unwrap_or_default(),
            dont_know: t.dont_know,
        };
        assert!(self.ground(reply, &g).await, "record_grounding");
    }

    /// A bound leaf `key` (a root) in both the fake and the local tree; its strata id.
    async fn bound_leaf(&self, key: &str) -> Uuid {
        let sid = lock(&self.fake.rag).add_store(self.tenant, None, "leaf", key);
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, updated_by) \
             VALUES ($1, $2, $3, $3, NULL, 'leaf', $4, 'eu', 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(key)
        .bind(sid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        // D110 (KH T8): the leaf's documents live in strata only; a local source in the primary language says what
        // feeds it, so a required kind bound to it may publish
        sqlx::query(
            "INSERT INTO source (tenant_id, job_id, id, name, type, target_store_id, language, updated_by) \
             VALUES ($1, $2, gen_random_uuid(), $3, 'file', $4, 'en', 'test')",
        )
        .bind(self.tenant)
        .bind(self.f.job.job_id)
        .bind(format!("feeds {key}"))
        .bind(id)
        .execute(&self.e.owner)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, finished_at, started_by, documents) \
             SELECT tenant_id, job_id, gen_random_uuid(), id, 'completed', now(), 'test', 1 FROM source \
              WHERE target_store_id = $1",
        )
        .bind(id)
        .execute(&self.e.owner)
        .await
        .unwrap();
        sid
    }

    /// A strata document titled `title` in leaf `sid`; its id.
    fn doc(&self, sid: Uuid, title: &str) -> Uuid {
        let id = Uuid::new_v4();
        lock(&self.fake.rag).documents.push((
            self.tenant,
            json!({"id": id, "store_id": sid, "source": format!("{title}.pdf"), "title": title,
                   "content_hash": "h", "token_count": 1, "chunk_count": 1, "source_id": null,
                   "source_run": null, "metadata": {}, "created_at": "2026-10-21T10:00:00Z"}),
        ));
        id
    }

    /// A tracked draft write of knowledge kind `(stage, kind)` = `{store, packs}`.
    async fn bind(&self, stage: &str, kind: &str, store: Option<&str>, packs: &[&str]) {
        let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
        design_contract::write(
            &self.e,
            &self.f,
            "knowledge",
            &kinds::kind_ref(stage, kind),
            Some(kinds::kind_value(store, &packs)),
        )
        .await;
    }

    /// Approve and apply the job's draft as a release manager.
    async fn publish(&self) {
        let (_, rm) = member(&self.e, self.tenant, "tenant_admin").await;
        let (s, b) = send(&self.e.app, get(&self.job_uri("target"), Some(&rm))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
        let (s, b) = send(
            &self.e.app,
            json("POST", &self.job_uri("releases"), Some(&rm), body),
        )
        .await;
        assert!(
            s == StatusCode::CREATED || s == StatusCode::OK,
            "publish: {s}: {b}"
        );
        assert_eq!(b["applied"], true, "{b}");
    }

    fn document_reads(&self) -> Vec<String> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == "GET" && r.path.ends_with("/documents"))
            .map(|r| r.path)
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// submitAnswerFeedback / answerFeedback

fn dont_know() -> ReplyGrounding {
    ReplyGrounding {
        knowledge_query: Some("price match".into()),
        used_knowledge: true,
        dont_know: true,
        ..ReplyGrounding::default()
    }
}

#[tokio::test]
async fn feedback_validation_422_invalid_feedback() {
    let w = world().await;
    let reply = w.reply("conv-1", "Do you price match?", None).await;
    let question: Uuid = sqlx::query_scalar(
        "SELECT message_id FROM conversation_message WHERE conversation_id = 'conv-1' AND role = 'customer'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    let long = "x".repeat(501);
    for bad in [
        json!({"vote": "sideways"}),
        json!({"vote": null}),
        json!({"surface": "email"}),
        json!({"answer_id": " "}),
        json!({"answer_id": null}),
        json!({"answer_id": "m-1-r"}),
        // a message id, but no message of this job
        json!({"answer_id": Uuid::now_v7()}),
        // a message of this job, but the customer's, not an agent reply
        json!({"answer_id": question}),
        json!({"vote": "down", "reason": "rude"}),
        json!({"vote": "down", "note": long}),
    ] {
        let (s, b) = w.rate(&w.lena.1, rating(reply, bad.clone())).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_feedback"),
            "{bad}: {b}"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM answer_feedback WHERE job_id = $1")
        .bind(w.f.job.job_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing stored");
    // exactly 500 characters is fine (on a down vote)
    let (s, b) = w
        .rate(
            &w.lena.1,
            rating(reply, json!({"vote": "down", "note": "y".repeat(500)})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn feedback_up_vote_drops_reason() {
    let w = world().await;
    let reply = w.reply("conv-1", "Do you price match?", None).await;
    assert!(w.ground(reply, &dont_know()).await);
    let (s, b) = w
        .rate(
            &w.lena.1,
            rating(reply, json!({"reason": "tone", "note": "Clear answer"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["vote"], "up");
    assert_eq!(b["reason"], Value::Null, "{b}");
    assert_eq!(b["note"], "Clear answer");
    assert_eq!(b["by"], w.lena.0.as_str());
    assert_eq!(b["by_name"], "Lena Hoek");
    assert_eq!(b["job"], w.f.job.job_key.as_str());
    assert_eq!(b["tenant_id"], w.tenant.to_string().as_str());
    // the message, its question and its grounding row are the truth, not the body
    assert_eq!(b["answer_id"], reply.to_string().as_str(), "{b}");
    assert_eq!(b["conversation_id"], "conv-1", "{b}");
    assert_eq!(b["question"], "Do you price match?", "{b}");
    assert_eq!(b["answer"], "I don't know.", "{b}");
    assert_eq!(b["grounding"], "dont_know", "{b}");
    // a down vote keeps its reason; a blank note is none; a reply without a grounding record is no_knowledge
    let other = w.reply("conv-2", "Opening hours?", None).await;
    let (s, b) = w
        .rate(
            &w.lena.1,
            rating(
                other,
                json!({"vote": "down", "reason": "incomplete", "note": "  "}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            b["reason"].as_str(),
            b["note"].clone(),
            b["grounding"].as_str()
        ),
        (Some("incomplete"), Value::Null, Some("no_knowledge"))
    );
    // audited, without the answer text
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'answer_feedback.submit' \
          ORDER BY id DESC LIMIT 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(details["vote"], "down", "{details}");
    assert_eq!(details["reason"], "incomplete", "{details}");
    assert!(!details.to_string().contains("I don't know"), "{details}");
}

#[tokio::test]
async fn feedback_again_replaces_mine_not_others() {
    let w = world().await;
    let reply = w.reply("conv-1", "Do you price match?", None).await;
    let (s, first) = w
        .rate(
            &w.lena.1,
            rating(reply, json!({"reason": "tone", "note": "Clear answer"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{first}");
    let (s, again) = w
        .rate(
            &w.lena.1,
            rating(
                reply,
                json!({"vote": "down", "reason": "incomplete", "note": " Offer the form "}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["note"], "Offer the form");
    let (s, mine) = w.list(&w.lena.1, "?conversation_id=conv-1").await;
    assert_eq!(s, StatusCode::OK, "{mine}");
    let mine = mine.as_array().unwrap().clone();
    assert_eq!(mine.len(), 1, "one rating per person per answer: {mine:?}");
    assert_eq!(mine[0]["id"], again["id"]);
    assert_eq!(
        (mine[0]["vote"].as_str(), mine[0]["reason"].as_str()),
        (Some("down"), Some("incomplete"))
    );
    // another person's rating of the same answer is its own row, and leaves mine alone
    let (s, b) = w.rate(&w.mara.1, rating(reply, json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, all) = w.list(&w.lena.1, "?conversation_id=conv-1").await;
    let all = all.as_array().unwrap().clone();
    assert_eq!(all.len(), 2, "{all:?}");
    let lenas: Vec<&Value> = all
        .iter()
        .filter(|r| r["by"] == w.lena.0.as_str())
        .collect();
    assert_eq!(lenas.len(), 1);
    assert_eq!(lenas[0]["vote"], "down");
}

#[tokio::test]
async fn feedback_list_filters_newest_first_with_by_name() {
    let w = world().await;
    let a1 = w.reply("conv-a", "First?", None).await;
    let a2 = w
        .message("conv-a", "agent", "A second reply.", None, Utc::now())
        .await;
    let b1 = w.reply("conv-b", "Refund?", Some("CS-915")).await;
    for (tok, answer, surface) in [
        (&w.lena.1, a1, "test_chat"),
        (&w.mara.1, a2, "test_chat"),
        (&w.lena.1, b1, "case"),
    ] {
        let (s, b) = w
            .rate(tok, rating(answer, json!({"surface": surface})))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    // a rating of another tenant's job with the same conversation id is never listed
    let other = Uuid::new_v4();
    insert_tenant(&w.e.owner, other, &format!("T11 other {other}")).await;
    let other_job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J1', '[]', 'test') RETURNING id",
    )
    .bind(other)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    let other_reply = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
         VALUES ($1, $2, 'conv-a', 'prod', 'web_chat')",
    )
    .bind(other)
    .bind(other_job)
    .execute(&w.e.owner)
    .await
    .unwrap();
    for sql in [
        "INSERT INTO conversation_message (message_id, tenant_id, job_id, conversation_id, role, text, at_ms) \
         VALUES ($3, $1, $2, 'conv-a', 'agent', 'elsewhere', 1)",
        "INSERT INTO answer_feedback (tenant_id, job_id, id, message_id, surface, vote, by_sub) \
         VALUES ($1, $2, gen_random_uuid(), $3, 'test_chat', 'up', 'u-elsewhere')",
    ] {
        sqlx::query(sql)
            .bind(other)
            .bind(other_job)
            .bind(other_reply)
            .execute(&w.e.owner)
            .await
            .unwrap();
    }

    let (s, all) = w.list(&w.mara.1, "").await;
    assert_eq!(s, StatusCode::OK, "{all}");
    let all = all.as_array().unwrap().clone();
    assert_eq!(all.len(), 3, "{all:?}");
    let ats: Vec<&str> = all.iter().map(|r| r["at"].as_str().unwrap()).collect();
    let mut sorted = ats.clone();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(ats, sorted, "newest first");
    assert_eq!(all[0]["answer_id"], b1.to_string().as_str(), "newest first");
    let (_, a) = w.list(&w.mara.1, "?conversation_id=conv-a").await;
    let a = a.as_array().unwrap().clone();
    assert_eq!(a.len(), 2, "{a:?}");
    assert!(a.iter().all(|r| r["conversation_id"] == "conv-a"));
    // the second reply's question is still the last customer message before it
    assert!(a.iter().all(|r| r["question"] == "First?"), "{a:?}");
    let names: Vec<&str> = a.iter().map(|r| r["by_name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Lena Hoek"), "{names:?}");
    // no display name of its own → the sub (insert_user sets display_name = sub)
    assert!(names.contains(&w.mara.0.as_str()), "{names:?}");
    let (_, c) = w.list(&w.mara.1, "?case_id=CS-915").await;
    let c = c.as_array().unwrap().clone();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(
        (c[0]["surface"].as_str(), c[0]["case_id"].as_str()),
        (Some("case"), Some("CS-915"))
    );
    // an empty filter is no filter
    let (_, e) = w.list(&w.mara.1, "?conversation_id=&case_id=").await;
    assert_eq!(e.as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------------------------------------------
// knowledgeAnalytics

#[tokio::test]
async fn analytics_window_422_invalid_window() {
    let w = world().await;
    for bad in ["0", "91", "7.5", "abc", "", "-3"] {
        let (s, _, b) = w.analytics(bad).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_window"),
            "days={bad}: {b}"
        );
    }
    let (s, _, b) = send_raw(
        &w.e.app,
        get(&w.job_uri("analytics/knowledge"), Some(&w.lena.1)),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_window"),
        "no days: {b}"
    );
    for ok in ["1", "90"] {
        let (s, _, b) = w.analytics(ok).await;
        assert_eq!(s, StatusCode::OK, "days={ok}: {b}");
    }
    let (s, b) = send(
        &w.e.app,
        get(
            "/api/jobs/no_such_job/analytics/knowledge?days=30",
            Some(&w.lena.1),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "job_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn analytics_empty_before_b4_is_zero_not_error() {
    let w = world().await;
    let (s, h, b) = w.analytics("30").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        h.get("x-agent-stale").is_none(),
        "no bound leaf, nothing to read"
    );
    for k in [
        "answers",
        "knowledge_answers",
        "grounded",
        "not_grounded",
        "dont_know",
        "never_cited_total",
        "documents_total",
    ] {
        assert_eq!(b[k], 0, "{k}: {b}");
    }
    for k in ["grounded_rate", "uncited_rate", "dont_know_rate"] {
        assert_eq!(b[k], Value::Null, "{k}: {b}");
    }
    for k in ["unanswered", "most_cited", "never_cited"] {
        assert_eq!(b[k], json!([]), "{k}: {b}");
    }
    assert_eq!(b["feedback"], json!({"up": 0, "down": 0, "reasons": []}));
    assert_eq!(b["job"], w.f.job.job_key.as_str());
    assert_eq!(b["days"], 30);
    let as_of = DateTime::parse_from_rfc3339(b["as_of"].as_str().unwrap()).unwrap();
    let since = DateTime::parse_from_rfc3339(b["since"].as_str().unwrap()).unwrap();
    assert_eq!(as_of - since, TimeDelta::days(30));
    assert!(
        !b["as_of"].as_str().unwrap().contains('.'),
        "whole seconds: {b}"
    );
}

#[tokio::test]
async fn analytics_prod_answers_only_feedback_all_surfaces() {
    let w = world().await;
    w.turn(Turn {
        citations: vec!["c1"],
        ..Turn::default()
    })
    .await;
    w.turn(Turn {
        dont_know: true,
        query: Some("price match"),
        question: "Do you price match?",
        ..Turn::default()
    })
    .await;
    // a test-chat answer (env test) is not production
    w.turn(Turn {
        env: "test",
        query: Some("test only gap"),
        ..Turn::default()
    })
    .await;
    // outside a 30-day window
    w.turn(Turn {
        days_ago: 40,
        query: Some("old gap"),
        ..Turn::default()
    })
    .await;
    // ratings from both surfaces count (of replies without a grounding record: not answers in the analytics)
    let (t1, cs1, t2) = (
        w.reply("conv-r1", "r1", None).await,
        w.reply("conv-r2", "r2", Some("CS-1")).await,
        w.reply("conv-r3", "r3", None).await,
    );
    let (s, b) = w
        .rate(
            &w.lena.1,
            rating(t1, json!({"vote": "down", "reason": "wrong"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .rate(
            &w.mara.1,
            rating(
                cs1,
                json!({"surface": "case", "vote": "down", "reason": "wrong"}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.rate(&w.mara.1, rating(t2, json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let (s, _, k) = w.analytics("30").await;
    assert_eq!(s, StatusCode::OK, "{k}");
    assert_eq!(
        (
            k["answers"].as_i64(),
            k["knowledge_answers"].as_i64(),
            k["grounded"].as_i64(),
            k["dont_know"].as_i64()
        ),
        (Some(2), Some(2), Some(1), Some(1)),
        "{k}"
    );
    assert_eq!(k["grounded_rate"], 0.5);
    let gaps: Vec<&str> = k["unanswered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["query"].as_str().unwrap())
        .collect();
    assert_eq!(gaps, ["price match"], "{k}");
    // the question is the customer message before the reply
    assert_eq!(k["unanswered"][0]["example"], "Do you price match?", "{k}");
    assert_eq!(
        k["feedback"],
        json!({"up": 1, "down": 2, "reasons": [{"reason": "wrong", "count": 2}]})
    );
    // the 40-day-old answer counts in a 90-day window
    let (_, _, k90) = w.analytics("90").await;
    assert_eq!(k90["answers"], 3, "{k90}");
}

#[tokio::test]
async fn analytics_most_cited_labels_platform_sources() {
    let w = world().await;
    let tenant_src = json!({"chunk_id": "c-t1", "document_id": "doc-t", "store_path": ["faq", "policy.returns"],
                            "title": "Returns policy", "page": 1, "excerpt": "", "layer": "tenant"});
    let platform_src = json!({"chunk_id": "c-p1", "document_id": "doc-p", "store_path": ["base"],
                              "title": "Consumer law", "page": null, "excerpt": "",
                              "layer": "platform", "pack": "base@2", "pack_layer": "base"});
    let platform_src2 = json!({"chunk_id": "c-p2", "document_id": "doc-p", "store_path": ["base"],
                               "title": "Consumer law", "page": null, "excerpt": "",
                               "layer": "platform", "pack": "base@2", "pack_layer": "base"});
    w.turn(Turn {
        citations: vec!["c-p1", "c-p2", "c-t1"],
        sources: json!([platform_src, platform_src2, tenant_src]),
        ..Turn::default()
    })
    .await;
    w.turn(Turn {
        citations: vec!["c-p1"],
        sources: json!([platform_src]),
        ..Turn::default()
    })
    .await;
    let (s, _, k) = w.analytics("30").await;
    assert_eq!(s, StatusCode::OK, "{k}");
    let cited: Vec<(String, Value, i64)> = k["most_cited"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["document_id"].as_str().unwrap().to_string(),
                d["store_path"].clone(),
                d["citations"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        cited,
        [
            ("doc-p".to_string(), json!(["platform:base@2"]), 2),
            ("doc-t".to_string(), json!(["faq", "policy.returns"]), 1),
        ],
        "two chunks of one document in one answer count once; platform sources are labelled by pin: {k}"
    );
    // R14 (KH-F3): the public route names the template layer a platform document's pack is referenced on
    assert_eq!(k["most_cited"][0]["pack_layer"], "base", "{k}");
    assert!(
        k["most_cited"][1].get("pack_layer").is_none(),
        "a tenant document has no pack layer: {k}"
    );
    // platform documents are never tenant documents
    assert_eq!(k["documents_total"], 0);
}

#[tokio::test]
async fn analytics_never_cited_reads_tenant_leaves_only() {
    let w = world().await;
    let (a, b, c) = (
        w.bound_leaf("faq-a").await,
        w.bound_leaf("faq-b").await,
        w.bound_leaf("faq-c").await,
    );
    let a1 = w.doc(a, "A1");
    let a2 = w.doc(a, "A2");
    let b1 = w.doc(b, "B1");
    let c1 = w.doc(c, "C1");
    let tag = Uuid::new_v4().simple().to_string();
    let pack_key = format!("t11-{}", &tag[..8]);
    let pack_root = lock(&w.fake.rag).add_pack(&pack_key, 1);
    let pin = format!("{pack_key}@1");
    let (s, b_) = send(
        &w.e.app,
        get("/api/knowledge/platform-packs", Some(&w.lena.1)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b_}");

    // the leaf set of a kind with a tenant leaf and a pack: only the tenant leaf is a never-cited source (R10)
    w.bind("support", "policy.returns", Some("faq-a"), &[pin.as_str()])
        .await;
    w.bind("exit", "policy.privacy", None, &[pin.as_str()])
        .await;
    let jr: JobRef = w.f.job_ref();
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let set = kinds::leaf_set(
        &mut tx,
        &w.e.state.design,
        &jr,
        "support",
        "policy.returns",
        &DesignOf::Draft,
    )
    .await
    .unwrap();
    assert_eq!(
        set.pins(),
        std::slice::from_ref(&pin),
        "the pin is in the set: {set:?}"
    );
    let leaves = analytics::bound_tenant_leaves(&mut tx, &w.e.state.design, &jr, &DesignOf::Draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        leaves.iter().map(|s| s.strata.0).collect::<Vec<_>>(),
        [a],
        "tenant leaves only, never a pack root"
    );

    // live: publish a + b (no packs), then bind c in the draft only
    w.bind("support", "policy.returns", Some("faq-a"), &[])
        .await;
    w.bind("exit", "policy.privacy", Some("faq-b"), &[]).await;
    w.publish().await;
    w.bind("exit", "policy.privacy", Some("faq-c"), &[]).await;
    w.turn(Turn {
        citations: vec!["c-a2"],
        sources: json!([{"chunk_id": "c-a2", "document_id": a2.to_string(), "store_path": ["faq-a"],
                         "title": "A2", "page": null, "excerpt": ""}]),
        ..Turn::default()
    })
    .await;
    let reads_before = w.document_reads().len();
    let (s, h, k) = w.analytics("30").await;
    assert_eq!(s, StatusCode::OK, "{k}");
    assert!(h.get("x-agent-stale").is_none(), "{h:?}");
    assert_eq!(
        (
            k["documents_total"].as_i64(),
            k["never_cited_total"].as_i64()
        ),
        (Some(3), Some(2)),
        "a and b of the live design, not the draft-only c: {k}"
    );
    let never: Vec<(String, Value)> = k["never_cited"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["document_id"].as_str().unwrap().to_string(),
                d["store_path"].clone(),
            )
        })
        .collect();
    // the smaller store first (b: 1 document, a: 2), then by title
    assert_eq!(
        never,
        [
            (b1.to_string(), json!(["faq-b"])),
            (a1.to_string(), json!(["faq-a"])),
        ],
        "{k}"
    );
    assert!(!k.to_string().contains(&c1.to_string()), "{k}");
    let reads: Vec<String> = w.document_reads().split_off(reads_before);
    assert_eq!(reads.len(), 2, "{reads:?}");
    for r in &reads {
        assert!(
            r.contains(&a.to_string()) || r.contains(&b.to_string()),
            "only bound tenant leaves are read: {reads:?}"
        );
        assert!(!r.contains(&pack_root.to_string()), "{reads:?}");
    }
}

/// After a model change the job's pinned generation runs on another strata store: never-cited lists that store and
/// labels its documents by the agent store (its key path), never by the generation's strata id.
#[tokio::test]
async fn never_cited_follows_the_pinned_generation_and_keeps_the_agent_store() {
    let w = world().await;
    let old = w.bound_leaf("faq-a").await;
    let store: Uuid =
        sqlx::query_scalar("SELECT id FROM knowledge_store WHERE tenant_id = $1 AND key = 'faq-a'")
            .bind(w.tenant)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    let new = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "faq-a-g2");
    let initial = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, build, state, created_by) \
         VALUES ($1, $2, $3, 'initial', 'ready', 'test')",
    )
    .bind(w.tenant)
    .bind(initial)
    .bind(store)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let g2 = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, strata_store_id, embed, build, state, source_id, \
                                       created_by) \
         VALUES ($1, $2, $3, $4, 'e-new', 'reembed', 'ready', $5, 'test')",
    )
    .bind(w.tenant)
    .bind(g2)
    .bind(store)
    .bind(new)
    .bind(initial)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let jr: JobRef = w.f.job_ref();
    sqlx::query(
        "INSERT INTO job_embedding_pin (tenant_id, job_id, store_key, generation_id, updated_by) \
         VALUES ($1, $2, 'faq-a', $3, 'test')",
    )
    .bind(w.tenant)
    .bind(jr.id)
    .bind(g2)
    .execute(&w.e.owner)
    .await
    .unwrap();
    w.bind("support", "policy.returns", Some("faq-a"), &[])
        .await;
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let leaves = analytics::bound_tenant_leaves(&mut tx, &w.e.state.design, &jr, &DesignOf::Draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        leaves,
        [analytics::BoundLeaf {
            store,
            strata: StrataStoreId(new),
        }],
        "the pinned generation's strata store, labelled by the agent store (never {old})"
    );
    // the label the route gives a document of that leaf resolves to the agent store's key path
    let k = analytics::analytics_of(&Input {
        job: jr.key.clone(),
        days: 30,
        as_of: Utc::now(),
        answers: Vec::new(),
        feedback: Vec::new(),
        documents: vec![DocRow {
            id: "d-1".into(),
            store_id: leaves[0].store.to_string(),
            title: "A1".into(),
        }],
        stores: vec![StoreNode {
            id: store.to_string(),
            key: "faq-a".into(),
            parent_id: None,
        }],
    });
    let k = serde_json::to_value(&k).unwrap();
    assert_eq!(k["never_cited"][0]["store_path"], json!(["faq-a"]), "{k}");
}

#[tokio::test]
async fn analytics_stale_when_strata_cannot_list_documents() {
    let w = world().await;
    let a = w.bound_leaf("faq-a").await;
    w.doc(a, "A1");
    w.bind("support", "policy.returns", Some("faq-a"), &[])
        .await;
    w.publish().await;
    lock(&w.fake.rag).script(
        "GET",
        &format!("/api/stores/{a}/documents"),
        503,
        json!({"error": {"code": "unavailable", "message": "down"}}),
    );
    let (s, h, k) = w.analytics("30").await;
    assert_eq!(s, StatusCode::OK, "{k}");
    assert_eq!(
        h.get("x-agent-stale").and_then(|v| v.to_str().ok()),
        Some("strata_rag")
    );
    assert_eq!(k["documents_total"], 0, "{k}");
}

#[tokio::test]
async fn answer_grounding_matches_its_fields_and_keys_one_agent_reply() {
    let w = world().await;
    let reply = w.reply("conv-g", "Do you price match?", None).await;
    let question: Uuid = sqlx::query_scalar(
        "SELECT message_id FROM conversation_message WHERE conversation_id = 'conv-g' AND role = 'customer'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    // used knowledge, no citation, not "I don't know" → not_grounded; storing "grounded" is refused
    let err = sqlx::query(
        "INSERT INTO answer_grounding (tenant_id, job_id, message_id, used_knowledge, citations, dont_know, grounding) \
         VALUES ($1, $2, $3, true, '{}', false, 'grounded')",
    )
    .bind(w.tenant)
    .bind(w.f.job.job_id)
    .bind(reply)
    .execute(&w.e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    // only an agent reply carries a grounding record: the customer's message is refused
    assert!(
        !w.ground(question, &dont_know()).await,
        "a customer message has no grounding"
    );
    let err = sqlx::query(
        "INSERT INTO answer_grounding (tenant_id, job_id, message_id, grounding) VALUES ($1, $2, $3, 'no_knowledge')",
    )
    .bind(w.tenant)
    .bind(w.f.job.job_id)
    .bind(question)
    .execute(&w.e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    // another tenant cannot attach a record to this tenant's reply
    let other = Uuid::new_v4();
    insert_tenant(&w.e.owner, other, &format!("T11 other {other}")).await;
    let err = sqlx::query(
        "INSERT INTO answer_grounding (tenant_id, job_id, message_id, grounding) VALUES ($1, $2, $3, 'no_knowledge')",
    )
    .bind(other)
    .bind(w.f.job.job_id)
    .bind(reply)
    .execute(&w.e.owner)
    .await
    .unwrap_err();
    assert!(
        matches!(sqlstate(&err).as_deref(), Some("23514" | "23503")),
        "{err}"
    );
    // 1:1 — the runtime records a reply once
    assert!(w.ground(reply, &dont_know()).await, "first record");
    assert!(!w.ground(reply, &dont_know()).await, "a second record");
    let verdict: String =
        sqlx::query_scalar("SELECT grounding FROM answer_grounding WHERE message_id = $1")
            .bind(reply)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(verdict, "dont_know", "the hook derives the verdict");
    // a reason on an up vote is refused
    let err = sqlx::query(
        "INSERT INTO answer_feedback (tenant_id, job_id, id, message_id, surface, vote, reason, by_sub) \
         VALUES ($1, $2, $3, $4, 'case', 'up', 'tone', 'u-x')",
    )
    .bind(w.tenant)
    .bind(w.f.job.job_id)
    .bind(Uuid::new_v4())
    .bind(reply)
    .execute(&w.e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    // a rating references an agent reply too
    let err = sqlx::query(
        "INSERT INTO answer_feedback (tenant_id, job_id, id, message_id, surface, vote, by_sub) \
         VALUES ($1, $2, $3, $4, 'case', 'up', 'u-x')",
    )
    .bind(w.tenant)
    .bind(w.f.job.job_id)
    .bind(Uuid::new_v4())
    .bind(question)
    .execute(&w.e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

// ---------------------------------------------------------------------------------------------------------------
// The port: web/tests/unit/grounding.test.ts `knowledge analytics`

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// A cited source (the reference's `src`; chunk ids are not part of it: citations count per document).
fn src(document_id: &str) -> Source {
    Source {
        document_id: document_id.to_string(),
        title: document_id.to_uppercase(),
        store_path: vec!["root".into(), "leaf".into()],
        layer: None,
        pack: None,
        pack_layer: None,
    }
}

fn row(at: &str, f: impl FnOnce(&mut AnswerRow)) -> AnswerRow {
    let mut r = AnswerRow {
        at: t(at),
        question: "q".into(),
        query: Some("q".into()),
        used_knowledge: true,
        citations: vec![],
        sources: vec![],
        dont_know: false,
    };
    f(&mut r);
    r
}

fn fb(vote: &str, reason: Option<&str>, at: &str) -> FeedbackRow {
    FeedbackRow {
        vote: vote.into(),
        reason: reason.map(str::to_string),
        at: t(at),
    }
}

fn doc(id: &str, store: &str) -> DocRow {
    DocRow {
        id: id.into(),
        store_id: store.into(),
        title: id.into(),
    }
}

fn store(id: &str, key: &str, parent: Option<&str>) -> StoreNode {
    StoreNode {
        id: id.into(),
        key: key.into(),
        parent_id: parent.map(str::to_string),
    }
}

#[test]
fn analytics_matches_ts_reference() {
    let a = src("doc-a");
    let a2 = src("doc-a");
    let b = src("doc-b");
    let cites = |r: &mut AnswerRow, c: &[&str], s: Vec<Source>| {
        r.citations = c.iter().map(|x| x.to_string()).collect();
        r.sources = s;
    };
    let k = analytics::analytics_of(&Input {
        job: "j".into(),
        days: 7,
        as_of: t("2026-10-18T12:00:00Z"),
        answers: vec![
            row("2026-10-17T10:00:00Z", |r| {
                cites(r, &["doc-a-c1", "doc-a-c2"], vec![a.clone(), a2.clone()])
            }),
            row("2026-10-16T10:00:00Z", |r| {
                cites(r, &["doc-a-c1", "doc-b-c1"], vec![a.clone(), b.clone()])
            }),
            row("2026-10-15T10:00:00Z", |r| {
                r.query = Some("Price match".into());
                r.question = "Do you price match?".into();
                r.dont_know = true;
            }),
            row("2026-10-16T09:00:00Z", |r| {
                r.query = Some("price match ".into());
                r.question = "Match a lower price?".into();
                r.dont_know = true;
            }),
            row("2026-10-14T10:00:00Z", |r| {
                r.query = Some("opening hours".into());
                r.question = "When are you open?".into();
            }),
            row("2026-10-14T11:00:00Z", |r| {
                r.used_knowledge = false;
                r.query = None;
            }),
            // Outside the 7-day window.
            row("2026-10-01T10:00:00Z", |r| r.query = Some("old gap".into())),
        ],
        feedback: vec![
            fb("up", None, "2026-10-17T00:00:00Z"),
            fb("down", Some("wrong"), "2026-10-17T00:00:00Z"),
            fb("down", Some("wrong"), "2026-10-19T08:00:00Z"),
            fb("down", Some("tone"), "2026-10-16T00:00:00Z"),
            fb("down", None, "2026-10-16T00:00:00Z"),
            fb("down", Some("other"), "2026-09-01T00:00:00Z"),
        ],
        documents: vec![
            doc("doc-a", "s1"),
            doc("doc-b", "s1"),
            doc("doc-c", "s1"),
            doc("doc-z", "s2"),
            doc("doc-y", "s2"),
            doc("doc-x", "s2"),
            doc("doc-w", "s2"),
        ],
        stores: vec![
            store("root", "faq", None),
            store("s1", "policy.returns", Some("root")),
            store("s2", "faq.general", Some("root")),
        ],
    });
    let v = serde_json::to_value(&k).unwrap();
    assert_eq!(v["since"], "2026-10-11T12:00:00Z");
    assert_eq!(v["as_of"], "2026-10-18T12:00:00Z");
    assert_eq!(
        (
            k.answers,
            k.knowledge_answers,
            k.grounded,
            k.not_grounded,
            k.dont_know
        ),
        (6, 5, 2, 1, 2)
    );
    assert_eq!(
        (k.grounded_rate, k.uncited_rate, k.dont_know_rate),
        (Some(0.4), Some(0.2), Some(0.4))
    );
    // The two price-match asks are one gap (query normalised), most asked first.
    let gaps: Vec<(&str, usize, String)> = k
        .unanswered
        .iter()
        .map(|u| (u.query.as_str(), u.count, analytics::iso(u.last_at)))
        .collect();
    assert_eq!(
        gaps,
        [
            ("Price match", 2, "2026-10-16T09:00:00Z".to_string()),
            ("opening hours", 1, "2026-10-14T10:00:00Z".to_string()),
        ]
    );
    assert_eq!(k.unanswered[0].example, "Do you price match?");
    // doc-a: two chunks in one answer count once → 2 answers; doc-b 1.
    let cited: Vec<(&str, usize)> = k
        .most_cited
        .iter()
        .map(|d| (d.document_id.as_str(), d.citations))
        .collect();
    assert_eq!(cited, [("doc-a", 2), ("doc-b", 1)]);
    assert_eq!(v["most_cited"][0]["last_cited_at"], "2026-10-17T10:00:00Z");
    // Never cited: the smaller (curated) store first (s1: 3 docs, s2: 4), then by title.
    let never: Vec<&str> = k
        .never_cited
        .iter()
        .map(|d| d.document_id.as_str())
        .collect();
    assert_eq!(never, ["doc-c", "doc-w", "doc-x", "doc-y", "doc-z"]);
    assert_eq!(k.never_cited[0].store_path, ["faq", "policy.returns"]);
    assert_eq!((k.never_cited_total, k.documents_total), (5, 7));
    assert_eq!(
        v["feedback"],
        json!({"up": 1, "down": 4, "reasons": [{"reason": "wrong", "count": 2}, {"reason": "tone", "count": 1}]})
    );

    // rates are null without knowledge answers
    let k = analytics::analytics_of(&Input {
        job: "j".into(),
        days: 30,
        as_of: t("2026-10-18T12:00:00Z"),
        answers: vec![row("2026-10-17T10:00:00Z", |r| r.used_knowledge = false)],
        feedback: vec![],
        documents: vec![],
        stores: vec![],
    });
    assert_eq!(
        (
            k.grounded_rate,
            k.uncited_rate,
            k.dont_know_rate,
            k.knowledge_answers,
            k.answers
        ),
        (None, None, None, 0, 1)
    );
    let v = serde_json::to_value(&k).unwrap();
    assert_eq!(v["grounded_rate"], Value::Null);
}
