//! B4 T11: usage and metering — `usage::emit` (ledger + outbox), the outbox drainer (`agent_claim_metering`, 0129)
//! and `tenantUsage` (`GET /api/tenant/usage`, crate::usage::report).
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{self, get, insert_tenant, insert_user, person, send, sqlstate};
use crate::redis_env as r;
use agent_api::conversations::store::begin_job;
use agent_api::tenancy::begin_scoped;
use agent_api::usage::{self, LlmUsage, MeteringConfig, UsageEvent};

async fn insert_job(db: &PgPool, tenant: Uuid, key: &str, label: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $3, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .bind(label)
    .fetch_one(db)
    .await
    .unwrap()
}

/// One seeded ledger row (owner pool: RLS does not apply).
struct Seed<'a> {
    tenant: Uuid,
    job: Option<Uuid>,
    days_ago: i32,
    source: &'a str,
    use_: &'a str,
    actor: &'a str,
    provider: &'a str,
    model: &'a str,
    funding: &'a str,
    tokens_in: i64,
    tokens_out: i64,
    conversation: Option<&'a str>,
    case_id: Option<&'a str>,
}

async fn seed(db: &PgPool, s: Seed<'_>) {
    sqlx::query(
        "INSERT INTO usage_event (tenant_id, job_id, event_key, occurred_at, source, use, actor, provider_id, model, \
                                  funding, tokens_in, tokens_out, conversation_id, case_id) \
         VALUES ($1, $2, $3, now() - make_interval(days => $4), $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(s.tenant)
    .bind(s.job)
    .bind(Uuid::now_v7())
    .bind(s.days_ago)
    .bind(s.source)
    .bind(s.use_)
    .bind(s.actor)
    .bind(s.provider)
    .bind(s.model)
    .bind(s.funding)
    .bind(s.tokens_in)
    .bind(s.tokens_out)
    .bind(s.conversation)
    .bind(s.case_id)
    .execute(db)
    .await
    .unwrap();
}

fn turn<'a>(tenant: Uuid, job: Uuid, actor: &'a str, funding: &'a str, tokens: i64) -> Seed<'a> {
    Seed {
        tenant,
        job: Some(job),
        days_ago: 0,
        source: if actor.starts_with("callers:") {
            "turn"
        } else {
            "staff_test"
        },
        use_: "qualify",
        actor,
        provider: if funding == "local_gpu" {
            "internal"
        } else {
            "openai"
        },
        model: if funding == "local_gpu" {
            "local-llm"
        } else {
            "gpt-x"
        },
        funding,
        tokens_in: tokens,
        tokens_out: 0,
        conversation: None,
        case_id: None,
    }
}

struct Fx {
    e: common::TestEnv,
    tenant: Uuid,
    returns: Uuid,
    billing: Uuid,
}

/// A tenant (`u-admin`, `u-staff`) with jobs `returns_agent` and `billing_agent`, and a second tenant whose usage
/// must never show.
async fn fixture(e: common::TestEnv) -> Fx {
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, &format!("Usage Co {tenant}")).await;
    insert_user(&e.owner, "u-admin", tenant, "tenant_admin").await;
    insert_user(&e.owner, "u-staff", tenant, "tenant_user").await;
    let returns = insert_job(&e.owner, tenant, "returns_agent", "Returns agent").await;
    let billing = insert_job(&e.owner, tenant, "billing_agent", "Billing agent").await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, &format!("Other Co {other}")).await;
    let oj = insert_job(&e.owner, other, "returns_agent", "Other returns").await;
    seed(
        &e.owner,
        turn(other, oj, "callers:web_chat", "local_gpu", 77_777),
    )
    .await;
    Fx {
        e,
        tenant,
        returns,
        billing,
    }
}

async fn report(e: &common::TestEnv, window: &str, group: &str) -> Value {
    let (s, b) = send(
        &e.app,
        get(
            &format!("/api/tenant/usage?window={window}&group={group}"),
            Some(&person("u-admin", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn row<'a>(b: &'a Value, key: &str) -> &'a Value {
    b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("no row {key}: {b}"))
}

fn lanes_sum(v: &Value) -> i64 {
    ["local_gpu", "platform_external", "tenant_byo"]
        .iter()
        .map(|l| v[*l].as_i64().unwrap())
        .sum()
}

/// The seeded mix of [`report_matches_seeded_events_per_group`] and [`funding_lanes_sum_to_total`].
async fn seed_mix(f: &Fx) {
    let db = &f.e.owner;
    // returns_agent: a customer turn (qualify + reply) that became case CS-1 `returns`, with one knowledge search
    let mut q = turn(f.tenant, f.returns, "callers:web_chat", "local_gpu", 100);
    q.tokens_out = 20;
    q.conversation = Some("conv-1");
    seed(db, q).await;
    let mut reply = turn(f.tenant, f.returns, "callers:web_chat", "local_gpu", 50);
    reply.use_ = "reply";
    reply.tokens_out = 10;
    reply.conversation = Some("conv-1");
    reply.case_id = Some("CS-1");
    seed(db, reply).await;
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, activity, \
                                 started_at) \
         VALUES ($1, $2, 'CS-1', 'conv-1', 'web_chat', 'test', 0, 'returns', now() + interval '1 second')",
    )
    .bind(f.tenant)
    .bind(f.returns)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, env, draft_rev, state, \
                                     finished_at, initiator) \
         VALUES ($1, $2, $3, 'CS-1', 'knowledge.search', 'builtin', 'test', 0, 'ok', now(), 'agent:runner')",
    )
    .bind(f.tenant)
    .bind(f.returns)
    .bind(format!("opc_{}", Uuid::new_v4()))
    .execute(db)
    .await
    .unwrap();
    // returns_agent: a staff test turn on a tenant BYO model
    let mut staff = turn(f.tenant, f.returns, "u-staff", "tenant_byo", 200);
    staff.tokens_out = 30;
    staff.conversation = Some("conv-s");
    seed(db, staff).await;
    // billing_agent: a telegram customer turn
    let mut tg = turn(f.tenant, f.billing, "callers:telegram", "local_gpu", 10);
    tg.tokens_out = 5;
    seed(db, tg).await;
    // a job-less generator run (D56): its own source, never in the turn totals
    let mut generator = turn(f.tenant, f.returns, "u-admin", "platform_external", 1_000);
    generator.job = None;
    generator.source = "generator";
    generator.use_ = "generator";
    seed(db, generator).await;
    // 40 days ago: outside a 7-day window
    let mut old = turn(f.tenant, f.returns, "callers:web_chat", "local_gpu", 999);
    old.days_ago = 40;
    seed(db, old).await;
}

#[tokio::test]
async fn report_matches_seeded_events_per_group() {
    let f = fixture(common::env().await).await;
    seed_mix(&f).await;
    let t = f.tenant.to_string();

    let b = report(&f.e, "7", "job").await;
    assert_eq!(b["window"], 7);
    assert_eq!(b["group"], "job");
    assert_eq!(b["totals"]["turns"], 3, "{b}");
    assert_eq!(b["totals"]["tokens"], 120 + 60 + 230 + 15, "{b}");
    assert_eq!(b["rows"].as_array().unwrap().len(), 2, "{b}");
    let r = row(&b, "returns_agent");
    assert_eq!(r["label"], "Returns agent");
    assert_eq!(r["turns"], 2);
    assert_eq!(r["tokens"], 410);
    assert_eq!(r["funding"]["local_gpu"], 180);
    assert_eq!(r["funding"]["tenant_byo"], 230);
    assert_eq!(row(&b, "billing_agent")["tokens"], 15);
    assert_eq!(b["rows"][0]["key"], "returns_agent", "most turns first");
    let daily = b["daily"].as_array().unwrap();
    assert_eq!(daily.len(), 7);
    assert_eq!(daily[6]["turns"], 3);
    assert_eq!(daily[6]["tokens"], 425);
    assert_eq!(daily[0]["turns"], 0);
    assert_eq!(b["staff_daily"].as_array().unwrap().len(), 7);
    assert_eq!(b["staff_daily"][6], 1);
    assert_eq!(b["to"], daily[6]["day"]);
    assert_eq!(b["from"], daily[0]["day"]);
    assert_eq!(b["generator"], serde_json::json!([]));
    assert_eq!(b["attributed"], false, "no tenant licence yet");
    assert_eq!(b["plan"]["licence"], "none");
    assert!(b["plan"]["allowance"].is_null());
    // drivers: the case's activity, else unqualified; one knowledge search on the returns case
    let drivers = b["drivers"].as_array().unwrap();
    let d = |job: &str, act: &str| {
        drivers
            .iter()
            .find(|d| d["job"] == job && d["activity"] == act)
            .unwrap_or_else(|| panic!("no driver {job}/{act}: {b}"))
            .clone()
    };
    let ret = d("returns_agent", "returns");
    assert_eq!(
        (ret["turns"].as_i64(), ret["tokens"].as_i64()),
        (Some(1), Some(180))
    );
    assert_eq!(ret["searches"], 1);
    let unq = d("returns_agent", "unqualified");
    assert_eq!(
        (unq["turns"].as_i64(), unq["tokens"].as_i64()),
        (Some(1), Some(230))
    );
    assert_eq!(unq["label"], "Not qualified");
    assert_eq!(d("billing_agent", "unqualified")["tokens"], 15);
    assert_eq!(b["knowledge"]["searches"], 1);
    // sources: customers vs staff by actor; the generator apart, with no turns
    let src = |k: &str| {
        b["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == k)
            .unwrap()
            .clone()
    };
    assert_eq!(src("customers")["turns"], 2);
    assert_eq!(src("customers")["tokens"], 195);
    assert_eq!(src("staff")["turns"], 1);
    assert_eq!(src("staff")["tokens"], 230);
    assert!(src("generator")["turns"].is_null());
    assert_eq!(src("generator")["tokens"], 1_000);
    assert_eq!(src("generator")["funding"]["platform_external"], 1_000);

    let b = report(&f.e, "7", "user").await;
    assert_eq!(b["rows"].as_array().unwrap().len(), 3, "{b}");
    let web = row(&b, "callers:web_chat");
    assert_eq!(web["label"], "Callers (web_chat)");
    assert_eq!(
        (web["turns"].as_i64(), web["tokens"].as_i64()),
        (Some(1), Some(180))
    );
    assert_eq!(row(&b, "u-staff")["label"], "u-staff");
    assert_eq!(row(&b, "u-staff")["tokens"], 230);
    assert_eq!(row(&b, "callers:telegram")["label"], "Callers (telegram)");

    let b = report(&f.e, "7", "project").await;
    assert_eq!(b["rows"].as_array().unwrap().len(), 1, "{b}");
    let p = row(&b, &t);
    assert!(
        p["label"].as_str().unwrap().ends_with("(tenant project)"),
        "{b}"
    );
    assert_eq!(
        (p["turns"].as_i64(), p["tokens"].as_i64()),
        (Some(3), Some(425))
    );

    let b = report(&f.e, "7", "provider").await;
    let local = row(&b, "internal · local-llm");
    assert_eq!(
        (local["turns"].as_i64(), local["tokens"].as_i64()),
        (Some(2), Some(195))
    );
    assert_eq!(row(&b, "openai · gpt-x")["tokens"], 230);

    // the 40-day-old turn is inside 90 days; the other tenant's never shows
    let b = report(&f.e, "90", "job").await;
    assert_eq!(b["totals"]["turns"], 4);
    assert_eq!(b["totals"]["tokens"], 425 + 999);
    assert_eq!(b["daily"].as_array().unwrap().len(), 90);
}

#[tokio::test]
async fn funding_lanes_sum_to_total() {
    let f = fixture(common::env().await).await;
    seed_mix(&f).await;
    for (who, funding, tokens) in [
        ("callers:web_chat", "platform_external", 7),
        ("u-admin", "platform_external", 11),
        ("u-staff", "local_gpu", 13),
    ] {
        seed(&f.e.owner, turn(f.tenant, f.billing, who, funding, tokens)).await;
    }
    for group in ["job", "user", "project", "provider"] {
        let b = report(&f.e, "30", group).await;
        let total = b["totals"]["tokens"].as_i64().unwrap();
        assert_eq!(
            total,
            425 + 7 + 11 + 13,
            "{group}: the 40-day-old turn is outside 30 days: {b}"
        );
        assert_eq!(lanes_sum(&b["funding"]), total, "{group}: funding lanes");
        let mut rows_sum = 0;
        for r in b["rows"].as_array().unwrap() {
            assert_eq!(
                lanes_sum(&r["funding"]),
                r["tokens"].as_i64().unwrap(),
                "{group}: {r}"
            );
            rows_sum += r["tokens"].as_i64().unwrap();
        }
        assert_eq!(rows_sum, total, "{group}: rows add up to the total");
        let daily: i64 = b["daily"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["tokens"].as_i64().unwrap())
            .sum();
        assert_eq!(daily, total, "{group}: days add up to the total");
        let conv: i64 = b["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["key"] != "generator")
            .map(|s| {
                assert_eq!(
                    lanes_sum(&s["funding"]),
                    s["tokens"].as_i64().unwrap(),
                    "{s}"
                );
                s["tokens"].as_i64().unwrap()
            })
            .sum();
        assert_eq!(conv, total, "{group}: customers + staff = the total");
        assert_eq!(
            b["funding"]["platform_external"], 18,
            "{group}: the generator's lane is not a turn's"
        );
    }
}

#[tokio::test]
async fn previous_is_null_beyond_90_days() {
    let f = fixture(common::env().await).await;
    for (days_ago, tokens) in [(0, 5), (40, 999), (70, 3)] {
        let mut s = turn(f.tenant, f.returns, "callers:web_chat", "local_gpu", tokens);
        s.days_ago = days_ago;
        seed(&f.e.owner, s).await;
    }
    let b = report(&f.e, "90", "job").await;
    assert!(
        b["previous"].is_null(),
        "180 days back is beyond the ledger: {b}"
    );
    let b = report(&f.e, "30", "job").await;
    assert_eq!(b["previous"]["turns"], 1, "{b}");
    assert_eq!(b["previous"]["tokens"], 999, "days 30..59 back");
    let b = report(&f.e, "7", "job").await;
    assert_eq!(b["previous"]["turns"], 0, "{b}");
    assert_eq!(b["previous"]["tokens"], 0);
    // a window or group the UI does not know is 422
    for q in ["window=14&group=job", "window=7&group=team", "window=x"] {
        let (s, body) = send(
            &f.e.app,
            get(
                &format!("/api/tenant/usage?{q}"),
                Some(&person("u-admin", &[])),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {body}");
        assert_eq!(body["error"]["code"], "invalid_query", "{q}");
    }
}

fn usage_of(tokens: i64) -> LlmUsage {
    LlmUsage {
        provider_id: "internal".into(),
        model: "local-llm".into(),
        funding: "local_gpu".into(),
        tokens_in: tokens,
        tokens_out: 1,
        latency_ms: 10,
    }
}

async fn emit_n(
    e: &common::TestEnv,
    cfg: &MeteringConfig,
    tenant: Uuid,
    job: Uuid,
    n: i64,
) -> Vec<Uuid> {
    let mut keys = Vec::new();
    for i in 0..n {
        let u = usage_of(i);
        let mut tx = begin_job(&e.state.db, tenant, job).await.unwrap();
        let k = usage::emit(
            &mut tx,
            cfg,
            &UsageEvent {
                job_id: Some(job),
                source: "turn",
                use_: "qualify",
                activity: None,
                conversation_id: Some("conv-d"),
                case_id: None,
                actor: "callers:web_chat",
                usage: &u,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        keys.push(k);
    }
    keys
}

async fn count(db: &PgPool, sql: &'static str, tenant: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(tenant)
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn two_drainers_send_once() {
    let url = r::url().await;
    let e = common::env_with(|v| {
        v.insert("METERING_EMIT_ENABLED".into(), "true".into());
        v.insert("METERING_REDIS_URL".into(), url.clone());
    })
    .await;
    let cfg = e.state.cfg.metering.clone();
    assert!(cfg.emit_enabled);
    let f = fixture(e).await;
    let e = &f.e;
    sqlx::query("UPDATE tenants SET licence_id = 'lic-usage' WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    let keys = emit_n(e, &cfg, f.tenant, f.returns, 25).await;
    let outbox = "SELECT count(*) FROM metering_outbox WHERE tenant_id = $1";
    let ledger = "SELECT count(*) FROM usage_event WHERE tenant_id = $1";
    assert_eq!(count(&e.owner, ledger, f.tenant).await, 25);
    assert_eq!(
        count(&e.owner, outbox, f.tenant).await,
        25,
        "emit on: one outbox row per ledger row"
    );
    let attributed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM metering_outbox WHERE tenant_id = $1 AND fields ->> 'license_id' = 'lic-usage' \
            AND fields ->> 'funding' = 'local_gpu' AND fields ->> 'project' = $1::text",
    )
    .bind(f.tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        attributed, 25,
        "every cost event carries the licence, the lane and the project"
    );

    // emit off: the ledger only
    emit_n(e, &MeteringConfig::default(), f.tenant, f.returns, 1).await;
    assert_eq!(count(&e.owner, ledger, f.tenant).await, 26);
    assert_eq!(count(&e.owner, outbox, f.tenant).await, 25);
    // no licence: the cost is not sent (loud), the ledger keeps it
    let bare = Uuid::new_v4();
    insert_tenant(&e.owner, bare, &format!("Bare {bare}")).await;
    let bj = insert_job(&e.owner, bare, "bare_agent", "Bare").await;
    emit_n(e, &cfg, bare, bj, 1).await;
    assert_eq!(count(&e.owner, ledger, bare).await, 1);
    assert_eq!(
        count(&e.owner, outbox, bare).await,
        0,
        "an unattributable cost event is never sent"
    );

    let stream = format!("{}costs", r::prefix("usage"));
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c1 = redis::aio::ConnectionManager::new(client.clone())
        .await
        .unwrap();
    let mut c2 = redis::aio::ConnectionManager::new(client).await.unwrap();
    let db = e.state.db.clone();
    let (a, b) = tokio::join!(
        usage::drainer::drain_once(&db, &mut c1, &stream),
        usage::drainer::drain_once(&db, &mut c2, &stream),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.sent + b.sent, 25, "{a:?} {b:?}");
    assert_eq!(a.failed + b.failed, 0);
    let again = usage::drainer::drain_once(&db, &mut c1, &stream)
        .await
        .unwrap();
    assert_eq!(again.claimed, 0, "nothing is due twice");
    let len: i64 = redis::cmd("XLEN")
        .arg(&stream)
        .query_async(&mut c1)
        .await
        .unwrap();
    assert_eq!(len, 25, "each event on the stream exactly once");
    let entries: redis::streams::StreamRangeReply = redis::cmd("XRANGE")
        .arg(&stream)
        .arg("-")
        .arg("+")
        .query_async(&mut c1)
        .await
        .unwrap();
    let mut sent: Vec<String> = entries
        .ids
        .iter()
        .map(|i| i.get::<String>("event_key").unwrap())
        .collect();
    sent.sort_unstable();
    let mut want: Vec<String> = keys.iter().map(Uuid::to_string).collect();
    want.sort_unstable();
    assert_eq!(sent, want);
    let pending: i64 = count(
        &e.owner,
        "SELECT count(*) FROM metering_outbox WHERE tenant_id = $1 AND (status <> 'sent' OR sent_at IS NULL)",
        f.tenant,
    )
    .await;
    assert_eq!(pending, 0, "every row settled as sent");
    let _: i64 = redis::cmd("DEL")
        .arg(&stream)
        .query_async(&mut c1)
        .await
        .unwrap();
}

/// Acceptance defects 2026-10-21: a turn whose case was qualified by request type (the runtime never writes
/// `activity`) drives that request type, not "Not qualified"; a staff actor with no `users` row in the tenant (a
/// platform user) is named from `actor_names`, not shown as the raw sub.
#[tokio::test]
async fn drivers_use_request_type_and_staff_names_resolve_actor_names() {
    let f = fixture(common::env().await).await;
    let db = &f.e.owner;
    let platform = format!("u-platform-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO actor_names (sub, name) VALUES ($1, 'fr-test (platform)')")
        .bind(&platform)
        .execute(db)
        .await
        .unwrap();
    let mut q = turn(f.tenant, f.returns, &platform, "local_gpu", 40);
    q.tokens_out = 2;
    q.source = "staff_test";
    q.conversation = Some("conv-q");
    seed(db, q).await;
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, request_type, \
                                 started_at) \
         VALUES ($1, $2, 'CS-9', 'conv-q', 'staff', 'test', 0, 'order_status', now() + interval '1 second')",
    )
    .bind(f.tenant)
    .bind(f.returns)
    .execute(db)
    .await
    .unwrap();

    let b = report(&f.e, "7", "job").await;
    let drivers = b["drivers"].as_array().unwrap();
    let d = drivers
        .iter()
        .find(|d| d["job"] == "returns_agent" && d["activity"] == "order_status")
        .unwrap_or_else(|| panic!("no order_status driver: {b}"));
    assert_eq!(d["label"], "Order status", "{b}");
    assert_eq!(
        (d["turns"].as_i64(), d["tokens"].as_i64()),
        (Some(1), Some(42))
    );
    assert!(
        !drivers.iter().any(|d| d["activity"] == "unqualified"),
        "the only turn was qualified: {b}"
    );

    let b = report(&f.e, "7", "user").await;
    assert_eq!(row(&b, &platform)["label"], "fr-test (platform)", "{b}");
}

/// 0253: the rollup is a materialisation, not an append-only cache. The owner can update a ledger row
/// (0233 did), so an UPDATE that moves it between dimension keys must remove the old key and add the
/// new one; a DELETE must remove the key once its event count reaches zero.
async fn rollup_row(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    actor: &str,
) -> Option<(i64, i64, i64, i64)> {
    sqlx::query_as(
        "SELECT events, calls, tokens_in, tokens_out FROM usage_rollup_daily \
          WHERE tenant_id = $1 AND job_id = $2 AND actor = $3 AND use = 'qualify' \
            AND source = 'turn' AND funding = 'local_gpu' AND provider_id = 'internal' \
            AND model = 'local-llm'",
    )
    .bind(tenant)
    .bind(job)
    .bind(actor)
    .fetch_optional(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn rollup_reconciles_owner_update_and_delete() {
    let f = fixture(common::env().await).await;
    let db = &f.e.owner;
    let mut e = turn(f.tenant, f.returns, "callers:web_chat", "local_gpu", 100);
    e.tokens_out = 20;
    seed(db, e).await;
    assert_eq!(
        rollup_row(db, f.tenant, f.returns, "callers:web_chat").await,
        Some((1, 1, 100, 20))
    );

    sqlx::query(
        "UPDATE usage_event SET actor = 'callers:telegram', tokens_in = 40 \
          WHERE tenant_id = $1 AND job_id = $2",
    )
    .bind(f.tenant)
    .bind(f.returns)
    .execute(db)
    .await
    .unwrap();
    assert_eq!(
        rollup_row(db, f.tenant, f.returns, "callers:web_chat").await,
        None
    );
    assert_eq!(
        rollup_row(db, f.tenant, f.returns, "callers:telegram").await,
        Some((1, 1, 40, 20))
    );

    sqlx::query("DELETE FROM usage_event WHERE tenant_id = $1 AND job_id = $2")
        .bind(f.tenant)
        .bind(f.returns)
        .execute(db)
        .await
        .unwrap();
    assert_eq!(
        rollup_row(db, f.tenant, f.returns, "callers:telegram").await,
        None
    );
}

/// 0253: the rollup carries both RLS scopes for `agent_app`, and `usage_daily` reads it through the
/// same policies (`security_invoker`). The trigger functions are not directly callable by the runtime
/// roles, so a caller cannot desync billing totals without a ledger row.
#[tokio::test]
async fn rollup_is_tenant_and_job_scoped_and_not_directly_writable() {
    let f = fixture(common::env().await).await;
    let other = Uuid::new_v4();
    insert_tenant(&f.e.owner, other, &format!("Rollup Co {other}")).await;
    let oj = insert_job(&f.e.owner, other, "returns_agent", "Other returns").await;
    seed(
        &f.e.owner,
        turn(f.tenant, f.returns, "callers:web_chat", "local_gpu", 100),
    )
    .await;
    seed(
        &f.e.owner,
        turn(f.tenant, f.billing, "callers:web_chat", "local_gpu", 200),
    )
    .await;
    seed(
        &f.e.owner,
        turn(other, oj, "callers:web_chat", "local_gpu", 500),
    )
    .await;

    async fn jobs(conn: &mut sqlx::PgConnection, rel: &str) -> Vec<Option<Uuid>> {
        let sql = match rel {
            "usage_rollup_daily" => "SELECT DISTINCT job_id FROM usage_rollup_daily",
            "usage_daily" => "SELECT DISTINCT job_id FROM usage_daily",
            other => panic!("unexpected relation {other}"),
        };
        let mut seen: Vec<Option<Uuid>> = sqlx::query_scalar(sql).fetch_all(conn).await.unwrap();
        seen.sort();
        seen
    }

    // Tenant scope on the table and the view: this tenant's two jobs, never the fixture's other tenant.
    let mut want = vec![Some(f.returns), Some(f.billing)];
    want.sort();
    let mut tx = begin_scoped(&f.e.state.db, f.tenant).await.unwrap();
    for rel in ["usage_rollup_daily", "usage_daily"] {
        assert_eq!(jobs(&mut *tx, rel).await, want, "{rel}");
    }
    let tokens: i64 =
        sqlx::query_scalar("SELECT coalesce(sum(tokens_in), 0)::bigint FROM usage_daily")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(tokens, 300);
    let err = sqlx::query(
        "INSERT INTO usage_rollup_daily (tenant_id, day) VALUES (app_current_tenant(), CURRENT_DATE)",
    )
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);

    // Job scope: only that job's row.
    let mut tx = begin_scoped(&f.e.state.db, f.tenant).await.unwrap();
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(f.returns.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    for rel in ["usage_rollup_daily", "usage_daily"] {
        assert_eq!(jobs(&mut *tx, rel).await, vec![Some(f.returns)], "{rel}");
    }
    drop(tx);

    // The other tenant sees only its own job.
    let mut tx = begin_scoped(&f.e.state.db, other).await.unwrap();
    for rel in ["usage_rollup_daily", "usage_daily"] {
        assert_eq!(jobs(&mut *tx, rel).await, vec![Some(oj)], "{rel}");
    }
    drop(tx);

    // No direct EXECUTE for the runtime roles on the trigger functions.
    for role in ["agent_app", "agent_runtime"] {
        for func in [
            "public.usage_rollup_add(uuid, date, uuid, text, text, text, text, text, text, text, bigint, bigint, bigint, bigint, bigint, integer)",
            "public.usage_rollup_ins()",
            "public.usage_rollup_upd()",
            "public.usage_rollup_del()",
        ] {
            let can: bool =
                sqlx::query_scalar("SELECT has_function_privilege($1::name, $2, 'EXECUTE')")
                    .bind(role)
                    .bind(func)
                    .fetch_one(&f.e.owner)
                    .await
                    .unwrap();
            assert!(!can, "{role} can execute {func}");
        }
    }
}
