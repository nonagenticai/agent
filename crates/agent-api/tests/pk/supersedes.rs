//! PK T6 + T6b (plan PK-8): tenant `supersedes` on sources and uploads. A source body and an upload form name the
//! platform topic keys they replace; the value is checked against strata's limits (≤32 keys, each trimmed to 1-128
//! characters of `[a-z0-9_./-]`, crate::knowledge::topic_keys), stored sorted and de-duplicated in
//! `source.supersedes` / `knowledge_upload.supersedes` and returned in the source JSON. strata_ingest #15 receives it
//! (T6b): an upload and a file-source run as a `supersedes` text field BEFORE the first `file` part, a website / api
//! run top-level in the JSON body, always (`[]` included). T5 also sends the union at query time. Real Postgres 18
//! and the in-process strata fakes.
use crate::common;

use crate::common::strata_fake::{self, FormPart, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const JOB: &str = "support";
const BOUNDARY: &str = "pk-t6-boundary-91d4";

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// A leaf bound to a strata store.
    leaf: Uuid,
    /// knowledge_manager
    km: String,
}

/// A fresh tenant with the job `support` (nl, en), one bound leaf `pk.t6.leaf` and a knowledge_manager.
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("PK T6 {tenant}")).await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, languages, created_by) \
         VALUES ($1, $2, 'Support', '[]', ARRAY['nl', 'en'], 'test')",
    )
    .bind(tenant)
    .bind(JOB)
    .execute(&e.owner)
    .await
    .unwrap();
    let leaf = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, updated_by) \
         VALUES ($1, $2, 'pk.t6.leaf', 'pk.t6.leaf', NULL, 'leaf', $3, 'eu', 'test')",
    )
    .bind(tenant)
    .bind(leaf)
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap();
    let sub = format!("u-km-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, "knowledge_manager").await;
    let km = person(&sub, &[]);
    W {
        e,
        fake,
        tenant,
        leaf,
        km,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

fn message(b: &Value) -> String {
    b["error"]["message"].as_str().unwrap_or("").to_string()
}

fn uri(rest: &str) -> String {
    format!("/api/jobs/{JOB}/sources{rest}")
}

/// One form field: (name, file name of a file field, bytes).
type Field<'a> = (&'a str, Option<&'a str>, Vec<u8>);

fn form(fields: &[Field<'_>]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, file, data) in fields {
        b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let head = match file {
            Some(f) => format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            ),
            None => format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n"),
        };
        b.extend_from_slice(head.as_bytes());
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    b
}

impl W {
    async fn call(&self, method: &str, rest: &str, body: Value) -> (StatusCode, Value) {
        send(&self.e.app, json(method, &uri(rest), Some(&self.km), body)).await
    }

    /// A file source body (no URL to check) into the leaf.
    fn file_source(&self, name: &str) -> Value {
        json!({"name": name, "type": "file", "config": {}, "mapping": [], "target_store_id": self.leaf,
               "schedule": null, "enabled": true})
    }

    fn with(&self, name: &str, supersedes: Value) -> Value {
        let mut b = self.file_source(name);
        b["supersedes"] = supersedes;
        b
    }

    /// `source.supersedes` and `updated_by` as stored.
    async fn stored(&self, id: &str) -> (Vec<String>, String) {
        sqlx::query_as("SELECT supersedes, updated_by FROM source WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    async fn sources_named(&self, name: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM source WHERE tenant_id = $1 AND name = $2")
            .bind(self.tenant)
            .bind(name)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    async fn audit(&self, action: &str, id: &str) -> Vec<Value> {
        sqlx::query_scalar(
            "SELECT details FROM audit_event WHERE action = $1 AND resource_id = $2 ORDER BY id",
        )
        .bind(action)
        .bind(id)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    async fn upload(&self, fields: &[Field<'_>]) -> (StatusCode, Value) {
        let req = Request::post(format!("/api/knowledge/stores/{}/upload", self.leaf))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header("authorization", format!("Bearer {}", self.km))
            .body(Body::from(form(fields)))
            .unwrap();
        send(&self.e.app, req).await
    }

    /// Every upload of the tenant: (supersedes, audit-free) oldest first.
    async fn uploads(&self) -> Vec<Vec<String>> {
        sqlx::query_scalar(
            "SELECT supersedes FROM knowledge_upload WHERE tenant_id = $1 ORDER BY created_at, id",
        )
        .bind(self.tenant)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    fn ingest_posts(&self) -> usize {
        self.fake
            .ingest_requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/api/ingest/jobs")
            .count()
    }

    /// The JSON bodies of this tenant's `POST /api/ingest/jobs` (a multipart one records `Null`).
    fn ingest_json_bodies(&self) -> Vec<Value> {
        self.fake
            .ingest_requests()
            .into_iter()
            .filter(|r| {
                r.method == "POST" && r.path == "/api/ingest/jobs" && r.tenant == Some(self.tenant)
            })
            .map(|r| r.body)
            .collect()
    }

    /// The parts of this tenant's multipart ingest requests, oldest first.
    fn forms(&self) -> Vec<Vec<FormPart>> {
        lock(&self.fake.ingest)
            .forms
            .iter()
            .filter(|(t, _)| *t == self.tenant)
            .map(|(_, f)| f.clone())
            .collect()
    }

    async fn resync(&self, id: &str) -> (StatusCode, Value) {
        self.call("POST", &format!("/{id}/resync"), Value::Null)
            .await
    }

    async fn resync_files(&self, id: &str, fields: &[Field<'_>]) -> (StatusCode, Value) {
        let req = Request::post(uri(&format!("/{id}/resync")))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header("authorization", format!("Bearer {}", self.km))
            .body(Body::from(form(fields)))
            .unwrap();
        send(&self.e.app, req).await
    }
}

/// The `supersedes` text field of a recorded form, parsed, and whether it came before the first file part.
fn supersedes_in(parts: &[FormPart]) -> (Value, bool) {
    let at = parts
        .iter()
        .position(|p| p.name == "supersedes" && p.file_name.is_none())
        .expect("a supersedes field");
    let first_file = parts
        .iter()
        .position(|p| p.file_name.is_some())
        .expect("a file part");
    (
        serde_json::from_str(&parts[at].value).expect("JSON"),
        at < first_file,
    )
}

fn file(name: &str) -> Field<'_> {
    ("file", Some(name), b"%PDF-1.7 pk".to_vec())
}

fn supersedes_field(v: &str) -> Field<'static> {
    ("supersedes", None, v.as_bytes().to_vec())
}

// ---------------------------------------------------------------------------------------------------------------
// Sources

/// Create, read, list and save: absent = `[]` on create and keep on save, `[]` clears, a list is trimmed, sorted and
/// de-duplicated; every save stamps `updated_by`; the audit detail names the list stored.
#[tokio::test]
async fn source_supersedes_round_trips() {
    let w = world().await;

    // absent on create: []
    let (s, b) = w.call("POST", "", w.file_source("Plain")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["supersedes"], json!([]), "{b}");

    // a list: trimmed, sorted, de-duplicated
    let (s, b) = w
        .call(
            "POST",
            "",
            w.with(
                "Returns",
                json!([" returns/policy ", "hours", "returns/policy"]),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["id"].as_str().unwrap().to_string();
    assert_eq!(b["supersedes"], json!(["hours", "returns/policy"]), "{b}");
    let (keys, _) = w.stored(&id).await;
    assert_eq!(keys, ["hours", "returns/policy"]);
    let audit = w.audit("source.create", &id).await;
    assert_eq!(audit[0]["supersedes"], json!(["hours", "returns/policy"]));

    // read one and the list
    let (s, b) = send(&w.e.app, get(&uri(&format!("/{id}")), Some(&w.km))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["supersedes"], json!(["hours", "returns/policy"]));
    let (s, b) = send(&w.e.app, get(&uri(""), Some(&w.km))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let listed: Vec<Value> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["supersedes"].clone())
        .collect();
    assert!(listed.contains(&json!(["hours", "returns/policy"])), "{b}");
    assert!(listed.contains(&json!([])), "{b}");

    // a save without supersedes (or with null) keeps the stored list
    sqlx::query("UPDATE source SET updated_by = 'someone-else' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let (s, b) = w
        .call("PUT", &format!("/{id}"), w.file_source("Returns"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["supersedes"], json!(["hours", "returns/policy"]), "{b}");
    let (keys, by) = w.stored(&id).await;
    assert_eq!(keys, ["hours", "returns/policy"]);
    assert_ne!(by, "someone-else", "a save stamps updated_by");
    let (s, b) = w
        .call("PUT", &format!("/{id}"), w.with("Returns", Value::Null))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["supersedes"], json!(["hours", "returns/policy"]), "{b}");

    // a new list replaces it
    let (s, b) = w
        .call(
            "PUT",
            &format!("/{id}"),
            w.with("Returns", json!(["opening-hours"])),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["supersedes"], json!(["opening-hours"]), "{b}");
    let words: Vec<Value> = w
        .audit("source.update", &id)
        .await
        .into_iter()
        .map(|d| d["supersedes"].clone())
        .collect();
    assert_eq!(
        words,
        [Value::Null, Value::Null, json!(["opening-hours"])],
        "a kept list is not named in the audit"
    );

    // [] clears it
    let (s, b) = w
        .call("PUT", &format!("/{id}"), w.with("Returns", json!([])))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["supersedes"], json!([]), "{b}");
    let (keys, _) = w.stored(&id).await;
    assert!(keys.is_empty());
}

/// Every rule is 422 `invalid_source` with a message naming it, on create (nothing stored) and on save (the stored
/// list kept); the limits themselves (32 keys, 128 characters) are accepted.
#[tokio::test]
async fn source_supersedes_rules_422_invalid_source() {
    let w = world().await;
    let over: Vec<String> = (0..33).map(|i| format!("k{i:02}")).collect();
    let cases = [
        (json!(over), "at most 32"),
        (json!(["k".repeat(129)]), "1 to 128"),
        (json!(["   "]), "1 to 128"),
        (json!(["opening\nhours"]), "[a-z0-9_./-]"),
        (json!(["a\u{7}b"]), "[a-z0-9_./-]"),
        (json!(["Opening-hours"]), "[a-z0-9_./-]"),
        (json!(["opening hours"]), "[a-z0-9_./-]"),
        (json!(["é"]), "[a-z0-9_./-]"),
        (json!(["hr:leave"]), "[a-z0-9_./-]"),
        (json!([42]), "string"),
        (json!("hours"), "list"),
        (json!({"topic": "hours"}), "list"),
    ];
    for (i, (bad, rule)) in cases.iter().enumerate() {
        let name = format!("Bad {i}");
        let (s, b) = w.call("POST", "", w.with(&name, bad.clone())).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(code(&b), "invalid_source", "{bad}: {b}");
        assert!(message(&b).contains(rule), "{bad}: {b}");
        assert_eq!(w.sources_named(&name).await, 0, "{bad}: nothing stored");
    }

    // the limits are allowed: 32 keys, 128 characters of strata's charset
    let max: Vec<String> = (0..32).map(|i| format!("k{i:02}")).collect();
    let (s, b) = w.call("POST", "", w.with("Max", json!(max))).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["supersedes"].as_array().unwrap().len(), 32);
    let long = format!("hr/{}", "k".repeat(125));
    let (s, b) = w.call("POST", "", w.with("Long", json!([long]))).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["id"].as_str().unwrap().to_string();

    // a bad save keeps the stored list
    let (s, b) = w
        .call("PUT", &format!("/{id}"), w.with("Long", json!(over)))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_source");
    let (keys, _) = w.stored(&id).await;
    assert_eq!(keys, [long]);

    // a secret-shaped topic key is refused like any secret in the body
    let (s, b) = w
        .call(
            "POST",
            "",
            w.with(
                "Leaky",
                json!(["sk-live-abcdefghijklmnopqrstuvwxyz0123456789"]),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "secret_rejected", "{b}");
    assert_eq!(w.sources_named("Leaky").await, 0);
}

// ---------------------------------------------------------------------------------------------------------------
// Uploads

/// An upload's `supersedes` is stored normalised in `knowledge_upload` and sent to strata_ingest (#15) as a
/// `supersedes` text field BEFORE the first `file` part; without the field the upload stores and sends `[]`.
#[tokio::test]
async fn upload_sends_supersedes_to_ingest_before_the_files() {
    let w = world().await;
    // the curator-side order does not matter: the field after the file still goes first to strata
    let (s, b) = w
        .upload(&[
            file("handbook.pdf"),
            supersedes_field(r#"[" hours ", "returns/policy", "hours"]"#),
        ])
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(w.uploads().await, [vec!["hours", "returns/policy"]]);
    let forms = w.forms();
    assert_eq!(forms.len(), 1, "{forms:?}");
    assert_eq!(
        supersedes_in(&forms[0]),
        (json!(["hours", "returns/policy"]), true),
        "{:?}",
        forms[0]
    );
    {
        let g = lock(&w.fake.ingest);
        let job = &g
            .jobs
            .iter()
            .rfind(|(t, _)| *t == w.tenant)
            .expect("the upload's ingest job")
            .1;
        assert_eq!(job["options"]["language"], "nl", "{job}");
        assert_eq!(
            job["meta"]["supersedes"],
            json!(["hours", "returns/policy"]),
            "strata_ingest took it: {job}"
        );
        assert_eq!(job["status"], "queued", "{job}");
    }
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'ingest.upload'",
    )
    .bind(w.tenant)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(audit[0]["supersedes"], json!(["hours", "returns/policy"]));

    // no field: [] stored and sent
    let (s, b) = w.upload(&[file("faq.pdf")]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let all = w.uploads().await;
    assert_eq!(all.len(), 2);
    assert!(all.contains(&Vec::new()), "{all:?}");
    let forms = w.forms();
    assert_eq!(
        supersedes_in(&forms[1]),
        (json!([]), true),
        "{:?}",
        forms[1]
    );
}

/// A bad `supersedes` field is 422 `invalid_supersedes` before strata_ingest is called; nothing is recorded.
#[tokio::test]
async fn upload_bad_supersedes_422_before_ingest() {
    let w = world().await;
    let over =
        serde_json::to_string(&(0..33).map(|i| format!("k{i}")).collect::<Vec<_>>()).unwrap();
    let long = format!("[\"{}\"]", "k".repeat(129));
    for (raw, rule) in [
        ("hours", "JSON array"),
        (r#"{"a": 1}"#, "list"),
        (over.as_str(), "at most 32"),
        (long.as_str(), "1 to 128"),
        (r#"["  "]"#, "1 to 128"),
        (r#"["a\tb"]"#, "[a-z0-9_./-]"),
        (r#"["Hours"]"#, "[a-z0-9_./-]"),
        (r#"["opening hours"]"#, "[a-z0-9_./-]"),
        ("[1]", "string"),
        (
            r#"["sk-live-abcdefghijklmnopqrstuvwxyz0123456789"]"#,
            "secret",
        ),
    ] {
        let (s, b) = w.upload(&[supersedes_field(raw), file("a.pdf")]).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{raw}: {b}");
        assert_eq!(code(&b), "invalid_supersedes", "{raw}: {b}");
        assert!(message(&b).contains(rule), "{raw}: {b}");
    }
    assert_eq!(w.ingest_posts(), 0, "refused before strata_ingest");
    assert!(w.uploads().await.is_empty());
}

// ---------------------------------------------------------------------------------------------------------------
// Source runs (T6b: strata_ingest #15)

/// A website run sends the source's `supersedes` top-level in its JSON body; after the list is cleared the next run
/// sends `[]` (an omitted field would keep what strata stored).
#[tokio::test]
async fn website_run_sends_supersedes_top_level_and_a_cleared_list_as_empty() {
    let w = world().await;
    let mut body = json!({"name": "Help", "type": "website",
        "config": {"urls": ["https://help.example.nl/returns"]}, "mapping": [], "target_store_id": w.leaf,
        "schedule": null, "enabled": true});
    body["supersedes"] = json!(["returns/policy", "hours"]);
    let (s, b) = w.call("POST", "", body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["id"].as_str().unwrap().to_string();

    let (s, run) = w.resync(&id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    let sent = w.ingest_json_bodies();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["supersedes"], json!(["hours", "returns/policy"]));
    assert_eq!(sent[0]["source_run"], run["id"], "{}", sent[0]);
    assert_eq!(
        sent[0]["options"].get("supersedes"),
        None,
        "top-level, not in options"
    );

    // finish the run so the next one may start, then clear the list
    {
        let mut g = lock(&w.fake.ingest);
        for (t, j) in g.jobs.iter_mut() {
            if *t == w.tenant {
                j["status"] = json!("completed");
            }
        }
    }
    body["supersedes"] = json!([]);
    let (s, b) = w.call("PUT", &format!("/{id}"), body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, run) = w.resync(&id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    let sent = w.ingest_json_bodies();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(
        sent[1]["supersedes"],
        json!([]),
        "a cleared list is sent as []"
    );
}

/// A file-source run sends `supersedes` as a text field before the first `file` part.
#[tokio::test]
async fn file_run_sends_supersedes_before_the_first_file() {
    let w = world().await;
    let (s, b) = w
        .call("POST", "", w.with("Manuals", json!(["hr/leave"])))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["id"].as_str().unwrap().to_string();
    let (s, run) = w.resync_files(&id, &[file("leave.pdf")]).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    let forms = w.forms();
    assert_eq!(forms.len(), 1, "{forms:?}");
    assert_eq!(
        supersedes_in(&forms[0]),
        (json!(["hr/leave"]), true),
        "{:?}",
        forms[0]
    );
    let names: Vec<&str> = forms[0].iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "store_id",
            "source_id",
            "source_run",
            "options",
            "supersedes",
            "file"
        ]
    );
}
