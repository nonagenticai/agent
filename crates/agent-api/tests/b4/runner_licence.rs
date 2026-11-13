//! B5 runner step 2 — the licence gate (crate::runtime::runner::licence) over real Postgres 18 + Redis, the runner
//! connected as `agent_runtime`: a licensed tenant's step runs; a tenant whose verdict is not a fresh `active` gets
//! `denied` / `licence_inactive` with zero connector calls, a `tool_failed` handover and the audit rows; the decision
//! is remembered for a bounded time only; the gate is off while `LICENSE_GATE_2_ENABLED` is off.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::OpState as ResultState;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::licence::LicenceCache;
use agent_api::runtime::runner::ops::Executors;
use serde_json::{Value, json};

use crate::b4::flow::Fx;
use crate::b4::runner_checks::{Fake, P_OP, call, case_with_conversation, setup};
use crate::redis_env as r;

/// A runner for the fixture's scope as `agent_runtime`, gate 2 `enabled` or not, with the licence `cache`
/// (`None`: the default).
fn runner(fx: &Fx, fake: &Arc<Fake>, enabled: bool, cache: Option<LicenceCache>) -> Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let mut cfg = (*st.cfg).clone();
    cfg.license_gate_2_enabled = enabled;
    st.cfg = Arc::new(cfg);
    let rn = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-licence-test")
        .with_consume(r::fast())
        .with_executors(Executors::default().register(P_OP, fake.clone()));
    match cache {
        Some(c) => rn.with_licence_cache(c),
        None => rn,
    }
}

/// A cache on a clock the test advances by hand: no real waiting.
fn manual_cache(ttl: Duration) -> (LicenceCache, Arc<AtomicU64>) {
    let base = Instant::now();
    let ms = Arc::new(AtomicU64::new(0));
    let m = Arc::clone(&ms);
    let clock = Arc::new(move || base + Duration::from_millis(m.load(Ordering::SeqCst)));
    (LicenceCache::with_clock(ttl, clock), ms)
}

fn advance(ms: &AtomicU64, by: Duration) {
    ms.fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
}

/// Overwrite the tenant's verdict: `status`, `reason`, checked `age` seconds ago; `None` status deletes it.
async fn verdict(fx: &Fx, status: Option<&str>, reason: Option<&str>, age: f64) {
    let t = fx.d.f.tenant;
    sqlx::query("DELETE FROM tenant_licence_verdict WHERE tenant_id = $1")
        .bind(t)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    if let Some(status) = status {
        sqlx::query(
            "INSERT INTO tenant_licence_verdict (tenant_id, status, reason, licence_id, checked_at) \
             VALUES ($1, $2, $3, 'lic-x', now() - make_interval(secs => $4))",
        )
        .bind(t)
        .bind(status)
        .bind(reason)
        .bind(age)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    }
}

/// A fresh case with a P-operation call (step `s1`) that is confirmed.
async fn launch(fx: &Fx, case: &str) -> agent_api::runtime::domain::OpCall {
    let msg = case_with_conversation(fx, case).await;
    call(fx, case, "s1", P_OP, Some(&msg), json!({}))
}

#[tokio::test]
async fn licensed_tenant_step_runs() {
    let fx = setup("licence-ok").await;
    verdict(&fx, Some("active"), None, 0.0).await;
    let fake = Fake::ok(json!({"status": "launched"}));
    let rn = runner(&fx, &fake, true, None);
    let c = launch(&fx, "CS-1").await;
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(fake.hits(), 1);

    // nonagentic's own tenant (D70) needs no verdict
    verdict(&fx, None, None, 0.0).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(fx.d.f.tenant)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    let c = launch(&fx, "CS-2").await;
    let res = runner(&fx, &fake, true, None).handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(fake.hits(), 2);
}

#[tokio::test]
async fn unlicensed_tenant_step_is_not_executed_and_hands_over() {
    let fx = setup("licence-refused").await;
    let fake = Fake::ok(json!({"status": "launched"}));
    let scenarios: [(&str, Option<&str>, Option<&str>, f64, &str); 6] = [
        ("CS-1", Some("revoked"), None, 0.0, "revoked"),
        ("CS-2", Some("expired"), None, 0.0, "expired"),
        (
            "CS-3",
            Some("inactive"),
            Some("tenant_suspended"),
            0.0,
            "suspended",
        ),
        (
            "CS-4",
            Some("inactive"),
            Some("license_unavailable"),
            0.0,
            "unavailable",
        ),
        ("CS-5", Some("active"), None, 400.0, "stale"),
        ("CS-6", None, None, 0.0, "missing"),
    ];
    for (case, status, reason, age, state) in scenarios {
        verdict(&fx, status, reason, age).await;
        let c = launch(&fx, case).await;
        // a fresh runner per scenario: nothing is remembered
        let res = runner(&fx, &fake, true, None).handle(&c).await.unwrap();
        assert_eq!(res.state, ResultState::Denied, "{case}: {res:?}");
        assert_eq!(
            res.outcome_code.as_deref(),
            Some("licence_inactive"),
            "{case}"
        );
        assert!(!res.retryable, "{case}");

        let (st, code, detail): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT state, outcome_code, detail FROM operation_call WHERE tenant_id = $1 AND call_id = $2",
        )
        .bind(fx.d.f.tenant)
        .bind(&c.call_id)
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap();
        assert_eq!(
            (st.as_str(), code.as_deref(), detail.as_deref()),
            (
                "denied",
                Some("licence_inactive"),
                Some(format!("licence {state}").as_str())
            ),
            "{case}"
        );

        // the case hands over (tool_failed), its outcome the code
        let (status, outcome): (String, Option<String>) = sqlx::query_as(
            "SELECT status, outcome FROM agent_case WHERE tenant_id = $1 AND case_id = $2",
        )
        .bind(fx.d.f.tenant)
        .bind(case)
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap();
        assert_eq!(
            (status.as_str(), outcome.as_deref()),
            ("handover", Some("licence_inactive")),
            "{case}"
        );
        let items: Vec<(String, String)> = sqlx::query_as(
            "SELECT reason, trigger FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2",
        )
        .bind(fx.d.f.tenant)
        .bind(case)
        .fetch_all(&fx.d.e.owner)
        .await
        .unwrap();
        assert_eq!(
            items,
            [(
                "tool_failed".to_owned(),
                format!("licence inactive for {P_OP}")
            )],
            "{case}"
        );

        // the audit rows: the call's `execute` and the plan's `operation.denied`
        for action in ["execute", "operation.denied"] {
            let (st, details): (String, Value) = sqlx::query_as(
                "SELECT status, details FROM audit_event \
                  WHERE tenant_id = $1 AND action = $2 AND resource_id = $3",
            )
            .bind(fx.d.f.tenant)
            .bind(action)
            .bind(&c.call_id)
            .fetch_one(&fx.d.e.owner)
            .await
            .unwrap();
            assert_eq!(st, "denied", "{case} {action}");
            assert_eq!(details["code"], "licence_inactive", "{case} {action}");
        }
    }
    assert_eq!(
        fake.hits(),
        0,
        "no connector was called for an unlicensed tenant"
    );
}

#[tokio::test]
async fn gate_off_does_not_look_at_the_verdict() {
    let fx = setup("licence-off").await;
    verdict(&fx, Some("revoked"), None, 0.0).await;
    let fake = Fake::ok(json!({"status": "launched"}));
    let c = launch(&fx, "CS-1").await;
    let res = runner(&fx, &fake, false, None).handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(fake.hits(), 1);
}

#[tokio::test]
async fn decision_is_remembered_for_the_ttl_only() {
    let fx = setup("licence-ttl").await;
    verdict(&fx, Some("active"), None, 0.0).await;
    let fake = Fake::ok(json!({"status": "launched"}));
    let (cache, clock) = manual_cache(Duration::from_secs(30));
    let rn = runner(&fx, &fake, true, Some(cache));
    let c = launch(&fx, "CS-1").await;
    assert_eq!(rn.handle(&c).await.unwrap().state, ResultState::Ok);

    // revoked now: 29 s later the remembered `active` still admits (no query per step) ...
    verdict(&fx, Some("revoked"), None, 0.0).await;
    advance(&clock, Duration::from_secs(29));
    let c = launch(&fx, "CS-2").await;
    assert_eq!(
        rn.handle(&c).await.unwrap().state,
        ResultState::Ok,
        "remembered"
    );
    assert_eq!(fake.hits(), 2);

    // ... and at 30 s the revocation takes hold
    advance(&clock, Duration::from_secs(1));
    let c = launch(&fx, "CS-3").await;
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Denied, "{res:?}");
    assert_eq!(res.outcome_code.as_deref(), Some("licence_inactive"));
    assert_eq!(fake.hits(), 2);

    // and the other way: a refusal is remembered too, a restored licence is seen after the TTL
    verdict(&fx, Some("active"), None, 0.0).await;
    advance(&clock, Duration::from_secs(29));
    let c = launch(&fx, "CS-4").await;
    assert_eq!(
        rn.handle(&c).await.unwrap().state,
        ResultState::Denied,
        "remembered refusal"
    );
    advance(&clock, Duration::from_secs(1));
    let c = launch(&fx, "CS-5").await;
    assert_eq!(rn.handle(&c).await.unwrap().state, ResultState::Ok);
    assert_eq!(fake.hits(), 3);
}

#[tokio::test]
async fn remembered_active_never_outlives_the_verdict_freshness() {
    let fx = setup("licence-fresh").await;
    // 290 s old: ten seconds of freshness left, however long the (30 s) TTL
    verdict(&fx, Some("active"), None, 290.0).await;
    let fake = Fake::ok(json!({"status": "launched"}));
    let (cache, clock) = manual_cache(Duration::from_secs(30));
    let rn = runner(&fx, &fake, true, Some(cache));
    let c = launch(&fx, "CS-1").await;
    assert_eq!(rn.handle(&c).await.unwrap().state, ResultState::Ok);

    // 5 s later (inside the freshness): remembered, even though the row has meanwhile been rewritten as stale
    verdict(&fx, Some("active"), None, 400.0).await;
    advance(&clock, Duration::from_secs(5));
    let c = launch(&fx, "CS-2").await;
    assert_eq!(
        rn.handle(&c).await.unwrap().state,
        ResultState::Ok,
        "remembered"
    );

    // 6 s more: the verdict's own freshness (10 s) is over, well before the 30 s TTL, so the row is read again
    advance(&clock, Duration::from_secs(6));
    let c = launch(&fx, "CS-3").await;
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(
        res.state,
        ResultState::Denied,
        "the verdict went stale: {res:?}"
    );
    assert_eq!(fake.hits(), 2);
}
