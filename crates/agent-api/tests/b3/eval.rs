//! B3 T9: golden questions + async eval runs (plan §5.6, D72, §0 R7/R10): the question CRUD and import pass strata's
//! answers through, `runEval` answers strata's 202 with the `Location` rewritten to agent's route and never waits
//! for the scoring, the root is resolved agent-side before strata is called (D91: `root_required`, `not_found`) and
//! translated agent store id ↔ strata store id both ways, writes send `X-Actor-Id` and need `knowledge_manager`.
//! Real Postgres 18 (one container for this binary, a fresh database per test) and the in-process strata fakes
//! (`common::strata_fake`).
use crate::common;

use std::time::{Duration, Instant};

use crate::common::strata_fake::{self, Recorded, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use agent_api::upstream;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// knowledge_manager; its `sub` is a uuid, so strata gets `X-Actor-Id`
    km: String,
    km_sub: String,
    /// a member without a role
    obs: String,
}

async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("T9 {tenant}")).await;
    let km_sub = Uuid::new_v4().to_string();
    insert_user(&e.owner, &km_sub, tenant, "knowledge_manager").await;
    let km = person(&km_sub, &[]);
    let obs_sub = format!("u-obs-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &obs_sub, tenant, "tenant_user").await;
    let obs = person(&obs_sub, &[]);
    W {
        e,
        fake,
        tenant,
        km,
        km_sub,
        obs,
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

impl W {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.e.app, json(method, uri, Some(tok), body)).await
    }

    async fn read(&self, uri: &str) -> (StatusCode, Value) {
        send(&self.e.app, get(uri, Some(&self.obs))).await
    }

    async fn start(&self, body: Value) -> (StatusCode, HeaderMap, Value) {
        send_raw(
            &self.e.app,
            json("POST", "/api/knowledge/eval/runs", Some(&self.km), body),
        )
        .await
    }

    /// A store in strata (router at the top, leaf below) and its bound local row; (agent id, strata id).
    async fn store(&self, key: &str, parent: Option<(Uuid, Uuid)>) -> (Uuid, Uuid) {
        let (kind, strata_kind) = if parent.is_none() {
            ("group", "router")
        } else {
            ("leaf", "leaf")
        };
        let sid =
            lock(&self.fake.rag).add_store(self.tenant, parent.map(|p| p.1), strata_kind, key);
        let id = self.local(key, kind, parent.map(|p| p.0), Some(sid)).await;
        (id, sid)
    }

    /// A local store row inserted as the owner; its id.
    async fn local(&self, key: &str, kind: &str, parent: Option<Uuid>, sid: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, updated_by) \
             VALUES ($1, $2, $3, $3, $4, $5, $6, 'eu', 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(key)
        .bind(parent)
        .bind(kind)
        .bind(sid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        id
    }

    /// A stored golden question in the fake (strata's row shape).
    fn seed_question(&self, query: &str) -> Uuid {
        let id = Uuid::new_v4();
        lock(&self.fake.rag).questions.push((
            self.tenant,
            json!({"id": id, "query": query, "expected_document": "Returns policy", "expected_leaf": null}),
        ));
        id
    }

    fn rag(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }

    async fn audit_count(&self, action: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = $2")
            .bind(self.tenant)
            .bind(action)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn questions_crud_passthrough_201_204() {
    let w = world().await;
    let (s, row) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/eval/questions",
            json!({"query": "How long do I have to return a sofa?", "expected_document": "Returns policy"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{row}");
    let id = row["id"].as_str().expect("id").to_string();
    assert_eq!(row["query"], "How long do I have to return a sofa?");

    let (s, page) = w.read("/api/knowledge/eval/questions").await;
    assert_eq!(s, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["items"][0]["id"], json!(id));

    let uri = format!("/api/knowledge/eval/questions/{id}");
    let (s, row) = w
        .call(
            &w.km,
            "PATCH",
            &uri,
            json!({"expected_leaf": "policy/returns"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{row}");
    assert_eq!(row["expected_leaf"], "policy/returns");
    assert_eq!(
        w.rag("PATCH", &format!("/api/eval/questions/{id}")).len(),
        1
    );

    let (s, b) = w.call(&w.km, "DELETE", &uri, Value::Null).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = w.call(&w.km, "DELETE", &uri, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "not_found"),
        "strata's 404 passes through: {b}"
    );

    assert_eq!(w.audit_count("eval_question.create").await, 1);
    assert_eq!(w.audit_count("eval_question.update").await, 1);
    assert_eq!(
        w.audit_count("eval_question.delete").await,
        1,
        "a refused delete is not audited"
    );
}

#[tokio::test]
async fn create_duplicate_409_question_exists_passthrough() {
    let w = world().await;
    let q = json!({"query": "Where is my order?", "expected_text": "tracking link"});
    let (s, b) = w
        .call(&w.km, "POST", "/api/knowledge/eval/questions", q.clone())
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/eval/questions",
            json!({"query": "WHERE IS MY ORDER?", "expected_text": "tracking link"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "question_exists"),
        "{b}"
    );
    // strata's other refusals keep their status and code too
    lock(&w.fake.rag).script(
        "POST",
        "/api/eval/questions",
        422,
        json!({"error": {"code": "too_many_questions", "message": "too many questions: at most 200"}}),
    );
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/eval/questions",
            json!({"query": "One more", "expected_text": "x"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "too_many_questions"),
        "{b}"
    );
    assert_eq!(w.audit_count("eval_question.create").await, 1);
}

#[tokio::test]
async fn import_json_all_or_nothing_201() {
    let w = world().await;
    let rows = json!({"questions": [
        {"query": "Can I pay by invoice?", "expected_document": "Payment"},
        {"query": "Do you ship abroad?", "expected_document": "Shipping"},
    ]});
    let (s, b) = w
        .call(&w.km, "POST", "/api/knowledge/eval/questions/import", rows)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b, json!({"added": 2, "total": 2}));
    let sent = w.rag("POST", "/api/eval/questions/import");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(sent[0].body["questions"].as_array().map(Vec::len), Some(2));
    assert_eq!(w.audit_count("eval_question.import").await, 1);

    // strata refuses the whole import: nothing is stored, the refusal passes through, nothing is audited
    lock(&w.fake.rag).script(
        "POST",
        "/api/eval/questions/import",
        422,
        json!({"error": {"code": "invalid_input", "message": "invalid input: nothing imported: question 2: query is empty"}}),
    );
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/eval/questions/import",
            json!({"questions": [{"query": "Fine", "expected_text": "x"}, {"query": "", "expected_text": "y"}]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    assert_eq!(
        lock(&w.fake.rag)
            .questions
            .iter()
            .filter(|(t, _)| *t == w.tenant)
            .count(),
        2
    );
    assert_eq!(w.audit_count("eval_question.import").await, 1);
}

#[tokio::test]
async fn run_eval_202_with_rewritten_location() {
    let w = world().await;
    w.store("faq", None).await;
    w.seed_question("How do returns work?");
    let (s, h, b) = w.start(json!({})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "running");
    let id = b["id"].as_str().expect("run id");
    let loc = h
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .expect("Location");
    assert_eq!(loc, format!("/api/knowledge/eval/runs/{id}"));
    assert!(!loc.starts_with("/api/eval/"), "strata's path never leaks");
    let (s, run) = w.read(loc).await;
    assert_eq!(s, StatusCode::OK, "the Location is served: {run}");
    assert_eq!(run["id"], json!(id));
    assert_eq!(w.audit_count("eval_run.start").await, 1);
}

#[tokio::test]
async fn run_eval_second_409_eval_running() {
    let w = world().await;
    w.store("faq", None).await;
    w.seed_question("How do returns work?");
    let (s, _, b) = w.start(json!({})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let (s, _, b) = w.start(json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "eval_running"), "{b}");
    assert_eq!(w.audit_count("eval_run.start").await, 1);
}

#[tokio::test]
async fn run_eval_translates_root_id_both_ways() {
    let w = world().await;
    let (root, root_sid) = w.store("faq", None).await;
    let (leaf, _) = w.store("faq-returns", Some((root, root_sid))).await;
    w.seed_question("How do returns work?");

    // a store of the tree names that tree: its root goes to strata, in strata's ids
    let (s, _, b) = w.start(json!({"root_id": leaf})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sent = w.rag("POST", "/api/eval");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body["root_id"], json!(root_sid));
    assert_eq!(sent[0].body["source"], "tenant");
    // and every run row answers in agent ids
    assert_eq!(b["root_id"], json!(root), "202 body");
    let id = b["id"].as_str().unwrap().to_string();
    let (_, run) = w.read(&format!("/api/knowledge/eval/runs/{id}")).await;
    assert_eq!(run["root_id"], json!(root), "detail");
    let (_, page) = w.read("/api/knowledge/eval/runs").await;
    assert_eq!(page["items"][0]["root_id"], json!(root), "list");

    // a run on a store the tree no longer holds answers null, never a strata id
    sqlx::query("DELETE FROM knowledge_store WHERE tenant_id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let (_, run) = w.read(&format!("/api/knowledge/eval/runs/{id}")).await;
    assert_eq!(run["root_id"], Value::Null);
}

#[tokio::test]
async fn run_eval_inline_questions_passthrough() {
    let w = world().await;
    let (_, root_sid) = w.store("faq", None).await;
    let inline =
        json!([{"query": "Is the Oslo sofa in stock?", "expected_document": "SKU LL-10204"}]);
    let (s, _, b) = w
        .start(
            json!({"questions": inline, "modes": ["embed", "flat"], "beam": 2, "min_score": 0.5}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["source"], "inline");
    let sent = w.rag("POST", "/api/eval");
    assert_eq!(
        sent[0].body["questions"], inline,
        "inline questions go as sent"
    );
    assert_eq!(sent[0].body["modes"], json!(["embed", "flat"]));
    assert_eq!(sent[0].body["beam"], 2);
    assert_eq!(sent[0].body["root_id"], json!(root_sid));
    assert!(sent[0].body.get("question_ids").is_none());
    assert!(
        lock(&w.fake.rag)
            .questions
            .iter()
            .all(|(t, _)| *t != w.tenant),
        "inline questions are never saved"
    );
}

#[tokio::test]
async fn run_eval_without_root_on_two_roots_422_root_required() {
    let w = world().await;
    w.store("faq", None).await;
    w.store("hr", None).await;
    w.seed_question("How do returns work?");
    let (s, _, b) = w.start(json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "root_required"),
        "{b}"
    );
    let mut roots: Vec<String> =
        serde_json::from_value(b["error"]["roots"].clone()).unwrap_or_default();
    roots.sort();
    assert_eq!(roots, ["faq", "hr"], "{b}");
    assert!(
        w.rag("POST", "/api/eval").is_empty(),
        "refused before strata"
    );
}

#[tokio::test]
async fn run_eval_unknown_root_404_not_found() {
    let w = world().await;
    w.store("faq", None).await;
    w.seed_question("How do returns work?");
    let (s, _, b) = w.start(json!({"root_id": Uuid::new_v4()})).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "not_found"), "{b}");
    assert!(w.rag("POST", "/api/eval").is_empty());

    // a root strata_rag does not hold yet: not ready, still no call
    let pending = w.local("hr", "group", None, None).await;
    let (s, _, b) = w.start(json!({"root_id": pending})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "eval_not_ready"),
        "{b}"
    );
    assert!(w.rag("POST", "/api/eval").is_empty());
}

#[tokio::test]
async fn runs_cursor_passthrough() {
    let w = world().await;
    let (root, root_sid) = w.store("faq", None).await;
    let run = json!({"id": Uuid::new_v4(), "source": "tenant", "root_id": root_sid, "status": "completed",
                     "question_count": 3});
    lock(&w.fake.rag).script(
        "GET",
        "/api/eval/runs",
        200,
        json!({"items": [run], "next": "opaque-c2"}),
    );
    let (s, page) = w
        .read("/api/knowledge/eval/runs?limit=5&cursor=opaque-c1")
        .await;
    assert_eq!(s, StatusCode::OK, "{page}");
    assert_eq!(
        page["next"], "opaque-c2",
        "strata's cursor comes back untouched"
    );
    assert_eq!(page["items"][0]["root_id"], json!(root));
    let sent = w.rag("GET", "/api/eval/runs");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].query, "limit=5&cursor=opaque-c1");
}

#[tokio::test]
async fn run_detail_running_then_completed() {
    let w = world().await;
    w.store("faq", None).await;
    w.seed_question("How do returns work?");
    let (s, _, b) = w.start(json!({})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let id = b["id"].as_str().unwrap().to_string();
    let uri = format!("/api/knowledge/eval/runs/{id}");

    let (s, run) = w.read(&uri).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "running");
    assert_eq!(run["finished_at"], Value::Null);

    // strata finishes the scoring in the background; the next poll sees it
    {
        let mut g = lock(&w.fake.rag);
        let (_, r) = g
            .runs
            .iter_mut()
            .find(|(_, r)| r["id"] == json!(id))
            .expect("run");
        r["status"] = json!("completed");
        r["verdict"] = json!("PASS");
        r["progress"] = json!({"done": 2, "total": 2});
        r["finished_at"] = json!("2026-10-21T10:00:05Z");
        r["took_ms"] = json!(5000);
    }
    let (s, run) = w.read(&uri).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "completed");
    assert_eq!(run["verdict"], "PASS");
    assert!(run["results"].is_array());

    let (s, b) = w
        .read(&format!("/api/knowledge/eval/runs/{}", Uuid::new_v4()))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "not_found"), "{b}");
}

#[tokio::test]
async fn writes_send_x_actor_id() {
    let w = world().await;
    w.store("faq", None).await;
    let (_, row) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/eval/questions",
            json!({"query": "Warranty length?", "expected_text": "two years"}),
        )
        .await;
    let id = row["id"].as_str().expect("id").to_string();
    let q = format!("/api/knowledge/eval/questions/{id}");
    w.call(&w.km, "PATCH", &q, json!({"expected_text": "2 years"}))
        .await;
    w.call(
        &w.km,
        "POST",
        "/api/knowledge/eval/questions/import",
        json!({"questions": [{"query": "Invoice?", "expected_text": "invoice"}]}),
    )
    .await;
    let (s, _, b) = w.start(json!({})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    w.call(&w.km, "DELETE", &q, Value::Null).await;

    let writes: Vec<Recorded> = w
        .fake
        .rag_requests()
        .into_iter()
        .filter(|r| r.method != "GET" && r.path.starts_with("/api/eval"))
        .collect();
    assert_eq!(writes.len(), 5, "{writes:?}");
    for r in &writes {
        assert_eq!(
            r.actor.as_deref(),
            Some(w.km_sub.as_str()),
            "{} {}",
            r.method,
            r.path
        );
        assert_eq!(
            r.tenant_header.as_deref(),
            Some(w.tenant.to_string().as_str())
        );
    }
}

#[tokio::test]
async fn tenant_user_cannot_write_403() {
    let w = world().await;
    w.store("faq", None).await;
    let q = format!("/api/knowledge/eval/questions/{}", Uuid::new_v4());
    let writes = [
        (
            "POST",
            "/api/knowledge/eval/questions".to_string(),
            json!({"query": "x", "expected_text": "y"}),
        ),
        ("PATCH", q.clone(), json!({"expected_text": "z"})),
        ("DELETE", q, Value::Null),
        (
            "POST",
            "/api/knowledge/eval/questions/import".to_string(),
            json!({"questions": []}),
        ),
        ("POST", "/api/knowledge/eval/runs".to_string(), json!({})),
    ];
    for (m, uri, body) in writes {
        let (s, b) = w.call(&w.obs, m, &uri, body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {uri}: {b}");
    }
    assert!(
        w.fake
            .rag_requests()
            .iter()
            .all(|r| r.method == "GET" || !r.path.starts_with("/api/eval")),
        "no write reached strata"
    );
    // reads stay open to every member
    let (s, b) = w.read("/api/knowledge/eval/questions").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.read("/api/knowledge/eval/runs").await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn proxy_timeout_is_10s_never_blocks_on_scoring() {
    assert_eq!(upstream::DEFAULT_TIMEOUT, Duration::from_secs(10));
    let w = world().await;
    w.store("faq", None).await;
    for i in 0..50 {
        w.seed_question(&format!("Question number {i}?"));
    }
    // the fake never finishes the run: agent answers with strata's 202 and does not wait for the scoring
    let t0 = Instant::now();
    let (s, _, b) = w
        .start(json!({"modes": ["embed", "laya", "hybrid", "flat"]}))
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "running");
    assert!(
        t0.elapsed() < upstream::DEFAULT_TIMEOUT,
        "runEval returned only after {:?}",
        t0.elapsed()
    );
    assert_eq!(b["progress"]["done"], 0);
}
