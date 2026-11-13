//! KH T10: live acceptance of the knowledge hierarchy (D106–D115) against a deployed `agent` — **ignored by default**,
//! run by an operator after the KH rollout (plan `2026-10-21-kh-knowledge-hierarchy.md` §4). Nothing here starts a
//! container or touches a test database: every call goes over HTTPS to the live api / edge, plus ONE read-only query
//! against the agent database (a citation's `pack_layer` is never served by an API, K50: it is kept only on the
//! reply's `answer_grounding.sources` row).
//!
//! The checklist (T10 row):
//! 1. [`live_compile_shows_the_layer_chain`] — the job's current release froze a selection with an industry and its
//!    departments, and pins every layer of the chain (`base` → `industry/<i>` → each `industry_dept/<i>/<d>` → `job/…`).
//! 2. [`live_web_chat_answer_cites_layer_and_pack_and_traces_markers`] — a customer web chat message on
//!    `chat.nonagentic.ai` is answered by the runtime (D109); the case's `qualified` event carries the prompt's layer
//!    markers (`[layer <key>@<version>]`, D108) in fold order with the release's pinned versions; the reply's grounding
//!    cites a platform pack with the template layer that references it (`pack_layer`, R14).
//! 3. [`live_readiness_blocks_without_required_knowledge`] — a job whose draft lacks a required knowledge kind lists a
//!    blocking `knowledge_unbound` item (D110). Read only: nothing is approved (an approve that did NOT block would cut
//!    and apply a production release).
//!
//! Run: `cargo test -p agent-api --test all kh::acceptance:: -- --ignored --nocapture` with
//!
//! | env | required | meaning |
//! |---|---|---|
//! | `KH_LIVE_TOKEN` | yes | a Keycloak access token of a member of the tenant (any role reads these routes) |
//! | `KH_LIVE_TENANT_ID` | yes | the owner's test tenant (sent as `X-Active-Tenant-Id`) |
//! | `KH_LIVE_JOB` | yes (1, 2) | key of a live job (current release with a compiling runtime design) bound to the widget |
//! | `KH_LIVE_WIDGET_KEY` | yes (2) | the job's web chat widget key (`wk_…`), Turnstile off on its binding |
//! | `KH_LIVE_DATABASE_URL` | yes (2) | a role that may `SELECT` `answer_grounding` + `conversation_message` (RLS: the test sets `app.current_tenant`) |
//! | `KH_LIVE_UNREADY_JOB` | yes (3) | key of a job of the tenant whose draft has a required kind with no knowledge bound |
//! | `KH_LIVE_UNREADY_SUBJECT` | no | the expected blocking subject (`<department>:<stage>\|<kind>`) |
//! | `KH_LIVE_API_URL` | no | default `https://agent.nonagentic.ai` |
//! | `KH_LIVE_EDGE_URL` | no | default `https://chat.nonagentic.ai` |
//! | `KH_LIVE_ORIGIN` | no | the `Origin` the widget calls from; default the edge URL (always allowed) |
//! | `KH_LIVE_QUESTION` | no | a question the job's platform packs answer; default a retail returns question |
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

const DEFAULT_API: &str = "https://agent.nonagentic.ai";
const DEFAULT_EDGE: &str = "https://chat.nonagentic.ai";
const DEFAULT_QUESTION: &str = "How many days do I have to return an item?";

fn var(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| {
            panic!("{name} is not set: see the env table in tests/kh/acceptance.rs (KH T10 live acceptance)")
        })
}

fn var_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The deployed system, as the operator's env names it.
struct Live {
    api: String,
    edge: String,
    origin: String,
    token: String,
    tenant: Uuid,
    http: reqwest::Client,
}

impl Live {
    fn from_env() -> Self {
        let edge = var_or("KH_LIVE_EDGE_URL", DEFAULT_EDGE)
            .trim_end_matches('/')
            .to_string();
        Self {
            api: var_or("KH_LIVE_API_URL", DEFAULT_API)
                .trim_end_matches('/')
                .to_string(),
            origin: var_or("KH_LIVE_ORIGIN", &edge),
            edge,
            token: var("KH_LIVE_TOKEN"),
            tenant: Uuid::parse_str(&var("KH_LIVE_TENANT_ID"))
                .expect("KH_LIVE_TENANT_ID is a uuid"),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("an HTTP client"),
        }
    }

    /// A staff read on the api: `(status, body)` (a body that is not JSON reads as its text).
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(format!("{}{path}", self.api))
            .bearer_auth(&self.token)
            .header("x-active-tenant-id", self.tenant.to_string())
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"));
        read(r).await
    }

    /// A widget call on the edge: `(status, body)`.
    async fn widget(&self, path: &str, bearer: Option<&str>, body: Value) -> (u16, Value) {
        let mut rq = self
            .http
            .post(format!("{}{path}", self.edge))
            .header("origin", &self.origin)
            .json(&body);
        if let Some(b) = bearer {
            rq = rq.bearer_auth(b);
        }
        let r = rq
            .send()
            .await
            .unwrap_or_else(|e| panic!("POST {path}: {e}"));
        read(r).await
    }

    /// The job's current release: `(release id, frozen selection, pinned layer → version)`.
    async fn current_release(&self, job: &str) -> (String, Value, Vec<(String, i64)>) {
        let (s, list) = self.get(&format!("/api/jobs/{job}/releases")).await;
        assert_eq!(s, 200, "release list of {job}: {list}");
        let id = list["items"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["state"] == "current"))
            .and_then(|r| r["id"].as_str())
            .unwrap_or_else(|| {
                panic!("{job} has no current release (D109: no live release → 409 job_not_live): {list}")
            })
            .to_string();
        let (s, d) = self.get(&format!("/api/jobs/{job}/releases/{id}")).await;
        assert_eq!(s, 200, "release {id} of {job}: {d}");
        let pins: Vec<(String, i64)> = d["provenance"]["pins"]
            .as_array()
            .unwrap_or_else(|| panic!("release {id} has no provenance.pins: {d}"))
            .iter()
            .map(|p| {
                (
                    p["layer"].as_str().expect("pin layer").to_string(),
                    p["version"].as_i64().expect("pin version"),
                )
            })
            .collect();
        (id, d["selection"].clone(), pins)
    }
}

async fn read(r: reqwest::Response) -> (u16, Value) {
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

/// `(industry, departments)` of a frozen selection, checked against D106/D115: one industry, ≥ 1 department, every
/// department a layer of that industry.
fn hierarchy(selection: &Value) -> (String, Vec<String>) {
    let industry = selection["industry"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !industry.is_empty(),
        "D106: the release froze a selection without an industry: {selection}"
    );
    let departments: Vec<String> = selection["departments"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !departments.is_empty(),
        "D115: the release froze a selection without departments: {selection}"
    );
    for d in &departments {
        assert!(
            d.starts_with(&format!("industry_dept/{industry}/")),
            "D115: department {d} is not of industry {industry}"
        );
    }
    (industry, departments)
}

/// The tag family a layer key is pinned by: a region sub-layer (`…/region/<r>`, D111) joins its parent's family.
fn family_of(key: &str) -> &str {
    key.split_once("/region/").map_or(key, |(parent, _)| parent)
}

/// THE fold order (D112/D115, `agent_model::compile::FoldPosition`) as a rank of a template layer key: base, base
/// region, profile, industry, industry region, then each department in declared order followed by its region, then
/// the job and its dimension layers. `None` for a key that is no layer of this selection.
fn fold_rank(key: &str, industry: &str, departments: &[String]) -> Option<usize> {
    let region = key.contains("/region/");
    let head = family_of(key);
    if head == "base" {
        return Some(usize::from(region));
    }
    if head.starts_with("profile/") {
        return Some(2);
    }
    if head == format!("industry/{industry}") {
        return Some(3 + usize::from(region));
    }
    if let Some(i) = departments.iter().position(|d| d == head) {
        return Some(10 + 2 * i + usize::from(region));
    }
    head.starts_with("job/").then_some(1000)
}

/// `[layer <key>@<version>]` → `(key, version)`.
fn marker(m: &str) -> Option<(&str, &str)> {
    m.strip_prefix("[layer ")?
        .strip_suffix(']')?
        .rsplit_once('@')
}

#[tokio::test]
#[ignore = "live: needs a deployed agent and the KH_LIVE_* env (module docs)"]
async fn live_compile_shows_the_layer_chain() {
    let live = Live::from_env();
    let job = var("KH_LIVE_JOB");
    let (id, selection, pins) = live.current_release(&job).await;
    let (industry, departments) = hierarchy(&selection);
    let pinned = |layer: &str| pins.iter().any(|(l, _)| l == layer);
    let mut chain = vec!["base".to_string(), format!("industry/{industry}")];
    chain.extend(departments.iter().cloned());
    for layer in &chain {
        assert!(
            pinned(layer.as_str()),
            "release {id} does not pin {layer}: {pins:?}"
        );
    }
    let jobs: Vec<&(String, i64)> = pins.iter().filter(|(l, _)| l.starts_with("job/")).collect();
    assert_eq!(jobs.len(), 1, "release {id} pins one job layer: {pins:?}");
    chain.push(jobs[0].0.clone());
    for (l, v) in &pins {
        assert!(*v >= 1, "pin {l}@{v} of release {id}");
    }
    eprintln!(
        "KH T10 compile: job {job} release {id} chain {}",
        chain
            .iter()
            .map(|l| {
                let v = pins.iter().find(|(p, _)| p == l).map_or(0, |(_, v)| *v);
                format!("{l}@{v}")
            })
            .collect::<Vec<_>>()
            .join(" → ")
    );
}

#[tokio::test]
#[ignore = "live: needs a deployed agent and the KH_LIVE_* env (module docs)"]
async fn live_web_chat_answer_cites_layer_and_pack_and_traces_markers() {
    let live = Live::from_env();
    let job = var("KH_LIVE_JOB");
    let widget = var("KH_LIVE_WIDGET_KEY");
    let db_url = var("KH_LIVE_DATABASE_URL");
    let (release, selection, pins) = live.current_release(&job).await;
    let (industry, departments) = hierarchy(&selection);

    // 1. a customer: a fresh web chat session and one message (the reference finds its case again)
    let (s, sess) = live
        .widget("/api/webchat/sessions", None, json!({"widget_key": widget}))
        .await;
    assert_eq!(
        s, 201,
        "web chat session (428 turnstile_required = switch Turnstile off on the binding): {sess}"
    );
    let session = sess["session_token"]
        .as_str()
        .expect("session_token")
        .to_string();
    let reference = format!("kh-acc-{}", &Uuid::now_v7().simple().to_string()[..12]);
    let question = format!(
        "{} ({reference})",
        var_or("KH_LIVE_QUESTION", DEFAULT_QUESTION)
    );
    let visitor = format!("kh-t10-{}", &Uuid::new_v4().simple().to_string()[..16]);
    let (s, answer) = live
        .widget(
            "/api/webchat/messages",
            Some(&session),
            json!({
                "widget_key": widget,
                "visitor_id": visitor,
                "message_id": format!("m-{reference}"),
                "messages": [{"role": "user", "text": question}],
            }),
        )
        .await;
    assert_eq!(
        s, 200,
        "web chat message (409 job_not_live = no live release with a runtime design): {answer}"
    );
    assert_eq!(
        answer["status"], "answered",
        "the runtime answered (queued = handed over: the answer was not grounded): {answer}"
    );
    let text = answer["text"].as_str().unwrap_or_default();
    assert!(!text.trim().is_empty(), "an answer text: {answer}");
    eprintln!("KH T10 web chat: {question:?} → {text:?}");

    // 2. its case, as staff read it (newest first; the title is the customer's first message)
    let mut case = Value::Null;
    for _ in 0..20 {
        let (s, page) = live
            .get(&format!(
                "/api/jobs/{job}/cases?env=prod&q={reference}&limit=5"
            ))
            .await;
        assert_eq!(s, 200, "cases of {job}: {page}");
        if let Some(c) = page["items"].as_array().and_then(|a| a.first()) {
            case = c.clone();
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let case_id = case["case_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no prod case titled with {reference} on {job}"))
        .to_string();
    assert_eq!(case["channel"], "web_chat", "{case}");
    let (s, detail) = live.get(&format!("/api/jobs/{job}/cases/{case_id}")).await;
    assert_eq!(s, 200, "case {case_id}: {detail}");

    // 3. the trace markers: the qualified event's layers, in fold order, at the release's pinned versions (D108)
    let qualified = detail["timeline"]
        .as_array()
        .and_then(|t| t.iter().find(|e| e["kind"] == "qualified"))
        .unwrap_or_else(|| panic!("case {case_id} has no qualified event: {detail}"))
        .clone();
    assert_eq!(
        qualified["release_id"], release,
        "the turn ran on the current release: {qualified}"
    );
    assert!(
        qualified["prompt_warning"].is_null(),
        "the prompt had template layer sections: {qualified}"
    );
    let markers: Vec<String> = qualified["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut last = 0;
    let mut template = 0;
    for m in &markers {
        let (key, version) = marker(m).unwrap_or_else(|| panic!("not a layer marker: {m}"));
        if key == "tenant" {
            assert_eq!(
                version, release,
                "the tenant section is the release's refinements: {m}"
            );
            continue;
        }
        let rank = fold_rank(key, &industry, &departments)
            .unwrap_or_else(|| panic!("marker {m} names no layer of the selection {selection}"));
        assert!(
            rank >= last,
            "markers out of fold order at {m}: {markers:?}"
        );
        last = rank;
        template += 1;
        if let Some((_, v)) = pins.iter().find(|(l, _)| l == family_of(key)) {
            assert_eq!(
                version,
                v.to_string(),
                "marker {m} is not the pinned version of {}",
                family_of(key)
            );
        }
    }
    assert!(template > 0, "no template layer marker: {markers:?}");
    assert!(
        markers
            .first()
            .and_then(|m| marker(m))
            .is_some_and(|(k, _)| k == "base"),
        "the prompt opens with the base layer: {markers:?}"
    );
    eprintln!("KH T10 markers: {markers:?}");

    // 4. the citation: the reply's grounding names a platform pack and the layer that references it (R14)
    let pool = sqlx::PgPool::connect(&db_url)
        .await
        .expect("KH_LIVE_DATABASE_URL connects");
    let mut tx = pool.begin().await.expect("a read transaction");
    sqlx::query("SELECT set_config('app.current_tenant', $1, true)")
        .bind(live.tenant.to_string())
        .execute(&mut *tx)
        .await
        .expect("set app.current_tenant");
    let row: Option<(Value, String)> = sqlx::query_as(
        "SELECT g.sources, g.grounding FROM answer_grounding g \
           JOIN conversation_message m \
             ON m.tenant_id = g.tenant_id AND m.job_id = g.job_id AND m.message_id = g.message_id \
          WHERE m.case_id = $1 AND m.role = 'agent' \
          ORDER BY m.at_ms DESC, m.message_id DESC LIMIT 1",
    )
    .bind(&case_id)
    .fetch_optional(&mut *tx)
    .await
    .expect("read answer_grounding");
    tx.rollback().await.expect("end the read");
    let (sources, grounding) =
        row.unwrap_or_else(|| panic!("case {case_id}: no answer_grounding row for its reply"));
    assert_eq!(grounding, "grounded", "the reply is grounded: {sources}");
    let platform: Vec<&Value> = sources
        .as_array()
        .map(|a| a.iter().filter(|s| s["layer"] == "platform").collect())
        .unwrap_or_default();
    assert!(
        !platform.is_empty(),
        "the answer cites no platform pack (only tenant knowledge?): pick a KH_LIVE_QUESTION the packs answer: {sources}"
    );
    for src in &platform {
        let pack = src["pack"].as_str().unwrap_or_default();
        assert!(
            pack.contains('@'),
            "a platform citation names its pack key@version: {src}"
        );
        let layer = src["pack_layer"]
            .as_str()
            .unwrap_or_else(|| panic!("R14: a platform citation names its pack_layer: {src}"));
        assert!(
            pins.iter().any(|(l, _)| l == family_of(layer)),
            "pack_layer {layer} is no layer the release pins: {pins:?}"
        );
        eprintln!("KH T10 citation: {layer} {pack} · {}", src["title"]);
    }
}

#[tokio::test]
#[ignore = "live: needs a deployed agent and the KH_LIVE_* env (module docs)"]
async fn live_readiness_blocks_without_required_knowledge() {
    let live = Live::from_env();
    let job = var("KH_LIVE_UNREADY_JOB");
    let (s, r) = live
        .get(&format!("/api/jobs/{job}/release-readiness?of=draft"))
        .await;
    assert_eq!(s, 200, "readiness of {job}: {r}");
    let unbound: Vec<&Value> = r["blocking"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|i| i["code"] == "knowledge_unbound")
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !unbound.is_empty(),
        "D110: a required kind without knowledge blocks the draft of {job}: {r}"
    );
    for i in &unbound {
        let subject = i["subject"].as_str().unwrap_or_default();
        // `<department>:<stage>|<kind>` (D115: per department; `job` when the selection has none)
        let (scope, kind) = subject
            .split_once(':')
            .unwrap_or_else(|| panic!("a knowledge_unbound subject names its department: {i}"));
        assert!(scope.starts_with("industry_dept/") || scope == "job", "{i}");
        assert!(kind.contains('|'), "{i}");
    }
    if let Ok(want) = std::env::var("KH_LIVE_UNREADY_SUBJECT") {
        assert!(
            unbound.iter().any(|i| i["subject"] == want.as_str()),
            "no knowledge_unbound for {want}: {r}"
        );
    }
    eprintln!(
        "KH T10 readiness: {job} blocked by {:?}",
        unbound.iter().map(|i| &i["subject"]).collect::<Vec<_>>()
    );
}
