//! Staff Workspace T7: the tenant-wide support inbox (`GET /api/support/inbox`, crate::support::inbox) and a job's
//! canned replies (migration 0193, crate::support::canned). Tabs and counts follow the one owner record; rows sort by
//! the SLA due time the web uses, then by waiting time; the staff roles are tenant-wide, so a caller with one sees every
//! job of its tenant and nothing of another tenant; a pre-SW staff owner without an item is still in its owner's Mine;
//! no row names a customer by a raw channel id. Real Postgres 18 (no Redis: `unread` is false).
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};
use uuid::Uuid;

/// A web chat conversation `wc` whose customer asked for a person: (conversation id, item id).
async fn queued(c: &Ctx, wc: &str) -> (String, String) {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    (
        b["conversation_id"].as_str().unwrap().to_string(),
        b["item_id"].as_str().unwrap().to_string(),
    )
}

/// The inbox as `sub` with query `q` (`""` or `?…`).
async fn inbox(c: &Ctx, sub: &str, q: &str) -> Value {
    let (s, b) = c.get(&format!("/api/support/inbox{q}"), sub).await;
    assert_eq!(s, StatusCode::OK, "{q}: {b}");
    b
}

/// The conversation ids of an inbox's rows, in order.
fn ids(b: &Value) -> Vec<String> {
    b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["conversation_id"].as_str().unwrap().to_string())
        .collect()
}

/// The row of `conv`, if listed.
fn row(b: &Value, conv: &str) -> Option<Value> {
    b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["conversation_id"] == conv)
        .cloned()
}

/// Run `sql` with `binds` (text) on the owner pool.
async fn exec(c: &Ctx, sql: &str, binds: &[&str]) {
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).bind(c.t);
    for b in binds {
        q = q.bind(b.to_string());
    }
    q.execute(&c.e.owner).await.unwrap();
}

/// A conversation `conv` on `channel` in job `job` of tenant `tenant`, owned by the queue, without messages or items.
async fn bare_queued(c: &Ctx, tenant: Uuid, job: Uuid, conv: &str, channel: &str) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) VALUES ($1, $2, $3, 'prod', $4)",
    )
    .bind(tenant)
    .bind(job)
    .bind(conv)
    .bind(channel)
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub) \
         VALUES ($1, $2, $3, 'queue', NULL) \
         ON CONFLICT (tenant_id, job_id, conversation_id) DO UPDATE SET mode = 'queue', by_sub = NULL",
    )
    .bind(tenant)
    .bind(job)
    .bind(conv)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// A job `key` of `tenant`.
async fn job(c: &Ctx, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, upper($2), '[]', 'test') \
         RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn mine_unassigned_open_counts() {
    let c = conv::setup().await;
    let (q, q_item) = queued(&c, "wc-q").await;
    let (mine, _) = queued(&c, "wc-mine").await;
    let (other, _) = queued(&c, "wc-other").await;
    let (s, b) = c.take(&mine, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c.take(&other, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let b = inbox(&c, "u-agent", "?tab=mine").await;
    assert_eq!(
        b["counts"],
        json!({"mine": 1, "unassigned": 1, "open": 3}),
        "{b}"
    );
    assert_eq!(ids(&b), vec![mine.clone()], "{b}");
    let r = row(&b, &mine).unwrap();
    assert_eq!(r["owner"]["owner"], "user:u-agent", "{r}");
    assert_eq!(r["owner"]["name"], "u-agent", "{r}");
    assert!(r["item_id"].is_string(), "{r}");
    assert_eq!(r["owner"]["item_id"], r["item_id"], "{r}");
    assert_eq!(r["job"], "j1", "{r}");
    assert_eq!(r["job_name"], "J1", "{r}");
    assert_eq!(r["channel"], "web_chat", "{r}");
    assert_eq!(r["reason_label"], "Customer asked", "{r}");
    assert!(r["state"].is_string(), "{r}");
    assert!(r["sla_starts_at"].is_string(), "{r}");
    assert!(r["waiting_since"].is_string(), "{r}");
    assert_eq!(r["unread"], false, "without Redis nothing is unread: {r}");
    for k in ["first_response_due", "first_response_at", "resolve_due"] {
        assert!(r.get(k).is_some(), "{k}: {r}");
    }
    assert!(r["last_message"]["text"].as_str().unwrap().chars().count() <= 140);

    let b = inbox(&c, "u-agent", "?tab=unassigned").await;
    assert_eq!(ids(&b), vec![q.clone()], "{b}");
    let r = row(&b, &q).unwrap();
    assert_eq!(r["owner"]["owner"], "queue", "{r}");
    assert_eq!(r["item_id"], q_item.as_str(), "{r}");

    let mut open = ids(&inbox(&c, "u-agent", "?tab=open").await);
    open.sort();
    let mut want = vec![q.clone(), mine.clone(), other.clone()];
    want.sort();
    assert_eq!(open, want);
    // no tab is the open tab
    assert_eq!(ids(&inbox(&c, "u-agent", "").await).len(), 3);
    // the counts are the caller's
    let b = inbox(&c, "u-agent2", "?tab=mine").await;
    assert_eq!(ids(&b), vec![other.clone()], "{b}");
    assert_eq!(b["counts"]["mine"], 1);
    // filters: the reason by label (what the web sends) or by code, the channel, the priority
    assert_eq!(
        ids(&inbox(&c, "u-agent", "?tab=open&reason=Customer%20asked").await).len(),
        3
    );
    assert_eq!(
        ids(&inbox(&c, "u-agent", "?tab=open&reason=customer_asked").await).len(),
        3
    );
    let b = inbox(&c, "u-agent", "?tab=open&reason=Gate%20blocked").await;
    assert!(ids(&b).is_empty(), "{b}");
    assert_eq!(b["counts"]["open"], 3, "counts ignore the filters: {b}");
    assert!(ids(&inbox(&c, "u-agent", "?tab=open&channel=telegram").await).is_empty());
    let (s, b) = c.get("/api/support/inbox?tab=everything", "u-agent").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn inbox_sorted_by_sla_then_wait() {
    let c = conv::setup().await;
    let (a, ia) = queued(&c, "wc-a").await;
    let (b, ib) = queued(&c, "wc-b").await;
    let (cc, ic) = queued(&c, "wc-c").await;
    let (d, id) = queued(&c, "wc-d").await;
    let set = "UPDATE agent_handover_items SET first_response_due = {due}, first_response_at = {at}, \
               resolve_due = {resolve}, queued_at = {queued} WHERE tenant_id = $1 AND id = $2";
    let up = |due: &str, at: &str, resolve: &str, queued: &str| {
        set.replace("{due}", due)
            .replace("{at}", at)
            .replace("{resolve}", resolve)
            .replace("{queued}", queued)
    };
    // a: first response due in 30 min
    exec(
        &c,
        &up(
            "now() + interval '30 minutes'",
            "NULL",
            "now() + interval '4 hours'",
            "now() - interval '10 minutes'",
        ),
        &[&ia],
    )
    .await;
    // b: answered, so its resolve due (1 h) counts, not its (past) first response due
    exec(
        &c,
        &up(
            "now() - interval '1 hour'",
            "now() - interval '5 minutes'",
            "now() + interval '1 hour'",
            "now() - interval '3 hours'",
        ),
        &[&ib],
    )
    .await;
    // c and d: no due time, so last, the longer waiting (c) first
    exec(
        &c,
        &up("NULL", "NULL", "NULL", "now() - interval '2 hours'"),
        &[&ic],
    )
    .await;
    exec(
        &c,
        &up("NULL", "NULL", "NULL", "now() - interval '1 hour'"),
        &[&id],
    )
    .await;
    let body = inbox(&c, "u-agent", "?tab=open").await;
    assert_eq!(ids(&body), vec![a, b.clone(), cc, d], "{body}");
    // the first response due is kept after the first response (the web derives the due time from both)
    let r = row(&body, &b).unwrap();
    assert!(r["first_response_due"].is_string(), "{r}");
    assert!(r["first_response_at"].is_string(), "{r}");
    assert!(r["resolve_due"].is_string(), "{r}");
}

#[tokio::test]
async fn inbox_hides_jobs_without_support_role() {
    let c = conv::setup().await;
    let (q, _) = queued(&c, "wc-q").await;
    // members without a staff role are refused the inbox and the stream
    for sub in ["u-obs", "u-int"] {
        for path in ["/api/support/inbox", "/api/support/events"] {
            let (s, b) = c.get(path, sub).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{sub} {path}: {b}");
            assert_eq!(code(&b), "forbidden", "{b}");
            assert_eq!(b["error"]["missing_role"], "support_agent", "{b}");
        }
    }
    // another job of the tenant is listed (the staff roles are tenant-wide); another tenant's job never is
    let j2 = job(&c, c.t, "j2").await;
    bare_queued(&c, c.t, j2, "j2-conv", "web_chat").await;
    let beta = Uuid::new_v4();
    common::insert_tenant(&c.e.owner, beta, "Beta").await;
    let bj = job(&c, beta, "j1").await;
    bare_queued(&c, beta, bj, "beta-conv", "web_chat").await;
    for sub in ["u-agent", "u-mgr"] {
        let b = inbox(&c, sub, "?tab=unassigned").await;
        let listed = ids(&b);
        assert!(listed.contains(&q), "{sub}: {b}");
        assert!(listed.contains(&"j2-conv".to_string()), "{sub}: {b}");
        assert!(!listed.contains(&"beta-conv".to_string()), "{sub}: {b}");
        assert_eq!(b["counts"]["unassigned"], 2, "{sub}: {b}");
        let r = row(&b, "j2-conv").unwrap();
        assert_eq!((&r["job"], &r["job_name"]), (&json!("j2"), &json!("J2")));
        assert!(r["item_id"].is_null(), "{r}");
    }
}

#[tokio::test]
async fn legacy_staff_owned_without_item_in_mine() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-legacy", "wc-legacy-1", "Hello").await;
    // a pre-SW console takeover: staff owner, no handover item
    let job_id = c.job.to_string();
    exec(
        &c,
        "INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub) \
         VALUES ($1, $3::uuid, $2, 'staff', 'u-agent') \
         ON CONFLICT (tenant_id, job_id, conversation_id) \
         DO UPDATE SET mode = 'staff', by_sub = 'u-agent', since = now()",
        &[&id, &job_id],
    )
    .await;
    let b = inbox(&c, "u-agent", "?tab=mine").await;
    assert_eq!(b["counts"]["mine"], 1, "{b}");
    let r = row(&b, &id).unwrap_or_else(|| panic!("not in Mine: {b}"));
    assert!(r["item_id"].is_null(), "{r}");
    assert_eq!(r["owner"]["owner"], "user:u-agent", "{r}");
    assert!(r["owner"]["item_id"].is_null(), "{r}");
    assert!(r["state"].is_null() && r["reason_label"].is_null(), "{r}");
    assert_eq!(r["priority"], "normal", "{r}");
    assert_eq!(r["last_message"]["kind"], "customer", "{r}");
    assert_eq!(r["last_message"]["text"], "Hello", "{r}");
    assert!(ids(&inbox(&c, "u-agent2", "?tab=mine").await).is_empty());
    // and it can be handed back
    let (s, h) = c.hand_back(&id, "u-agent", "Back to you").await;
    assert_eq!(s, StatusCode::OK, "{h}");
    assert_eq!(h["owner"], "agent", "{h}");
    let b = inbox(&c, "u-agent", "?tab=mine").await;
    assert!(ids(&b).is_empty(), "{b}");
    assert!(!ids(&inbox(&c, "u-agent", "?tab=open").await).contains(&id));
}

#[tokio::test]
async fn inbox_rows_show_no_raw_ids() {
    let c = conv::setup().await;
    let (named, _) = queued(&c, "wc-named").await;
    let (anon, _) = queued(&c, "wc-anon").await;
    // nobody named the second customer: no party name, no profile name
    exec(
        &c,
        "UPDATE party SET display_name = NULL \
          WHERE tenant_id = $1 AND id = (SELECT party_id FROM conversation WHERE tenant_id = $1 AND conversation_id = $2)",
        &[&anon],
    )
    .await;
    exec(
        &c,
        "DELETE FROM conversation_state WHERE tenant_id = $1 AND conversation_id = $2 AND key = 'name'",
        &[&anon],
    )
    .await;
    bare_queued(&c, c.t, c.job, "tg-123456", "telegram").await;
    let b = inbox(&c, "u-agent", "?tab=open").await;
    let rows = b["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{b}");
    for r in rows {
        let n = r["display_name"].as_str().unwrap();
        assert!(
            !n.starts_with("wc:") && !n.starts_with("tg-") && n != r["conversation_id"],
            "{r}"
        );
    }
    assert_eq!(row(&b, &named).unwrap()["display_name"], "Femke");
    let tail: String = {
        let ch: Vec<char> = anon.chars().collect();
        ch[ch.len().saturating_sub(4)..].iter().collect()
    };
    assert_eq!(
        row(&b, &anon).unwrap()["display_name"],
        format!("Web visitor · {tail}").as_str()
    );
    assert_eq!(
        row(&b, "tg-123456").unwrap()["display_name"],
        "Telegram user · 3456"
    );
}

#[tokio::test]
async fn inbox_row_and_header_say_whether_the_customer_is_verified() {
    let c = conv::setup().await;
    let (anon, _) = queued(&c, "wc-vanon").await;
    let (known, _) = queued(&c, "wc-vknown").await;
    let header = |id: String| {
        let c = &c;
        async move {
            let (s, t) = c
                .get(&format!("/api/jobs/j1/conversations/{id}"), "u-agent")
                .await;
            assert_eq!(s, StatusCode::OK, "{t}");
            t["conversation"]["verified"].clone()
        }
    };
    let b = inbox(&c, "u-agent", "?tab=open").await;
    assert_eq!(row(&b, &anon).unwrap()["verified"], false, "{b}");
    assert_eq!(row(&b, &known).unwrap()["verified"], false, "{b}");
    assert_eq!(header(anon.clone()).await, false);
    assert_eq!(header(known.clone()).await, false);
    exec(
        &c,
        "UPDATE conversation SET customer_id = 'cust-1001' WHERE tenant_id = $1 AND conversation_id = $2",
        &[&known],
    )
    .await;
    common::state::identify(
        &c.e.owner,
        c.t,
        c.job,
        &known,
        "cust-1001",
        Some("a@shop.example"),
    )
    .await;
    let b = inbox(&c, "u-agent", "?tab=open").await;
    assert_eq!(row(&b, &known).unwrap()["verified"], true, "{b}");
    assert_eq!(row(&b, &anon).unwrap()["verified"], false, "{b}");
    assert_eq!(header(known).await, true);
    assert_eq!(header(anon).await, false);
}

#[tokio::test]
async fn canned_reply_manager_only_writes() {
    let c = conv::setup().await;
    let path = "/api/jobs/j1/canned-replies";
    let reply = json!({"title": "  Refund timing ", "body": "Refunds take 5 working days."});
    // a support agent may not write; an observer may not either
    for sub in ["u-agent", "u-obs"] {
        let (s, b) = c.call("POST", path, sub, reply.clone()).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub}: {b}");
        assert_eq!(b["error"]["missing_role"], "support_manager", "{b}");
    }
    let (s, made) = c.call("POST", path, "u-mgr", reply.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    assert_eq!(made["title"], "Refund timing");
    assert_eq!(made["body"], "Refunds take 5 working days.");
    assert_eq!(made["created_by"], "u-mgr");
    assert!(made["updated_at"].is_string(), "{made}");
    let id = made["id"].as_str().unwrap().to_string();
    // too long / blank
    for bad in [
        json!({"title": "t".repeat(81), "body": "b"}),
        json!({"title": "t", "body": "b".repeat(4001)}),
        json!({"title": " ", "body": "b"}),
        json!({"body": "b"}),
    ] {
        let (s, b) = c.call("POST", path, "u-mgr", bad).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(code(&b), "invalid_canned_reply", "{b}");
    }
    // every member reads them
    for sub in ["u-agent", "u-obs"] {
        let (s, list) = c.get(path, sub).await;
        assert_eq!(s, StatusCode::OK, "{list}");
        assert_eq!(list, json!([made.clone()]), "{sub}");
    }
    // another job's list is its own
    job(&c, c.t, "j2").await;
    let (s, list) = c.get("/api/jobs/j2/canned-replies", "u-agent").await;
    assert_eq!((s, list), (StatusCode::OK, json!([])));
    // only the manager deletes
    let one = format!("{path}/{id}");
    let (s, b) = c.call("DELETE", &one, "u-agent", json!({})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = c.call("DELETE", &one, "u-mgr", json!({})).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = c.call("DELETE", &one, "u-mgr", json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "canned_reply_not_found", "{b}");
    let (_, list) = c.get(path, "u-agent").await;
    assert_eq!(list, json!([]));
    // both writes are audited, never with the text
    let audit: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT action, resource_id, details FROM audit_event \
          WHERE tenant_id = $1 AND action LIKE 'canned_reply.%' ORDER BY id",
    )
    .bind(c.t)
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    let actions: Vec<&str> = audit.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["canned_reply.create", "canned_reply.delete"]);
    assert!(audit.iter().all(|(_, rid, _)| *rid == id), "{audit:?}");
    assert!(
        audit
            .iter()
            .all(|(_, _, d)| !d.to_string().contains("Refund")),
        "{audit:?}"
    );
}

#[tokio::test]
async fn staff_test_chat_is_not_in_the_inbox() {
    let c = conv::setup().await;
    let (web, _) = queued(&c, "wc-web").await;
    let (chat, item) = queued(&c, "wc-chat").await;
    // the second one is a staff test chat with an open item (owned by the queue)
    exec(
        &c,
        "UPDATE conversation SET channel = 'staff' WHERE tenant_id = $1 AND conversation_id = $2",
        &[&chat],
    )
    .await;
    assert_eq!(c.item(&item).await["owner"]["owner"], "queue");
    for tab in ["open", "unassigned"] {
        let b = inbox(&c, "u-agent", &format!("?tab={tab}")).await;
        assert_eq!(ids(&b), vec![web.clone()], "{tab}: {b}");
        assert_eq!(
            b["counts"],
            json!({"mine": 0, "unassigned": 1, "open": 1}),
            "{b}"
        );
    }
    // also when it is mine
    let (s, t) = c.take(&chat, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let b = inbox(&c, "u-agent", "?tab=mine").await;
    assert!(ids(&b).is_empty(), "{b}");
    assert_eq!(b["counts"]["mine"], 0, "{b}");
}
