//! B13 party identity (crate::party) and the B12 fixes on the item / console views: a new subject gets one anonymous
//! party (concurrent first contact included); inbound puts the party on the conversation, the case and the handover
//! item; linkCaseParty merges an anonymous party into the target, which then answers for the merged subject; the
//! party routes' roles; the item carries `csat` after a rating; staff names fall back to the email; the handover
//! notice the customer saw is in the transcript. Real Postgres 18; the edge and the backend are fakes.
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};
use uuid::Uuid;

/// Job `job`'s customer-side Telegram binding (team `team`, verified), created on first use. Telegram, not web chat: a
/// web chat subject is its session (`wc:<session_id>`, one session = one conversation), so only a channel with a
/// stable wire subject can bring the same customer back on a new conversation.
async fn telegram_on(c: &Ctx, job: Uuid, team: &str) -> Uuid {
    sqlx::query(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, created_by, verified_at) \
         VALUES ($1, $2, 'telegram', 'customer', $3, 'Telegram', 'test', now()) \
         ON CONFLICT ON CONSTRAINT connector_bindings_account DO NOTHING",
    )
    .bind(c.t)
    .bind(job)
    .bind(team)
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query_scalar(
        "SELECT id FROM connector_bindings WHERE kind = 'telegram' AND side = 'customer' AND external_id = $1",
    )
    .bind(team)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

/// One customer message from subject `party` on Telegram conversation `conv` of job `job_key` (`job`, team `team`).
#[allow(clippy::too_many_arguments)]
async fn inbound_in(
    c: &Ctx,
    job_key: &str,
    job: Uuid,
    team: &str,
    conv: &str,
    msg: &str,
    party: &str,
    text: &str,
) -> (StatusCode, Value) {
    let binding = telegram_on(c, job, team).await;
    c.internal(
        "/api/internal/channel/inbound",
        json!({
            "kind": "telegram", "external_id": team, "binding_id": binding,
            "tenant": c.t, "job_id": job,
            "job_key": job_key, "conversation": {"channel": "telegram", "id": conv, "route": {}},
            "message_id": msg, "party": {"id": party, "name": "Femke", "verified": false},
            "text": text,
        }),
    )
    .await
}

/// One customer message from subject `party` on Telegram conversation `conv` of j1 (team `T-test`).
async fn inbound_as(
    c: &Ctx,
    conv: &str,
    msg: &str,
    party: &str,
    text: &str,
) -> (StatusCode, Value) {
    inbound_in(c, "j1", c.job, "T-test", conv, msg, party, text).await
}

/// A second job `j2` of the tenant.
async fn job2(c: &Ctx) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j2', 'J2', '[]', 'test') RETURNING id",
    )
    .bind(c.t)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

/// The `job` keys of a party's `cases`.
fn case_jobs(p: &Value) -> Vec<String> {
    p["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("no cases: {p}"))
        .iter()
        .map(|c| c["job"].as_str().unwrap().to_string())
        .collect()
}

/// The party of the identity (telegram, `subject`).
async fn party_of(c: &Ctx, subject: &str) -> Uuid {
    sqlx::query_scalar(
        "SELECT party_id FROM party_identity WHERE channel = 'telegram' AND subject = $1",
    )
    .bind(subject)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

async fn count(c: &Ctx, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(&c.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn new_subject_creates_anonymous_party_once() {
    let c = conv::setup().await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = c.e.state.db.clone();
        let t = c.t;
        tasks.push(tokio::spawn(async move {
            let mut tx = agent_api::tenancy::begin_scoped(&db, t).await.unwrap();
            let id =
                agent_api::party::resolve(&mut tx, t, "web_chat", "visitor-7", Some("Ann"), false)
                    .await
                    .unwrap();
            tx.commit().await.unwrap();
            id
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "every caller got the same party: {ids:?}");
    assert_eq!(count(&c, "SELECT count(*) FROM party").await, 1);
    assert_eq!(count(&c, "SELECT count(*) FROM party_identity").await, 1);
    let name: Option<String> = sqlx::query_scalar("SELECT display_name FROM party")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(name.as_deref(), Some("Ann"));
    // the same subject on another channel is another identity (and party)
    let mut tx = agent_api::tenancy::begin_scoped(&c.e.state.db, c.t)
        .await
        .unwrap();
    let other = agent_api::party::resolve(&mut tx, c.t, "telegram", "visitor-7", None, false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_ne!(other, ids[0]);
}

#[tokio::test]
async fn inbound_sets_party_on_conversation_case_and_item() {
    let c = conv::setup().await;
    let (s, b) = inbound_as(&c, "sl-1", "m1", "cust-1", "Can I speak to a real person?").await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("queued")),
        "{b}"
    );
    let p = party_of(&c, "cust-1").await;
    let on: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT party_id FROM conversation UNION ALL SELECT party_id FROM agent_case \
         UNION ALL SELECT party_id FROM agent_handover_items",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(on, [Some(p), Some(p), Some(p)]);
    // the item and the console row carry the party
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let party = &q["items"][0]["party"];
    assert_eq!(party["id"], p.to_string(), "{q}");
    assert_eq!(party["known"], false);
    assert_eq!(party["display_name"], "Femke");
    assert!(party["subject"].is_null(), "{party}");
    let (_, l) = c.get("/api/jobs/j1/console/conversations", "u-obs").await;
    assert_eq!(l["items"][0]["party"]["id"], p.to_string(), "{l}");
    // caseParty: any member
    let (s, b) = c.get("/api/jobs/j1/cases/CS-1/party", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["case_id"], "CS-1");
    assert_eq!(b["party"]["id"], p.to_string());
    assert_eq!(b["party"]["identities"][0]["subject"], "cust-1");
    assert_eq!(b["party"]["cases"][0]["case_id"], "CS-1");
    let (s, b) = c.get("/api/jobs/j1/cases/CS-9/party", "u-obs").await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "case_not_found"));
    // a second message from the same subject: no new party
    let (_, _) = inbound_as(&c, "sl-1", "m2", "cust-1", "Hello?").await;
    assert_eq!(count(&c, "SELECT count(*) FROM party").await, 1);
}

/// cust-1 on sl-1 (CS-1, party A) and cust-2 on sl-2 (CS-2, party B), both queued for a person (D109: the agent's
/// answers are the runtime's, and job `j1` has no live release here).
async fn two_parties(c: &Ctx) -> (Uuid, Uuid) {
    let (_, b) = inbound_as(c, "sl-1", "m1", "cust-1", "Can I speak to a real person?").await;
    assert_eq!(b["case_id"], "CS-1", "{b}");
    let (_, b) = inbound_as(c, "sl-2", "m2", "cust-2", "Can I speak to a real person?").await;
    assert_eq!(b["case_id"], "CS-2", "{b}");
    (party_of(c, "cust-1").await, party_of(c, "cust-2").await)
}

#[tokio::test]
async fn link_case_party_merges_anonymous() {
    let c = conv::setup().await;
    let (a, b_party) = two_parties(&c).await;
    assert_ne!(a, b_party);
    let url = "/api/jobs/j1/cases/CS-1/party";
    let (s, b) = c
        .call("POST", url, "u-obs", json!({"party_id": b_party}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"));
    let (s, b) = c
        .call("POST", url, "u-agent", json!({"party_id": Uuid::now_v7()}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "party_not_found"));
    let (s, b) = c
        .call("POST", url, "u-agent", json!({"party_id": b_party}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["party"]["id"], b_party.to_string());
    let subjects: Vec<&str> = b["party"]["identities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["cust-1", "cust-2"], "{b}");
    // A answers as B; every row of A is B's
    let (s, pa) = c.get(&format!("/api/parties/{a}"), "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{pa}");
    assert_eq!(pa["merged_into"], b_party.to_string());
    assert_eq!(party_of(&c, "cust-1").await, b_party);
    assert_eq!(
        count(
            &c,
            &format!("SELECT count(*) FROM conversation WHERE party_id = '{b_party}'")
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &c,
            &format!("SELECT count(*) FROM agent_case WHERE party_id = '{b_party}'")
        )
        .await,
        2
    );
    let details: Value =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'case.party_link'")
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(details["party_id"], b_party.to_string());
    assert_eq!(details["from_party"], a.to_string());
    assert_eq!(details["merged"], true);
    // linking to the merged party is refused
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/cases/CS-2/party",
            "u-agent",
            json!({"party_id": a}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "party_merged"));
}

#[tokio::test]
async fn merged_party_answers_target() {
    let c = conv::setup().await;
    let (a, b_party) = two_parties(&c).await;
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/cases/CS-1/party",
            "u-agent",
            json!({"party_id": b_party}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the merged subject writes again, on a new conversation: B, not A, and no new party
    let (_, b) = inbound_as(
        &c,
        "sl-3",
        "m3",
        "cust-1",
        "Me again: can I speak to a real person?",
    )
    .await;
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    let p: Option<Uuid> =
        sqlx::query_scalar("SELECT party_id FROM conversation WHERE conversation_id = $1")
            .bind(&conv_id)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(p, Some(b_party));
    assert_eq!(count(&c, "SELECT count(*) FROM party").await, 2);
    // resolve follows merged_into even for an identity still pointing at the merged party
    sqlx::query(
        "UPDATE party_identity SET party_id = $1 WHERE channel = 'telegram' AND subject = 'cust-1'",
    )
    .bind(a)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let mut tx = agent_api::tenancy::begin_scoped(&c.e.state.db, c.t)
        .await
        .unwrap();
    let got = agent_api::party::resolve(&mut tx, c.t, "telegram", "cust-1", None, false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got, b_party);
}

#[tokio::test]
async fn parties_requires_support_agent() {
    let c = conv::setup().await;
    two_parties(&c).await;
    for sub in ["u-obs", "u-int"] {
        let (s, b) = c.get("/api/parties", sub).await;
        assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{sub}");
        let (s, b) = c
            .get(&format!("/api/parties/{}", Uuid::now_v7()), sub)
            .await;
        assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{sub}");
    }
    let (s, b) = c.get("/api/parties", "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 2);
    assert_eq!(b["next"], Value::Null);
    let (_, b) = c.get("/api/parties?q=CUST-2", "u-agent").await;
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{b}");
    assert_eq!(items[0]["identities"][0]["subject"], "cust-2");
    let (_, b) = c.get("/api/parties?q=zzz", "u-agent").await;
    assert_eq!(b["items"], json!([]));
    let (s, b) = c.get("/api/parties?before=nope", "u-agent").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_cursor")
    );
    let (s, b) = c
        .get(&format!("/api/parties/{}", Uuid::now_v7()), "u-agent")
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "party_not_found"));
}

#[tokio::test]
async fn handover_item_carries_csat_after_rating() {
    let c = conv::setup().await;
    let (_, b) = c.inbound("wc-1", "m1", "talk to someone please").await;
    let item = b["item_id"].as_str().unwrap().to_string();
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    c.take(&conv_id, "u-agent", "queue").await;
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let h = c.item(&item).await;
    assert_eq!(h["csat"], Value::Null, "asked, not answered: {h}");
    let (_, b) = c.inbound("wc-1", "m-rate", "4").await;
    assert_eq!(b["status"], "csat_recorded", "{b}");
    let (_, q) = c.get("/api/jobs/j1/handover?state=resolved", "u-obs").await;
    let csat = &q["items"][0]["csat"];
    assert_eq!(csat["rating"], 4, "{q}");
    assert!(csat["recorded_at"].is_string(), "{csat}");
    let (_, l) = c.get("/api/jobs/j1/console/conversations", "u-obs").await;
    assert_eq!(l["items"][0]["csat"]["rating"], 4, "{l}");
}

#[tokio::test]
async fn staff_name_falls_back_to_email() {
    let c = conv::setup().await;
    sqlx::query("UPDATE users SET display_name = '' WHERE sub = 'u-agent'")
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (_, b) = c
        .inbound("wc-1", "m1", "Can I speak to a real person?")
        .await;
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent");
    assert_eq!(b["name"], "agent@alpha.example", "{b}");
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let it = &q["items"][0];
    assert_eq!(it["assignee"], "u-agent");
    assert_eq!(it["assignee_name"], "agent@alpha.example", "{it}");
    let last = it["history"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["actor_name"], "agent@alpha.example");
    let a = q["assignees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["sub"] == "u-agent")
        .unwrap()
        .clone();
    assert_eq!(a["name"], "agent@alpha.example");
}

#[tokio::test]
async fn handover_notice_is_in_transcript() {
    let c = conv::setup().await;
    let (_, b) = c
        .inbound("wc-1", "m1", "Can I speak to a real person?")
        .await;
    assert_eq!(b["notice"], true, "{b}");
    let id = b["conversation_id"].as_str().unwrap().to_string();
    let want = "I've passed this conversation to a colleague, who will reply here.";
    let (s, t) = c
        .get(&format!("/api/jobs/j1/conversations/{id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let notices: Vec<&Value> = t["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["author"] == "handover_notice")
        .collect();
    assert_eq!(notices.len(), 1, "{t}");
    assert_eq!(
        notices[0]["role"], "agent",
        "customer-visible, not a staff-only system note"
    );
    assert_eq!(notices[0]["text"], want);
    // the handover drawer's transcript too
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    assert!(
        q["items"][0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["from"] == "agent" && m["text"] == want),
        "{q}"
    );
    // a message while queued sends no notice and stores none
    c.inbound("wc-1", "m2", "Hello?").await;
    assert_eq!(
        count(
            &c,
            "SELECT count(*) FROM conversation_message WHERE author = 'handover_notice'"
        )
        .await,
        1
    );
}

/// cust-1 has CS-1 in j1 (sl-1) and CS-1 in j2 (sl-9), each queued for a person: one party, cases in both jobs.
async fn one_party_two_jobs(c: &Ctx) -> (Uuid, Uuid) {
    let j2 = job2(c).await;
    let (_, b) = inbound_as(c, "sl-1", "m1", "cust-1", "Can I speak to a real person?").await;
    assert_eq!(b["case_id"], "CS-1", "{b}");
    let (_, b) = inbound_in(
        c,
        "j2",
        j2,
        "T-two",
        "sl-9",
        "m9",
        "cust-1",
        "Can I speak to a real person?",
    )
    .await;
    assert_eq!(b["case_id"], "CS-1", "{b}");
    (party_of(c, "cust-1").await, j2)
}

#[tokio::test]
async fn case_party_lists_only_this_jobs_cases() {
    let c = conv::setup().await;
    let (p, _) = one_party_two_jobs(&c).await;
    // a job member sees the party's cases of the path job only
    for job in ["j1", "j2"] {
        let (s, b) = c
            .get(&format!("/api/jobs/{job}/cases/CS-1/party"), "u-obs")
            .await;
        assert_eq!(s, StatusCode::OK, "{job}: {b}");
        assert_eq!(b["party"]["id"], p.to_string());
        assert_eq!(case_jobs(&b["party"]), [job], "{job}: {b}");
    }
    // the tenant-level party view (support_agent) keeps every job's cases
    let (s, b) = c.get(&format!("/api/parties/{p}"), "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut jobs = case_jobs(&b);
    jobs.sort();
    assert_eq!(jobs, ["j1", "j2"], "{b}");
}

#[tokio::test]
async fn link_does_not_touch_other_jobs() {
    let c = conv::setup().await;
    let (a, j2) = one_party_two_jobs(&c).await;
    let (_, b) = inbound_as(&c, "sl-2", "m2", "cust-2", "Can I speak to a real person?").await;
    assert_eq!(b["case_id"], "CS-2", "{b}");
    let target = party_of(&c, "cust-2").await;
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/cases/CS-1/party",
            "u-agent",
            json!({"party_id": target}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["party"]["id"], target.to_string());
    assert_eq!(
        case_jobs(&b["party"]),
        ["j1", "j1"],
        "the answer lists j1's cases only: {b}"
    );
    // j1's rows of A moved to the target
    for table in ["conversation", "agent_case"] {
        assert_eq!(
            count(
                &c,
                &format!(
                    "SELECT count(*) FROM {table} WHERE party_id = '{a}' AND job_id = '{}'",
                    c.job
                )
            )
            .await,
            0,
            "{table}"
        );
    }
    // j2's rows still name A, so A is not merged and keeps its identity
    for table in ["conversation", "agent_case"] {
        assert_eq!(
            count(
                &c,
                &format!("SELECT count(*) FROM {table} WHERE party_id = '{a}' AND job_id = '{j2}'")
            )
            .await,
            1,
            "{table}"
        );
    }
    let (_, pa) = c.get(&format!("/api/parties/{a}"), "u-agent").await;
    assert_eq!(pa["merged_into"], Value::Null, "{pa}");
    assert_eq!(party_of(&c, "cust-1").await, a);
    let (_, b2) = c.get("/api/jobs/j2/cases/CS-1/party", "u-obs").await;
    assert_eq!(b2["party"]["id"], a.to_string(), "{b2}");
    let details: Value =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'case.party_link'")
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(details["merged"], false, "{details}");
}

#[tokio::test]
async fn notice_is_not_in_model_history() {
    let c = conv::setup().await;
    let (_, b) = c
        .inbound("wc-1", "m1", "Can I speak to a real person?")
        .await;
    assert_eq!(b["notice"], true, "{b}");
    let id = b["conversation_id"].as_str().unwrap().to_string();
    assert_eq!(
        count(
            &c,
            "SELECT count(*) FROM conversation_message WHERE author = 'handover_notice'"
        )
        .await,
        1,
        "the notice is stored (the transcript shows it)"
    );
    let mut tx = agent_api::tenancy::begin_scoped(&c.e.state.db, c.t)
        .await
        .unwrap();
    let history = agent_api::conversations::store::history(
        &mut tx,
        &id,
        agent_api::conversations::store::HISTORY_TURNS,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(history.iter().any(|m| m.role == "customer"), "{history:?}");
    assert!(
        history
            .iter()
            .all(|m| m.author != agent_api::conversations::handover::NOTICE_AUTHOR),
        "the edge's notice is not the model's prior answer: {history:?}"
    );
}

/// A party whose conversation has an identified customer (state `subject` + a sourcing `identity_source`) is not
/// merged into the target by a link (only its case moves); one without is merged. Replaces the dropped `party.kind`.
#[tokio::test]
async fn link_merges_only_a_party_with_no_identified_customer() {
    let c = conv::setup().await;
    let (a, b_party) = two_parties(&c).await;
    // cust-1's conversation is identified: its party (A) is "known"
    let conv_a: String =
        sqlx::query_scalar("SELECT conversation_id FROM conversation WHERE party_id = $1")
            .bind(a)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    common::state::identify(
        &c.e.owner,
        c.t,
        c.job,
        &conv_a,
        "C-1",
        Some("a@shop.example"),
    )
    .await;
    let (s, v) = c.get(&format!("/api/parties/{a}"), "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["known"].clone(), v["subject"].clone()),
        (json!(true), json!("C-1"))
    );
    let url = "/api/jobs/j1/cases/CS-1/party";
    let (s, b) = c
        .call("POST", url, "u-agent", json!({"party_id": b_party}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, pa) = c.get(&format!("/api/parties/{a}"), "u-agent").await;
    assert!(
        pa["merged_into"].is_null(),
        "an identified party is not merged: {pa}"
    );
    assert_eq!(b["party"]["id"], b_party.to_string());
}

/// Link customer takes the conversation's control lock and owner row before the case row, like Hand back and Resolve
/// (SW I2): racing them on one case must never deadlock (Postgres 40P01 -> 500), and the case, its item and the
/// outcome of the command all land.
#[tokio::test]
async fn link_customer_and_resolve_race_no_500() {
    let c = conv::setup().await;
    for round in 0..5 {
        let (sa, sb) = (format!("cust-a{round}"), format!("cust-b{round}"));
        let (_, b) = inbound_as(
            &c,
            &format!("sl-a{round}"),
            "m1",
            &sa,
            "Can I speak to a real person?",
        )
        .await;
        let case = b["case_id"].as_str().unwrap().to_string();
        inbound_as(
            &c,
            &format!("sl-b{round}"),
            "m1",
            &sb,
            "Can I speak to a real person?",
        )
        .await;
        let target = party_of(&c, &sb).await;
        let conv_id = c
            .texts(&format!(
                "SELECT conversation_id FROM agent_case WHERE case_id = '{case}'"
            ))
            .await
            .remove(0);
        let item = c
            .texts(&format!(
                "SELECT id FROM agent_handover_items WHERE conversation_id = '{conv_id}'"
            ))
            .await
            .remove(0);
        let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
        assert_eq!(s, StatusCode::OK, "round {round}: {b}");
        let url = format!("/api/jobs/j1/cases/{case}/party");
        let resolve = round % 2 == 0;
        let (l, r) = tokio::join!(
            c.call("POST", &url, "u-agent2", json!({"party_id": target})),
            async {
                if resolve {
                    c.resolve(&item, "u-agent", json!({"outcome": "resolved"}))
                        .await
                } else {
                    c.hand_back(&conv_id, "u-agent", "back to the agent").await
                }
            }
        );
        assert_eq!(l.0, StatusCode::OK, "round {round} link: {}", l.1);
        assert_eq!(r.0, StatusCode::OK, "round {round} command: {}", r.1);
        assert_eq!(r.1["owner"], "agent", "round {round}: {}", r.1);
        // consistent: the case and its item name the target, the item is closed
        assert_eq!(
            c.texts(&format!(
                "SELECT (a.party_id = '{target}' AND i.party_id = '{target}')::text \
                   FROM agent_case a JOIN agent_handover_items i ON i.case_id = a.case_id AND i.job_id = a.job_id \
                  WHERE a.case_id = '{case}'"
            ))
            .await,
            ["true"],
            "round {round}"
        );
        assert_eq!(
            c.texts(&format!(
                "SELECT count(*)::text FROM agent_handover_items \
                  WHERE conversation_id = '{conv_id}' AND state NOT IN ('resolved', 'returned_to_agent')"
            ))
            .await,
            ["0"],
            "round {round}: no open item"
        );
    }
}
