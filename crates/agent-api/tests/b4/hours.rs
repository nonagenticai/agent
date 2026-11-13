//! B4 T16a (F-14): business hours on the handover policy and the tenant holiday calendar — the `tenant_calendar`
//! design kind (contract, tenant draft recorded in every job, 422 `invalid_calendar`, `support_manager` only), the
//! policy's `hours` (422 `invalid_policy` with `details.field = "hours"`), the runner's out-of-hours check on
//! `case.create` (live hours in prod, the draft in test; the policy's own timezone, never the session's), the
//! out-of-hours reply, and take-next leaving items whose clock has not started for last. Real Postgres 18 (one
//! container for the `b4` binary, a fresh database per test).
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgPool;

use crate::common::conv::{self, Ctx, code};
use crate::common::{self, design_contract};
use agent_api::conversations::store;
use agent_api::design::{CaseDesign, DraftRef, JobRef};
use agent_api::runtime::domain::Resolution;
use agent_api::runtime::reply;
use agent_api::runtime::runner::agent_cases;
use agent_api::runtime::runner::ops::{OpOutcome, OpState};

pub(crate) fn targets() -> Value {
    let t = |fr: i64, res: i64| json!({"first_response_min": fr, "resolve_min": res});
    json!({"high": t(15, 240), "normal": t(60, 1440), "low": t(240, 4320)})
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// Monday to Friday 09:00–17:00 in `tz`.
fn weekdays(tz: &str, mode: &str) -> Value {
    let w = json!([{"from": "09:00", "to": "17:00"}]);
    json!({
        "timezone": tz,
        "weekly": {"mon": w, "tue": w, "wed": w, "thu": w, "fri": w},
        "use_tenant_calendar": false,
        "out_of_hours": mode,
    })
}

/// Every day, all day, in `tz`.
fn always(tz: &str, use_tenant_calendar: bool) -> Value {
    let w = json!([{"from": "00:00", "to": "24:00"}]);
    json!({
        "timezone": tz,
        "weekly": {"mon": w, "tue": w, "wed": w, "thu": w, "fri": w, "sat": w, "sun": w},
        "use_tenant_calendar": use_tenant_calendar,
        "out_of_hours": "queue",
    })
}

async fn put_policy(c: &Ctx, body: Value) -> (StatusCode, Value) {
    c.call("PUT", "/api/jobs/j1/handover/policy", "u-mgr", body)
        .await
}

pub(crate) async fn save_hours(c: &Ctx, hours: Value) {
    let (s, b) = put_policy(c, json!({"targets": targets(), "hours": hours})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn save_calendar(c: &Ctx, sub: &str, body: Value) -> (StatusCode, Value) {
    c.call("PUT", "/api/tenant/calendar", sub, body).await
}

pub(crate) fn job(c: &Ctx) -> JobRef {
    JobRef {
        id: c.job,
        key: "j1".into(),
    }
}

/// A conversation `id` in `env` (owner pool) and a case on it (as agent_app): (case id, the case's design).
pub(crate) async fn case_in(c: &Ctx, id: &str, env: &str) -> (String, CaseDesign) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', 'cust-1')",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(id)
    .bind(env)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let conv = store::conversation(&mut tx, id).await.unwrap().unwrap();
    let d = store::design_of(&mut tx, c.job, "j1", &conv).await.unwrap();
    let case_id = store::create_case(&mut tx, c.job, &conv, "Help", &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (case_id, d)
}

/// `case.create{handover}` of `case_id` at `now`, through the runner's `agent_cases` on `db` (agent_app or
/// agent_runtime), committed.
async fn hand_over(
    c: &Ctx,
    db: &PgPool,
    case_id: &str,
    conv_id: &str,
    d: &CaseDesign,
    now: DateTime<Utc>,
) -> OpOutcome {
    let mut tx = store::begin_job(db, c.t, c.job).await.unwrap();
    let o = agent_cases::create_at(
        &mut tx,
        &job(c),
        case_id,
        conv_id,
        "handover",
        &Map::new(),
        d,
        now,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    o
}

/// The scope of `d` closed at `now`: its next open time (`None` = open), read on `db`.
async fn next_open(
    c: &Ctx,
    db: &PgPool,
    d: &CaseDesign,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let mut tx = store::begin_job(db, c.t, c.job).await.unwrap();
    let closed = agent_cases::closed_at(&mut tx, &job(c), d, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    closed.map(|x| x.next_open)
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct ItemRow {
    pub(crate) outside_hours: bool,
    pub(crate) clock_starts_at: Option<DateTime<Utc>>,
    pub(crate) queued_at: DateTime<Utc>,
    pub(crate) first_response_due: Option<DateTime<Utc>>,
    pub(crate) resolve_due: Option<DateTime<Utc>>,
    pub(crate) callback: Option<Value>,
    pub(crate) trigger: String,
}

pub(crate) async fn item_of(c: &Ctx, case_id: &str) -> ItemRow {
    sqlx::query_as(
        "SELECT outside_hours, clock_starts_at, queued_at, first_response_due, resolve_due, callback, trigger \
           FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(c.t)
    .bind(case_id)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

/// Release `cfg-2026-10-21.1` of job j1 as its live release, whose snapshot holds handover policy `policy` and
/// (when given) tenant calendar `calendar` (owner pool).
pub(crate) async fn release_live(c: &Ctx, policy: Value, calendar: Option<Value>) {
    let rel = "cfg-2026-10-21.1";
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $4, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'current', 'u-rm', now(),
               'u-rm', now(), ARRAY['handover_policy', 'tenant_calendar'])"#,
    )
    .bind(c.t)
    .bind(c.job)
    .bind(rel)
    .bind(format!("sha256:{}", "c".repeat(64)))
    .execute(&c.e.owner)
    .await
    .unwrap();
    let mut rows = vec![("handover_policy", "handover_policy", policy)];
    if let Some(cal) = calendar {
        rows.push(("tenant-calendar", "tenant_calendar", cal));
    }
    for (key, kind, v) in rows {
        sqlx::query(
            "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
             VALUES ($1, $2, $3, $4, $5, '{}', $6)",
        )
        .bind(c.t)
        .bind(c.job)
        .bind(rel)
        .bind(key)
        .bind(kind)
        .bind(json!({ "v": v }))
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(c.job)
        .bind(rel)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

/// A prod item on web chat `wc` (the customer asked for a person): its item id.
async fn queued(c: &Ctx, wc: &str) -> String {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["item_id"].as_str().unwrap().to_string()
}

// ---- the tenant_calendar design kind --------------------------------------------------------------------------------

#[tokio::test]
async fn tenant_calendar_design_contract() {
    let e = common::env().await;
    let samples = vec![(
        DraftRef::new(),
        json!({"timezone": "Europe/Amsterdam",
               "holidays": [{"date": "2026-12-25", "label": "Christmas", "closed": true}]}),
    )];
    design_contract::run_all(&e, "tenant_calendar", &samples, Some("no calendar")).await;
}

#[tokio::test]
async fn tenant_calendar_is_tenant_scoped_with_rls_and_exact_grants() {
    let c = conv::setup().await;
    let (rls, force): (bool, bool) = sqlx::query_as(
        "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = 'public.tenant_calendar'::regclass",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert!(rls && !force, "RLS on, not forced");
    let policies: Vec<String> = sqlx::query_scalar(
        "SELECT policyname::text FROM pg_policies WHERE schemaname = 'public' AND tablename = 'tenant_calendar'",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(policies, ["tenant_isolation"]);
    for (role, p, want) in [
        ("agent_app", "SELECT", true),
        ("agent_app", "INSERT", true),
        ("agent_app", "UPDATE", true),
        ("agent_app", "DELETE", true),
        ("agent_app", "TRUNCATE", false),
        ("agent_platform", "SELECT", false),
        ("agent_runtime", "SELECT", false),
        ("agent_runtime", "INSERT", false),
    ] {
        let has: bool = sqlx::query_scalar(
            "SELECT has_table_privilege($1::text::name, 'public.tenant_calendar', $2)",
        )
        .bind(role)
        .bind(p)
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
        assert_eq!(has, want, "{role} {p} on tenant_calendar");
    }
    // the runtime reads the value columns, never who changed them
    for (col, want) in [
        ("tenant_id", true),
        ("timezone", true),
        ("holidays", true),
        ("updated_by", false),
    ] {
        let has: bool = sqlx::query_scalar(
            "SELECT has_column_privilege('agent_runtime', 'public.tenant_calendar', $1, 'SELECT')",
        )
        .bind(col)
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
        assert_eq!(has, want, "agent_runtime SELECT ({col})");
    }
    // another tenant's calendar is invisible
    let other = uuid::Uuid::new_v4();
    common::insert_tenant(&c.e.owner, other, &format!("Other {other}")).await;
    for t in [c.t, other] {
        sqlx::query(
            "INSERT INTO tenant_calendar (tenant_id, timezone, holidays, updated_by) VALUES ($1, 'UTC', '[]', 'test')",
        )
        .bind(t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    let mut tx = agent_api::tenancy::begin_scoped(&c.e.state.db, c.t)
        .await
        .unwrap();
    let seen: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT tenant_id FROM tenant_calendar")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen, [c.t]);
}

#[tokio::test]
async fn calendar_save_is_a_tenant_draft_recorded_in_every_job() {
    let c = conv::setup().await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j2', 'J2', '[]', 'test')",
    )
    .bind(c.t)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, v) = c.get("/api/tenant/calendar", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"live": null, "draft": null, "drafted": []}));
    let revs_before = c
        .texts("SELECT draft_rev::text FROM jobs ORDER BY key")
        .await;

    let (s, v) = save_calendar(
        &c,
        "u-mgr",
        json!({"timezone": "Europe/Amsterdam", "holidays": [
            {"date": "2026-12-26", "label": "Boxing Day", "closed": true},
            {"date": "2026-12-25", "label": " Christmas "},
        ]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let draft = json!({"timezone": "Europe/Amsterdam", "holidays": [
        {"date": "2026-12-25", "label": "Christmas", "closed": true},
        {"date": "2026-12-26", "label": "Boxing Day", "closed": true},
    ]});
    assert_eq!(
        v,
        json!({"live": null, "draft": draft, "drafted": ["j1", "j2"]}),
        "a draft, never live on save; recorded in every job's draft"
    );
    let (_, read) = c.get("/api/tenant/calendar", "u-obs").await;
    assert_eq!(read, v, "a member reads the same view");
    assert_eq!(
        c.texts(
            "SELECT j.key || ':' || d.key FROM draft_ledger d JOIN jobs j ON j.id = d.job_id \
              WHERE d.kind = 'tenant_calendar' ORDER BY j.key"
        )
        .await,
        ["j1:tenant-calendar", "j2:tenant-calendar"]
    );
    let revs_after = c
        .texts("SELECT draft_rev::text FROM jobs ORDER BY key")
        .await;
    for (b, a) in revs_before.iter().zip(&revs_after) {
        assert!(
            a.parse::<i64>().unwrap() > b.parse::<i64>().unwrap(),
            "every job's draft_rev moved: {revs_before:?} -> {revs_after:?}"
        );
    }
    assert_eq!(
        c.texts(
            "SELECT details->>'kind' FROM audit_event WHERE action = 'tenant.draft_change' AND resource_type = 'calendar'"
        )
        .await,
        ["tenant_calendar"]
    );
    // the working row is the normalised value
    assert_eq!(
        c.texts("SELECT timezone || ' ' || jsonb_array_length(holidays) FROM tenant_calendar")
            .await,
        ["Europe/Amsterdam 2"]
    );
}

#[tokio::test]
async fn invalid_calendar_422() {
    let c = conv::setup().await;
    let many: Vec<Value> = (0..367)
        .map(|i| {
            let d = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + Duration::days(i);
            json!({"date": d.format("%Y-%m-%d").to_string()})
        })
        .collect();
    for (body, msg) in [
        (
            json!({"timezone": "Mars/Olympus_Mons", "holidays": []}),
            "unknown timezone",
        ),
        (
            json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "25-12-2026"}]}),
            "holiday 1: date must be YYYY-MM-DD",
        ),
        (
            json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-12-25"}, {"date": "2026-12-25"}]}),
            "2026-12-25 is listed twice",
        ),
        (
            json!({"timezone": "Europe/Amsterdam", "holidays": many}),
            "at most 366 holidays",
        ),
        (json!({"holidays": []}), "timezone is required"),
    ] {
        let (s, b) = save_calendar(&c, "u-mgr", body).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_calendar"),
            "{b}"
        );
        assert_eq!(b["error"]["message"], msg, "{b}");
    }
    assert!(
        c.texts("SELECT timezone FROM tenant_calendar")
            .await
            .is_empty(),
        "nothing was written"
    );
    assert!(
        c.texts("SELECT key FROM draft_ledger WHERE kind = 'tenant_calendar'")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn save_calendar_needs_support_manager() {
    let c = conv::setup().await;
    let body = json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-12-25"}]});
    for sub in ["u-agent", "u-obs", "u-int"] {
        let (s, b) = save_calendar(&c, sub, body.clone()).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{sub}: {b}"
        );
    }
    assert!(
        c.texts("SELECT timezone FROM tenant_calendar")
            .await
            .is_empty()
    );
    let (s, b) = save_calendar(&c, "u-mgr", body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---- business hours on the policy ----------------------------------------------------------------------------------

#[tokio::test]
async fn invalid_hours_is_422_invalid_policy_field_hours() {
    let c = conv::setup().await;
    let mut no_window = weekdays("Europe/Amsterdam", "queue");
    no_window["weekly"] = json!({});
    for (hours, msg) in [
        (json!("9 to 5"), "hours: must be an object or null"),
        (
            weekdays("Mars/Olympus_Mons", "queue"),
            "hours: unknown timezone",
        ),
        (no_window, "hours: no open window in the week"),
        (
            weekdays("Europe/Amsterdam", "voicemail"),
            "hours: out_of_hours must be callback or queue",
        ),
        (
            json!({"timezone": "UTC", "weekly": {"mon": [{"from": "17:00", "to": "09:00"}]}}),
            "hours: mon windows need from before to, HH:MM between 00:00 and 24:00",
        ),
    ] {
        let (s, b) = put_policy(&c, json!({"targets": targets(), "hours": hours})).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_policy"),
            "{b}"
        );
        assert_eq!(b["error"]["message"], msg, "{b}");
        assert_eq!(b["error"]["details"]["field"], "hours", "{b}");
    }
    assert!(
        c.texts("SELECT job_id::text FROM handover_policy")
            .await
            .is_empty(),
        "nothing was written"
    );
}

#[tokio::test]
async fn null_hours_is_always_open() {
    let c = conv::setup().await;
    let (_, d) = case_in(&c, "conv-t1", "test").await;
    let db = &c.e.state.db;
    let instants = [
        "2026-10-24T03:00:00Z", // a Saturday night
        "2026-10-21T22:00:00Z",
        "2026-12-25T12:00:00Z",
    ];
    // targets only: no hours, always staffed
    let (s, v) = put_policy(&c, json!({"targets": targets()})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["hours"], json!({"live": null, "draft": null}), "{v}");
    for t in instants {
        assert_eq!(next_open(&c, db, &d, at(t)).await, None, "{t}");
    }
    // hours: closed on Saturday; a later save without `hours` keeps them
    save_hours(&c, weekdays("Europe/Amsterdam", "queue")).await;
    let (s, v) = put_policy(&c, json!({"targets": targets()})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["hours"]["draft"]["timezone"], "Europe/Amsterdam", "{v}");
    assert!(next_open(&c, db, &d, at(instants[0])).await.is_some());
    // `hours: null` clears them: always open again
    let (s, v) = put_policy(&c, json!({"targets": targets(), "hours": null})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["hours"]["draft"], Value::Null, "{v}");
    for t in instants {
        assert_eq!(next_open(&c, db, &d, at(t)).await, None, "{t}");
    }
    let (case_id, d2) = case_in(&c, "conv-t2", "test").await;
    let o = hand_over(&c, db, &case_id, "conv-t2", &d2, at(instants[0])).await;
    assert_eq!((o.state, o.outcome_code.as_deref()), (OpState::Ok, None));
    assert!(!item_of(&c, &case_id).await.outside_hours);
}

// ---- out of hours on case.create -----------------------------------------------------------------------------------

#[tokio::test]
async fn closed_hours_set_outside_hours_and_clock_start() {
    let c = conv::setup().await;
    save_hours(&c, weekdays("Europe/Amsterdam", "queue")).await;
    let db = &c.e.state.db;
    // Wednesday 22:00 UTC = Thursday 00:00 in Amsterdam (CEST): closed until Thursday 09:00 = 07:00 UTC
    let (case_id, d) = case_in(&c, "conv-t1", "test").await;
    let o = hand_over(&c, db, &case_id, "conv-t1", &d, at("2026-10-21T22:00:00Z")).await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert_eq!(o.outcome_code.as_deref(), Some("outside_hours"));
    assert_eq!(o.output["outside_hours"], true);
    assert_eq!(o.output["out_of_hours"], "queue");
    assert_eq!(
        o.output["next_open_local"],
        "Thu 22 Oct 09:00 (Europe/Amsterdam)"
    );
    let opens = at("2026-10-22T07:00:00Z");
    assert_eq!(
        serde_json::from_value::<DateTime<Utc>>(o.output["next_open"].clone()).unwrap(),
        opens
    );
    let row = item_of(&c, &case_id).await;
    assert!(row.outside_hours);
    assert_eq!(row.clock_starts_at, Some(opens));
    assert_eq!(
        row.first_response_due,
        Some(opens + Duration::minutes(60)),
        "the SLA clocks start at the next open time (normal: 60 / 1440 min)"
    );
    assert_eq!(row.resolve_due, Some(opens + Duration::minutes(1440)));
    assert_eq!(row.callback, None, "queue mode offers no call-back");
    assert_eq!(row.trigger, "case.create{handover}, outside hours");
    // the queue shows it
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let h = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["case_ref"] == case_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(h["outside_hours"], true, "{h}");
    assert!(h["clock_starts_at"].is_string(), "{h}");
    assert_eq!(h["callback"], Value::Null, "{h}");

    // Thursday 10:00 in Amsterdam: open, the clocks start now
    let (open_case, d2) = case_in(&c, "conv-t2", "test").await;
    let o = hand_over(
        &c,
        db,
        &open_case,
        "conv-t2",
        &d2,
        at("2026-10-22T08:00:00Z"),
    )
    .await;
    assert_eq!((o.state, o.outcome_code.as_deref()), (OpState::Ok, None));
    assert!(o.output.get("outside_hours").is_none(), "{o:?}");
    let row = item_of(&c, &open_case).await;
    assert!(!row.outside_hours);
    assert_eq!(row.clock_starts_at, None);
    assert_eq!(
        row.first_response_due,
        Some(row.queued_at + Duration::minutes(60))
    );

    // Saturday 00:00 CEST: the next open time is Monday 09:00 CET, after the DST change (08:00 UTC)
    assert_eq!(
        next_open(&c, db, &d, at("2026-10-24T22:00:00Z")).await,
        Some(at("2026-10-26T08:00:00Z"))
    );
}

#[tokio::test]
async fn callback_text_names_next_open_in_policy_timezone() {
    let c = conv::setup().await;
    save_hours(&c, weekdays("America/New_York", "callback")).await;
    let (case_id, d) = case_in(&c, "conv-t1", "test").await;
    // the database session's timezone is not the policy's: it must not matter
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    sqlx::query("SET LOCAL TIME ZONE 'Asia/Tokyo'")
        .execute(&mut *tx)
        .await
        .unwrap();
    // Saturday 12:00 UTC = 08:00 in New York (EDT): closed until Monday 09:00 EDT = 13:00 UTC
    let now = at("2026-10-24T12:00:00Z");
    let o = agent_cases::create_at(
        &mut tx,
        &job(&c),
        &case_id,
        "conv-t1",
        "handover",
        &Map::new(),
        &d,
        now,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(o.outcome_code.as_deref(), Some("outside_hours"), "{o:?}");
    assert_eq!(
        o.output["next_open_local"],
        "Mon 26 Oct 09:00 (America/New_York)"
    );
    let row = item_of(&c, &case_id).await;
    assert_eq!(row.clock_starts_at, Some(at("2026-10-26T13:00:00Z")));
    let cb = row.callback.expect("a call-back is offered");
    assert_eq!(
        serde_json::from_value::<DateTime<Utc>>(cb["after"].clone()).unwrap(),
        at("2026-10-26T13:00:00Z")
    );
    // the interaction daemon's reply for the resolved (handed over) case
    let r: Resolution = serde_json::from_value(json!({
        "case_id": case_id, "conversation_id": "conv-t1", "context_space": "retail:order-support",
        "outcome": "handover", "code": "handed_over",
        "facts": {"hand_over": Value::Object(o.output.clone())},
    }))
    .unwrap();
    assert_eq!(
        reply::compose(&r),
        "We're closed right now. A colleague will call you back after Mon 26 Oct 09:00 (America/New_York)."
    );
}

#[tokio::test]
async fn holiday_from_tenant_calendar_is_closed_all_day() {
    let c = conv::setup().await;
    save_hours(&c, always("Europe/Amsterdam", true)).await;
    let (s, b) = save_calendar(
        &c,
        "u-mgr",
        json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-12-25", "label": "Christmas"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, d) = case_in(&c, "conv-t1", "test").await;
    let db = &c.e.state.db;
    // Christmas is closed all day in the policy's timezone; it opens at 00:00 on the 26th (23:00 UTC)
    let opens = Some(at("2026-12-25T23:00:00Z"));
    assert_eq!(
        next_open(&c, db, &d, at("2026-12-25T12:00:00Z")).await,
        opens
    );
    assert_eq!(
        next_open(&c, db, &d, at("2026-12-24T23:30:00Z")).await,
        opens,
        "00:30 on the 25th in Amsterdam is the holiday, while UTC is still the 24th"
    );
    assert_eq!(
        next_open(&c, db, &d, at("2026-12-24T22:30:00Z")).await,
        None,
        "23:30 on the 24th in Amsterdam is open"
    );
    assert_eq!(
        next_open(&c, db, &d, at("2026-12-25T23:00:00Z")).await,
        None
    );
    // hours that do not use the tenant calendar ignore its holidays
    save_hours(&c, always("Europe/Amsterdam", false)).await;
    assert_eq!(
        next_open(&c, db, &d, at("2026-12-25T12:00:00Z")).await,
        None
    );
}

#[tokio::test]
async fn test_env_uses_draft_hours_prod_uses_live() {
    let c = conv::setup().await;
    // live (the release snapshot): weekdays 09:00–17:00, no calendar
    release_live(
        &c,
        json!({"targets": targets(), "hours": weekdays("Europe/Amsterdam", "queue")}),
        None,
    )
    .await;
    // draft: always open, but the tenant calendar (draft) closes Friday 23 October
    save_hours(&c, always("Europe/Amsterdam", true)).await;
    let (s, b) = save_calendar(
        &c,
        "u-mgr",
        json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-10-23"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, v) = c.get("/api/jobs/j1/handover/policy", "u-obs").await;
    assert_eq!(v["hours"]["live"]["use_tenant_calendar"], false, "{v}");
    assert_eq!(v["hours"]["draft"]["use_tenant_calendar"], true, "{v}");

    let (prod_case, prod) = case_in(&c, "conv-p1", "prod").await;
    let (_, test) = case_in(&c, "conv-t1", "test").await;
    assert!(matches!(prod, CaseDesign::Live { .. }), "{prod:?}");
    assert!(matches!(test, CaseDesign::Draft { .. }), "{test:?}");
    // the runner reads both as agent_runtime (0066: SELECT on the calendar's value columns)
    let rt = common::runtime_pool(&c.e.owner).await;
    let thursday_night = at("2026-10-21T22:00:00Z");
    let friday_noon = at("2026-10-23T10:00:00Z");
    assert_eq!(
        next_open(&c, &rt, &prod, thursday_night).await,
        Some(at("2026-10-22T07:00:00Z")),
        "prod runs the live weekday hours"
    );
    assert_eq!(
        next_open(&c, &rt, &test, thursday_night).await,
        None,
        "test runs the draft: always open"
    );
    assert_eq!(
        next_open(&c, &rt, &test, friday_noon).await,
        Some(at("2026-10-23T22:00:00Z")),
        "the draft calendar's holiday closes the test scope until Saturday 00:00"
    );
    assert_eq!(
        next_open(&c, &rt, &prod, friday_noon).await,
        None,
        "live hours do not use the calendar"
    );
    // a prod handover queued by the runner out of hours
    let o = hand_over(&c, &rt, &prod_case, "conv-p1", &prod, thursday_night).await;
    assert_eq!(o.outcome_code.as_deref(), Some("outside_hours"), "{o:?}");
    let row = item_of(&c, &prod_case).await;
    assert!(row.outside_hours);
    assert_eq!(row.clock_starts_at, Some(at("2026-10-22T07:00:00Z")));
}

// ---- the queue order -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn take_next_skips_items_whose_clock_has_not_started() {
    let c = conv::setup().await;
    let waiting = queued(&c, "wc-1").await;
    let started = queued(&c, "wc-2").await;
    // `waiting` was raised out of hours: high priority, but its clock starts in two hours
    sqlx::query(
        "UPDATE agent_handover_items SET priority = 'high', outside_hours = true, \
                clock_starts_at = now() + interval '2 hours', first_response_due = now() + interval '3 hours' \
          WHERE tenant_id = $1 AND id = $2",
    )
    .bind(c.t)
    .bind(waiting.as_str())
    .execute(&c.e.owner)
    .await
    .unwrap();
    // the queue order (`handover::next_unassigned`), each taken in turn (SW T2 Take)
    let mut taken = Vec::new();
    for _ in 0..2 {
        let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
        let it = agent_api::conversations::handover::next_unassigned(&mut tx)
            .await
            .unwrap()
            .expect("an unassigned item");
        tx.commit().await.unwrap();
        let (s, b) = c.take(&it.conversation_id, "u-agent", "queue").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        taken.push(it.id);
    }
    assert_eq!(
        taken,
        [started, waiting],
        "an item whose clock has not started comes after every started item"
    );
}
