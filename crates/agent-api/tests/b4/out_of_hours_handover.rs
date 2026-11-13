//! B4 T16a (F-14, acceptance A15): every handover item the agent raises is out-of-hours aware, not only
//! `case.create{handover}`. Live A15 FAIL: a test-env late-delivery chat handed over by the orchestrator's
//! success-criteria path while the draft hours were closed queued its item `outside_hours = false` with the SLA due
//! from now, and the customer read "I've passed this to a colleague, who will take it from here." (no call-back, no
//! next open time). Here: the orchestrator's hand-over (success criteria unmet) and failed-case (`queue_failed`)
//! items while closed and while open, and the hours source of `agent_cases::queue` (the live policy in prod, the
//! draft in test, plan R8). Real Postgres 18 + Redis (the `b4` binary's containers, a fresh database per test).
//!
//! "Closed" is built at the database's real `now()`: hours open every day all day in UTC, the tenant calendar
//! closing today and tomorrow (UTC). A run crossing midnight is still on a holiday, so the next open time is always
//! 00:00 UTC the day after tomorrow.
use std::sync::Arc;

use agent_api::conversations::store;
use agent_api::runtime::Env;
use agent_api::runtime::domain::{CaseOutcome, OpCall, OpState, QualifiedRequest};
use agent_api::runtime::orchestrator::{SUCCESS_CRITERIA_UNMET, StaticProcedures};
use agent_api::runtime::reply;
use agent_api::runtime::routes::{ResolvedTarget, RouteQuery, RouteResolver};
use agent_api::runtime::runner::agent_cases::{self, Queued};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::b4::flow::{
    Fx, answer, calls, current_design, insert_case, orch, qualified, qualify, resolutions,
    resolutions_in, setup_latest,
};
use crate::b4::hours as h;
use crate::common::{self, conv};

/// The template's delivery-problem space (area `logistics`, procedure `delivery_problem@1`).
const DELIVERY_AREA: &str = "logistics";

/// What the customer reads for a handover queued while open.
const PLAIN_HANDOVER: &str = "I've passed this to a colleague, who will take it from here.";

/// Open every day, all day, in UTC; closed on the tenant calendar's holidays when `use_tenant_calendar`.
fn always_utc(use_tenant_calendar: bool, mode: &str) -> Value {
    let w = json!([{"from": "00:00", "to": "24:00"}]);
    json!({
        "timezone": "UTC",
        "weekly": {"mon": w, "tue": w, "wed": w, "thu": w, "fri": w, "sat": w, "sun": w},
        "use_tenant_calendar": use_tenant_calendar,
        "out_of_hours": mode,
    })
}

/// Today in UTC on the database's clock.
async fn utc_today(db: &PgPool) -> NaiveDate {
    sqlx::query_scalar("SELECT (now() AT TIME ZONE 'UTC')::date")
        .fetch_one(db)
        .await
        .unwrap()
}

/// A UTC calendar closing `today` and the day after.
fn two_holidays(today: NaiveDate) -> Value {
    let d = |n: i64| (today + Duration::days(n)).format("%Y-%m-%d").to_string();
    json!({"timezone": "UTC", "holidays": [{"date": d(0), "label": "Closed"}, {"date": d(1), "label": "Closed"}]})
}

/// The next open time of [`two_holidays`]: 00:00 UTC the day after tomorrow.
fn opens_after(today: NaiveDate) -> DateTime<Utc> {
    (today + Duration::days(2))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}

/// `next_open` as `crate::handover::hours::local_text` writes it for the UTC policy.
fn local(next_open: DateTime<Utc>) -> String {
    format!("{} (UTC)", next_open.format("%a %-d %b %H:%M"))
}

/// The fixture job's draft (test env) hours, saved through the API by a `support_manager`: always open; with
/// `closed`, they use the tenant calendar, whose draft closes today and tomorrow. Returns the next open time.
async fn draft_hours(fx: &Fx, closed: bool, mode: &str) -> DateTime<Utc> {
    let sub = format!("u-ooh-mgr-{}", fx.d.f.tenant.simple());
    common::insert_user(&fx.d.e.owner, &sub, fx.d.f.tenant, "support_manager").await;
    let tok = common::person(&sub, &[]);
    let today = utc_today(&fx.d.e.owner).await;
    let (s, b) = common::send(
        &fx.d.e.app,
        common::json(
            "PUT",
            "/api/tenant/calendar",
            Some(&tok),
            two_holidays(today),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = common::send(
        &fx.d.e.app,
        common::json(
            "PUT",
            &format!("/api/jobs/{}/handover/policy", fx.d.f.job.job_key),
            Some(&tok),
            json!({"targets": h::targets(), "hours": always_utc(closed, mode)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    opens_after(today)
}

/// The staff conversation `conv` of the fixture's job (the J21 item's).
async fn insert_conversation(fx: &Fx, conv: &str) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', 'C-10482')",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

/// The J21 item of `case` (owner pool).
async fn item(fx: &Fx, case: &str) -> h::ItemRow {
    sqlx::query_as(
        "SELECT outside_hours, clock_starts_at, queued_at, first_response_due, resolve_due, callback, trigger \
           FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// Every operation is offered (an instance): the delivery flow's `tool_requirements` pass (as tests/b4/flow.rs).
struct EveryOpOffered;

impl RouteResolver for EveryOpOffered {
    fn resolve<'a>(
        &'a self,
        _c: &'a mut sqlx::PgConnection,
        _q: &'a RouteQuery,
    ) -> futures_util::future::BoxFuture<'a, agent_api::error::ApiResult<ResolvedTarget>> {
        Box::pin(async {
            Ok(ResolvedTarget::Instance {
                instance_id: "every-op".to_owned(),
            })
        })
    }
}

/// A late-delivery complaint for `case` (copied from tests/b4/flow.rs, private there).
fn delivery_request(case: &str) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": format!("conv-{case}"), "area": DELIVERY_AREA,
        "context_space": "retail:logistics:delivery_problem", "request_type": "delivery_problem",
        "slots": {"order_id": "ORD-58790", "problem": "late"}, "customer_id": "C-10482",
        "identity_level": 2, "message_id": format!("msg-{case}")
    }))
    .unwrap()
}

/// The fake runner's output per operation of the delivery flow: the carrier reports a problem.
fn late_delivery_output(call: &OpCall) -> Value {
    match call.operation.as_str() {
        "order.read" => json!({"status": "shipped", "lines": [{"sku": "SKU-1", "qty": 1}]}),
        "delivery.read" => json!({"status": "problem", "carrier": "PostNL", "tracking_ref": "3S-1",
                                  "expected_window": {"from": "2026-10-21", "until": "2026-10-23"}}),
        "delivery.report_problem" => json!({"case_id": "DP-1", "status": "open"}),
        _ => json!({}),
    }
}

/// The delivery flow of `case` to its end (acceptance A4: success criteria unmet, the orchestrator's `hand_over`).
async fn run_late_delivery(fx: &Fx, case: &str) -> Vec<agent_api::runtime::domain::Resolution> {
    insert_case(fx, case, &current_design(fx).await).await;
    insert_conversation(fx, &format!("conv-{case}")).await;
    let o = orch(fx, StaticProcedures).with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(qualified(&delivery_request(case)))
        .await
        .unwrap();
    for _ in 0..3 {
        let call = calls(fx, case).await.pop().unwrap();
        let out = late_delivery_output(&call);
        o.on_result(answer(&call, OpState::Ok, "ok", out))
            .await
            .unwrap();
    }
    resolutions_in(fx, DELIVERY_AREA, case).await
}

/// A returns case `case` whose first call times out: the case ends `failed` and `queue_failed` queues its item.
async fn run_failed(fx: &Fx, case: &str) -> Vec<agent_api::runtime::domain::Resolution> {
    insert_conversation(fx, &format!("conv-{case}")).await;
    let o = orch(fx, StaticProcedures);
    qualify(fx, &o, case).await;
    let call = calls(fx, case).await[0].clone();
    o.on_result(answer(&call, OpState::Error, "timeout", Value::Null))
        .await
        .unwrap();
    resolutions(fx, case).await
}

/// A15 (the live defect): the success-criteria hand-over while the draft hours are closed queues the item
/// `outside_hours`, its clocks from the next open time, and the customer is offered a call-back naming it.
/// Unfixed, `hand_over` → `agent_cases::queue` passed no out-of-hours stamp (`outside_hours = false`,
/// `clock_starts_at = NULL`, due from now) and `finish` put no out-of-hours note in the resolution, so the reply
/// was the plain handover text: every assertion below on the item and the reply fails.
#[tokio::test]
async fn a_success_criteria_handover_while_closed_is_outside_hours_with_a_call_back() {
    let fx = setup_latest("ooh-handover-closed", Env::Test).await;
    let opens = draft_hours(&fx, true, "callback").await;
    let rs = run_late_delivery(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some(SUCCESS_CRITERIA_UNMET));

    let row = item(&fx, "CS-1").await;
    assert!(row.outside_hours, "{row:?}");
    assert_eq!(row.clock_starts_at, Some(opens), "{row:?}");
    assert_eq!(
        row.first_response_due,
        Some(opens + Duration::minutes(60)),
        "the SLA clocks start at the next open time (normal: 60 / 1440 min)"
    );
    assert_eq!(row.resolve_due, Some(opens + Duration::minutes(1440)));
    let cb = row.callback.expect("callback mode offers a call-back");
    assert_eq!(
        serde_json::from_value::<DateTime<Utc>>(cb["after"].clone()).unwrap(),
        opens
    );
    assert_eq!(
        row.trigger,
        "success criteria unmet: delivery_on_track, outside hours"
    );

    let when = local(opens);
    let note = rs[0]
        .out_of_hours
        .as_ref()
        .expect("the resolution carries the note");
    assert_eq!(
        (
            note.next_open,
            note.next_open_local.as_str(),
            note.out_of_hours.as_str()
        ),
        (opens, when.as_str(), "callback")
    );
    assert!(
        !rs[0]
            .facts
            .values()
            .any(|v| v.get("outside_hours").is_some()),
        "the note is not a step output (the reply gate's operations are the facts keys)"
    );
    assert_eq!(
        reply::compose(&rs[0]),
        format!("We're closed right now. A colleague will call you back after {when}.")
    );
}

/// A failed case's item (`queue_failed`) while closed is `outside_hours` too, its clocks from the next open time.
/// Its reply names the next open time instead of "a colleague will follow up by email today". Unfixed,
/// `queue_failed` → `agent_cases::queue` queued it in hours and the reply promised an email today.
#[tokio::test]
async fn a_failed_case_while_closed_queues_its_item_outside_hours() {
    let fx = setup_latest("ooh-failed-closed", Env::Test).await;
    let opens = draft_hours(&fx, true, "queue").await;
    let rs = run_failed(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed, "{rs:?}");
    let row = item(&fx, "CS-1").await;
    assert!(row.outside_hours, "{row:?}");
    assert_eq!(row.clock_starts_at, Some(opens), "{row:?}");
    assert_eq!(row.first_response_due, Some(opens + Duration::minutes(60)));
    assert_eq!(row.callback, None, "queue mode offers no call-back");
    assert!(row.trigger.ends_with(", outside hours"), "{row:?}");
    // the reply says when a colleague follows up, not "by email today" (a holiday is not "today")
    assert_eq!(
        rs[0].out_of_hours.as_ref().map(|n| n.next_open),
        Some(opens),
        "{rs:?}"
    );
    assert_eq!(
        reply::compose(&rs[0]),
        format!(
            "I couldn't finish this automatically: {}. Our team is closed right now; a colleague will reply \
             after {}.",
            reply::failure_cause(rs[0].code.as_deref()),
            local(opens)
        )
    );
}

/// No regression: the same two paths while open (the calendar's holidays are set but the hours do not use it)
/// queue in hours, clocks from now, and the replies are the plain texts with no out-of-hours note.
#[tokio::test]
async fn the_same_paths_while_open_queue_in_hours() {
    let fx = setup_latest("ooh-open", Env::Test).await;
    draft_hours(&fx, false, "callback").await;
    let rs = run_late_delivery(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    let row = item(&fx, "CS-1").await;
    assert!(!row.outside_hours, "{row:?}");
    assert_eq!(row.clock_starts_at, None);
    assert_eq!(row.callback, None);
    assert_eq!(
        row.first_response_due,
        Some(row.queued_at + Duration::minutes(60))
    );
    assert_eq!(row.trigger, "success criteria unmet: delivery_on_track");
    assert_eq!(rs[0].out_of_hours, None, "{rs:?}");
    assert_eq!(reply::compose(&rs[0]), PLAIN_HANDOVER);

    let rs = run_failed(&fx, "CS-2").await;
    assert_eq!(rs[0].outcome, CaseOutcome::Failed, "{rs:?}");
    let row = item(&fx, "CS-2").await;
    assert!(!row.outside_hours, "{row:?}");
    assert_eq!(row.clock_starts_at, None);
    assert_eq!(rs[0].out_of_hours, None, "{rs:?}");
    assert!(
        reply::compose(&rs[0]).ends_with("A colleague will follow up here today."),
        "{rs:?}"
    );
}

/// Plan R8 for every agent-raised item (`agent_cases::queue`, as agent_runtime): a prod case reads the LIVE hours
/// (the release snapshot: closed on the live calendar's holidays), a test case the DRAFT (always open).
#[tokio::test]
async fn queue_reads_live_hours_in_prod_and_the_draft_in_test() {
    let c = conv::setup().await;
    let today = utc_today(&c.e.owner).await;
    h::release_live(
        &c,
        json!({"targets": h::targets(), "hours": always_utc(true, "callback")}),
        Some(two_holidays(today)),
    )
    .await;
    h::save_hours(&c, always_utc(false, "callback")).await;
    let (prod_case, prod) = h::case_in(&c, "conv-p1", "prod").await;
    let (test_case, test) = h::case_in(&c, "conv-t1", "test").await;
    let rt = common::runtime_pool(&c.e.owner).await;
    for (case, conv_id, d) in [
        (&prod_case, "conv-p1", &prod),
        (&test_case, "conv-t1", &test),
    ] {
        let mut tx = store::begin_job(&rt, c.t, c.job).await.unwrap();
        let q = agent_cases::queue(
            &mut tx,
            &h::job(&c),
            case,
            conv_id,
            "tool_failed",
            "case failed: timeout",
            "Help",
            d,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(q, Queued::Created(_)), "{q:?}");
    }
    let p = h::item_of(&c, &prod_case).await;
    assert!(
        p.outside_hours && p.clock_starts_at == Some(opens_after(today)),
        "prod reads the live hours: {p:?}"
    );
    let t = h::item_of(&c, &test_case).await;
    assert!(
        !t.outside_hours && t.clock_starts_at.is_none(),
        "test reads the draft: {t:?}"
    );
}
