//! B2 T9: business rules (J9) — `GET /api/jobs/{job}/rules`, `PUT …/rules/{id}`, `PUT …/rules/{id}/passage`,
//! `GET …/rules/passages` and the `POST …/rules/{id}/evaluate` dry run (crate::release::rules, rules_eval).
//!
//! The job is created from a template whose industry layer authors the rule slots RET-01, WAR-01 and PAY-04 as
//! `rule_slot` rows (`rules_support::template`; the evaluator decides by those ids).
//!
//! `GOLDENS` were produced by the fixture evaluator itself (`web/src/lib/release/J9-evaluate.ts` `evaluateJ9`, run
//! under node over the same slot definitions, `rules_support/retail_rules.json`): every example of every rule slot, with the fixture world's values
//! (`web/src/lib/fixtures/release.ts`) on `FIXTURE_DAY`, plus edge cases for the rules without examples.
use crate::common;
use crate::rules_support;

use std::collections::BTreeMap;

use agent_api::release::rules_eval;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::design_contract::ContractJob;
use common::{TestEnv, get, insert_user, json as req, person, send};

/// The fixture's day (`web/src/lib/fixtures/release.ts` `FIXTURE_DAY`).
const FIXTURE_DAY: &str = "2026-10-18";
const DESIGNER: &str = "u-rules-designer";
const OBSERVER: &str = "u-rules-obs";

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

impl Fx {
    fn job(&self) -> Uuid {
        self.f.job.job_id
    }
    fn path(&self, rest: &str) -> String {
        format!("/api/jobs/{}/rules{rest}", self.f.job.job_key)
    }
    fn db(&self) -> &PgPool {
        &self.e.owner
    }
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = rules_support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, f.tenant, "tenant_user").await;
    Fx { e, f }
}

async fn list(fx: &Fx, sub: &str) -> Vec<Value> {
    let (s, b) = send(&fx.e.app, get(&fx.path(""), Some(&person(sub, &[])))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["next"].is_null(), "{b}");
    b["items"].as_array().expect("Paged.items").clone()
}

fn rule<'v>(items: &'v [Value], id: &str) -> &'v Value {
    items
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no rule {id} in {items:#?}"))
}

fn value_of<'v>(r: &'v Value, key: &str) -> &'v Value {
    &r["values"]
        .as_array()
        .expect("values")
        .iter()
        .find(|v| v["key"] == key)
        .unwrap_or_else(|| panic!("no value {key} in {r}"))["value"]
}

async fn save(fx: &Fx, sub: &str, id: &str, values: Value) -> (StatusCode, Value) {
    send(
        &fx.e.app,
        req(
            "PUT",
            &fx.path(&format!("/{id}")),
            Some(&person(sub, &[])),
            json!({ "values": values }),
        ),
    )
    .await
}

async fn evaluate(fx: &Fx, sub: &str, id: &str, body: Value) -> (StatusCode, Value) {
    send(
        &fx.e.app,
        req(
            "POST",
            &fx.path(&format!("/{id}/evaluate")),
            Some(&person(sub, &[])),
            body,
        ),
    )
    .await
}

async fn count(db: &PgPool, sql: &str, job: Uuid) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(job)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn draft_rev(fx: &Fx) -> i64 {
    count(
        fx.db(),
        "SELECT draft_rev FROM jobs WHERE id = $1",
        fx.job(),
    )
    .await
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct LedgerRow {
    kind: String,
    area: String,
    page: String,
    label: String,
    by_sub: String,
}

async fn ledger_at(fx: &Fx, key: &str) -> Option<LedgerRow> {
    sqlx::query_as::<_, LedgerRow>(
        "SELECT kind, area, page, label, by_sub FROM draft_ledger WHERE job_id = $1 AND key = $2",
    )
    .bind(fx.job())
    .bind(key)
    .fetch_optional(fx.db())
    .await
    .unwrap()
}

fn assert_refused(s: StatusCode, b: &Value, status: StatusCode, code: &str, message: &str) {
    assert_eq!(s, status, "{b}");
    assert_eq!(b["error"]["code"], code, "{b}");
    assert_eq!(b["error"]["message"], message, "{b}");
}

// ---- reads and the draft write --------------------------------------------------------------------------------------

#[tokio::test]
async fn rule_save_is_draft_and_live_values_shown() {
    let fx = setup().await;

    // the template's slots, nothing set yet (an observer reads them too)
    let items = list(&fx, OBSERVER).await;
    let ids: Vec<&str> = items.iter().filter_map(|r| r["id"].as_str()).collect();
    for id in ["RET-01", "WAR-01", "PAY-04"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    let ret = rule(&items, "RET-01");
    assert_eq!(ret["label"], "Return window");
    assert_eq!(ret["placeholder"], true);
    assert_eq!(value_of(ret, "window_days"), &Value::Null);
    assert_eq!(ret["values"][0]["suggested"], "14");
    assert!(
        ret["slot_layer"]
            .as_str()
            .is_some_and(|l| l.starts_with("industry/retail")),
        "{ret}"
    );
    assert!(ret["statement"].as_str().unwrap().contains("{window_days}"));
    assert_eq!(ret["examples"].as_array().map(Vec::len), Some(3));
    assert!(ret.get("live_values").is_none(), "{ret}");
    assert_eq!(ret["passage"], Value::Null);
    assert_eq!(ret["updated_by"], Value::Null);
    assert_eq!(ret["slot_release"], Value::Null);
    assert_eq!(ret["history"], json!([]));
    assert_eq!(ret["decisions"], json!([]));

    // a save lands in the draft: values trimmed, a ledger row, draft_rev bumped, a history row, audit
    let rev = draft_rev(&fx).await;
    let (s, r) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"window_days": " 30 ", "excluded_categories": "made-to-measure rugs"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(value_of(&r, "window_days"), "30");
    assert_eq!(value_of(&r, "excluded_categories"), "made-to-measure rugs");
    assert_eq!(r["placeholder"], false);
    assert_eq!(r["updated_by"], DESIGNER);
    assert_eq!(r["updated_by_name"], DESIGNER);
    assert_eq!(
        r["live_values"],
        json!({"window_days": null, "excluded_categories": null}),
        "live still has no values: the save is a draft"
    );
    let change = "Return window: not set → 30; Excluded categories: not set → made-to-measure rugs";
    assert_eq!(r["history"].as_array().map(Vec::len), Some(1), "{r}");
    assert_eq!(r["history"][0]["change"], change);
    assert_eq!(r["history"][0]["by"], DESIGNER);
    assert_eq!(draft_rev(&fx).await, rev + 1);
    assert_eq!(
        ledger_at(&fx, "rule:RET-01").await,
        Some(LedgerRow {
            kind: "rule_values".into(),
            area: "rules".into(),
            page: "J9".into(),
            label: format!("Return window: {change}"),
            by_sub: DESIGNER.into(),
        })
    );
    let audited = count(
        fx.db(),
        "SELECT count(*) FROM audit_event WHERE job IS NOT NULL AND action = 'job.draft_change' \
           AND resource_id = 'rule:RET-01' AND tenant_id = (SELECT tenant_id FROM jobs WHERE id = $1)",
        fx.job(),
    )
    .await;
    assert_eq!(audited, 1);
    // live (the baseline: the job was never released) is untouched
    let live: Option<Value> = sqlx::query_scalar(
        "SELECT snap FROM release_baseline WHERE job_id = $1 AND key = 'rule:RET-01'",
    )
    .bind(fx.job())
    .fetch_one(fx.db())
    .await
    .unwrap();
    assert_eq!(live, None);
    // the list shows the same draft
    let items = list(&fx, OBSERVER).await;
    assert_eq!(rule(&items, "RET-01")["live_values"], r["live_values"]);
    assert_eq!(value_of(rule(&items, "RET-01"), "window_days"), "30");

    // saving the same values again changes nothing
    let (s, again) = save(&fx, DESIGNER, "RET-01", json!({"window_days": "30"})).await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["history"].as_array().map(Vec::len), Some(1));
    assert_eq!(draft_rev(&fx).await, rev + 1);

    // back to "nothing set" = the live value: the ledger row goes, live_values disappear
    let (s, back) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"window_days": null, "excluded_categories": "  "}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{back}");
    assert_eq!(back["placeholder"], true);
    assert!(back.get("live_values").is_none(), "{back}");
    assert_eq!(ledger_at(&fx, "rule:RET-01").await, None);
    assert_eq!(back["history"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        back["history"][0]["change"],
        "Return window: 30 → not set; Excluded categories: made-to-measure rugs → not set"
    );
}

/// Live is a real value here (review I2): the never-released job's baseline for `rule:RET-01` is replaced by one
/// with `window_days = 14`, so `live_values`, the ledger label (against live) and the history (against the
/// previous draft) all differ. Two saves of different values of one rule both persist (review I1).
#[tokio::test]
async fn rule_save_labels_against_real_live_values() {
    let fx = setup().await;
    list(&fx, OBSERVER).await; // ensure_job: the baseline is taken
    // release_baseline refuses UPDATE (release_snapshot_immutable), so swap the row as the owner
    let mut tx = fx.db().begin().await.unwrap();
    let (tenant, kind, r#ref): (Uuid, String, Value) = sqlx::query_as(
        "DELETE FROM release_baseline WHERE job_id = $1 AND key = 'rule:RET-01' RETURNING tenant_id, kind, ref",
    )
    .bind(fx.job())
    .fetch_one(&mut *tx)
    .await
    .expect("a baseline row for rule:RET-01");
    sqlx::query(
        "INSERT INTO release_baseline (tenant_id, job_id, key, kind, ref, snap) \
         VALUES ($1, $2, 'rule:RET-01', $3, $4, $5)",
    )
    .bind(tenant)
    .bind(fx.job())
    .bind(kind)
    .bind(r#ref)
    .bind(json!({"v": {"window_days": "14"}}))
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let live = json!({"window_days": "14", "excluded_categories": null});

    // the draft has no row, live has 14: the rule shows its live values
    let items = list(&fx, OBSERVER).await;
    assert_eq!(rule(&items, "RET-01")["live_values"], live);

    // one value: history is against the draft (not set), the ledger label against live (14)
    let (s, r) = save(&fx, DESIGNER, "RET-01", json!({"window_days": "30"})).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["live_values"], live, "{r}");
    assert_eq!(r["history"][0]["change"], "Return window: not set → 30");
    assert_eq!(
        ledger_at(&fx, "rule:RET-01").await.map(|l| l.label),
        Some("Return window: Return window: 14 → 30".to_string())
    );

    // the other value, separately: both persist
    let (s, r) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"excluded_categories": "made-to-measure rugs"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(value_of(&r, "window_days"), "30");
    assert_eq!(value_of(&r, "excluded_categories"), "made-to-measure rugs");
    let items = list(&fx, OBSERVER).await;
    assert_eq!(value_of(rule(&items, "RET-01"), "window_days"), "30");
    assert_eq!(
        value_of(rule(&items, "RET-01"), "excluded_categories"),
        "made-to-measure rugs"
    );
    assert_eq!(
        r["history"][0]["change"],
        "Excluded categories: not set → made-to-measure rugs"
    );
    assert_eq!(
        ledger_at(&fx, "rule:RET-01").await.map(|l| l.label),
        Some(
            "Return window: Return window: 14 → 30; Excluded categories: not set → made-to-measure rugs"
                .to_string()
        )
    );

    // back to the live values ("…: back to the live values", rules::tests): the ledger row goes, live_values hide
    let (s, back) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"window_days": "14", "excluded_categories": null}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{back}");
    assert_eq!(value_of(&back, "window_days"), "14");
    assert!(back.get("live_values").is_none(), "{back}");
    assert_eq!(ledger_at(&fx, "rule:RET-01").await, None);
    assert_eq!(back["history"].as_array().map(Vec::len), Some(3), "{back}");
    assert_eq!(
        back["history"][0]["change"],
        "Return window: 30 → 14; Excluded categories: made-to-measure rugs → not set"
    );
}

#[tokio::test]
async fn invalid_value_422s() {
    let fx = setup().await;
    let cases = [
        (
            "RET-01",
            json!({"window_days": "0"}),
            "Return window must be a whole number of days (1–3650).",
        ),
        (
            "RET-01",
            json!({"window_days": "3651"}),
            "Return window must be a whole number of days (1–3650).",
        ),
        (
            "RET-01",
            json!({"window_days": "1.5"}),
            "Return window must be a whole number of days (1–3650).",
        ),
        (
            "RET-01",
            json!({"window_days": "two weeks"}),
            "Return window must be a whole number of days (1–3650).",
        ),
        (
            "WAR-01",
            json!({"period_months": "-3"}),
            "Warranty period must be a whole number of months (1–3650).",
        ),
        (
            "RET-01",
            json!({"excluded_categories": "x".repeat(281)}),
            "Excluded categories is at most 280 characters.",
        ),
    ];
    for (id, values, message) in cases {
        let (s, b) = save(&fx, DESIGNER, id, values).await;
        assert_refused(
            s,
            &b,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
            message,
        );
    }
    // the bounds themselves are fine
    let (s, b) = save(&fx, DESIGNER, "RET-01", json!({"window_days": "3650"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = save(&fx, DESIGNER, "WAR-01", json!({"period_months": "1"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"excluded_categories": "x".repeat(280)}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // a refused save writes nothing: only the three accepted saves left rows
    let before = count(
        fx.db(),
        "SELECT count(*) FROM business_rule_history WHERE job_id = $1",
        fx.job(),
    )
    .await;
    assert_eq!(before, 3);
    let (s, b) = save(&fx, DESIGNER, "WAR-01", json!({"period_months": "0"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let after = count(
        fx.db(),
        "SELECT count(*) FROM business_rule_history WHERE job_id = $1",
        fx.job(),
    )
    .await;
    assert_eq!(after, before);
    let (_, b) = save(&fx, DESIGNER, "WAR-01", json!({})).await;
    assert_eq!(value_of(&b, "period_months"), "1");

    // a rule the template does not define, a body of the wrong shape, an observer
    let (s, b) = save(&fx, DESIGNER, "XYZ-01", json!({"a": "1"})).await;
    assert_refused(
        s,
        &b,
        StatusCode::NOT_FOUND,
        "rule_not_found",
        "No rule XYZ-01.",
    );
    let (s, b) = save(&fx, DESIGNER, "RET-01", json!({"window_days": 30})).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_body"))
    );
    let (s, b) = save(&fx, OBSERVER, "RET-01", json!({"window_days": "30"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "job_designer", "{b}");
}

#[tokio::test]
async fn unknown_value_422() {
    let fx = setup().await;
    let (s, b) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"window_days": "30", "nope": "1"}),
    )
    .await;
    assert_refused(
        s,
        &b,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_value",
        "RET-01 has no value “nope”.",
    );
    assert_eq!(
        count(
            fx.db(),
            "SELECT count(*) FROM business_rule WHERE job_id = $1",
            fx.job()
        )
        .await,
        0,
        "the valid value of a refused save is not written either"
    );
    let (s, b) = evaluate(
        &fx,
        OBSERVER,
        "RET-01",
        json!({"facts": {}, "values": {"nope": "1"}}),
    )
    .await;
    assert_refused(
        s,
        &b,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_value",
        "RET-01 has no value “nope”.",
    );
    let (s, b) = evaluate(&fx, OBSERVER, "XYZ-01", json!({"facts": {}})).await;
    assert_refused(
        s,
        &b,
        StatusCode::NOT_FOUND,
        "rule_not_found",
        "No rule XYZ-01.",
    );
}

#[tokio::test]
async fn passages_are_empty_until_b3_and_linking_needs_one() {
    let fx = setup().await;
    let (s, b) = send(
        &fx.e.app,
        get(&fx.path("/passages"), Some(&person(OBSERVER, &[]))),
    )
    .await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");
    let link = |pid: Value| {
        req(
            "PUT",
            &fx.path("/RET-01/passage"),
            Some(&person(DESIGNER, &[])),
            json!({ "passage_id": pid }),
        )
    };
    let (s, b) = send(&fx.e.app, link(json!("faq-policies/returns#2"))).await;
    assert_refused(
        s,
        &b,
        StatusCode::NOT_FOUND,
        "passage_not_found",
        "No policy passage faq-policies/returns#2.",
    );
    // unlinking what was never linked records a history row and leaves no draft change
    let (s, b) = send(&fx.e.app, link(Value::Null)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["passage"], Value::Null);
    assert_eq!(b["history"][0]["change"], "Unlinked the policy passage");
    assert_eq!(ledger_at(&fx, "rule-passage:RET-01").await, None);
}

// ---- the J9 dry run --------------------------------------------------------------------------------------------------

/// Pairs in slot order → the request's `values` object.
fn values_object(pairs: &Value) -> Value {
    let mut m = serde_json::Map::new();
    for p in pairs.as_array().expect("values pairs") {
        m.insert(p[0].as_str().unwrap().to_string(), p[1].clone());
    }
    Value::Object(m)
}

fn goldens() -> Vec<Value> {
    serde_json::from_str::<Value>(GOLDENS)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn evaluate_matches_fixture_goldens() {
    // the pure evaluator (B4's `rule.evaluate`), every golden including the edge cases
    for g in goldens() {
        let id = g["id"].as_str().unwrap();
        let facts: BTreeMap<String, String> = serde_json::from_value(g["facts"].clone()).unwrap();
        let values: Vec<(String, Option<String>)> = g["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p[0].as_str().unwrap().to_string(),
                    p[1].as_str().map(str::to_owned),
                )
            })
            .collect();
        let got = rules_eval::evaluate(id, &facts, &values, g["today"].as_str().unwrap());
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            g["want"],
            "{id} · {}",
            g["label"]
        );
    }

    // the HTTP dry run: one call per rule id × example the API lists
    let fx = setup().await;
    let items = list(&fx, OBSERVER).await;
    let gs = goldens();
    let mut seen = Vec::new();
    for r in &items {
        let id = r["id"].as_str().unwrap();
        for ex in r["examples"].as_array().unwrap() {
            let g = gs
                .iter()
                .find(|g| g["id"] == id && g["label"] == ex["label"])
                .unwrap_or_else(|| panic!("no golden for {id} · {}", ex["label"]));
            assert_eq!(
                g["facts"], ex["facts"],
                "{id} · {}: the example's facts",
                ex["label"]
            );
            let (s, b) = evaluate(
                &fx,
                OBSERVER,
                id,
                json!({"facts": ex["facts"], "values": values_object(&g["values"]), "as_of": FIXTURE_DAY}),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
            assert_eq!(b, g["want"], "{id} · {}", ex["label"]);
            seen.push(format!("{id} · {}", ex["label"].as_str().unwrap()));
        }
    }
    assert_eq!(
        seen.len(),
        8,
        "3 RET-01 + 2 WAR-01 + 3 PAY-04 examples: {seen:#?}"
    );
}

#[tokio::test]
async fn placeholder_evaluates_to_placeholder() {
    let fx = setup().await;
    let pending = json!({"facts": {"payment.status": "pending", "payment.at": "2026-10-16"}, "as_of": FIXTURE_DAY});
    let (s, b) = evaluate(&fx, OBSERVER, "PAY-04", pending.clone()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({
            "rule": "PAY-04",
            "outcome": "placeholder",
            "code": "placeholder",
            "reason": "PAY-04 has no value for drop_off_days, customer_text: the agent cannot decide and hands the case to a person.",
            "trace": ["value drop_off_days, customer_text = not set → cannot evaluate"],
            "values_used": {"drop_off_days": null, "customer_text": null},
        })
    );
    // one value saved, the other still unset (a blank override counts as unset)
    let (s, r) = save(&fx, DESIGNER, "PAY-04", json!({"drop_off_days": "7"})).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["placeholder"], true);
    let mut blank = pending.clone();
    blank["values"] = json!({"customer_text": "  "});
    for body in [pending.clone(), blank] {
        let (s, b) = evaluate(&fx, OBSERVER, "PAY-04", body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["outcome"], "placeholder", "{b}");
        assert_eq!(
            b["reason"],
            "PAY-04 has no value for customer_text: the agent cannot decide and hands the case to a person."
        );
        assert_eq!(b["values_used"]["drop_off_days"], "7");
    }
    // an override fills the gap for the dry run only
    let mut filled = pending;
    filled["values"] = json!({"customer_text": "It drops off by itself."});
    let (_, b) = evaluate(&fx, OBSERVER, "PAY-04", filled).await;
    assert_eq!(b["code"], "ineligible:PAY-04", "{b}");
    assert_eq!(
        b["reason"],
        "It is a reservation, not a payment. Customer is told: “It drops off by itself.”"
    );
}

#[tokio::test]
async fn evaluate_stores_nothing() {
    let fx = setup().await;
    let (s, r) = save(
        &fx,
        DESIGNER,
        "RET-01",
        json!({"window_days": "30", "excluded_categories": "made-to-measure rugs, opened bedding"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    async fn state(fx: &Fx) -> Vec<i64> {
        let mut v = Vec::new();
        for sql in [
            "SELECT count(*) FROM draft_ledger WHERE job_id = $1",
            "SELECT count(*) FROM business_rule WHERE job_id = $1",
            "SELECT count(*) FROM business_rule_history WHERE job_id = $1",
            "SELECT count(*) FROM audit_event WHERE tenant_id = (SELECT tenant_id FROM jobs WHERE id = $1)",
            "SELECT draft_rev FROM jobs WHERE id = $1",
        ] {
            v.push(count(fx.db(), sql, fx.job()).await);
        }
        v
    }
    async fn stored(fx: &Fx) -> Value {
        sqlx::query_scalar(
            "SELECT slot_values FROM business_rule WHERE job_id = $1 AND rule_id = 'RET-01'",
        )
        .bind(fx.job())
        .fetch_one(fx.db())
        .await
        .unwrap()
    }
    let before = state(&fx).await;
    let values_before = stored(&fx).await;

    // the draft values decide (a lamp delivered 52 days ago is outside 30 days) …
    let lamp = json!({"facts": {"order.delivered_at": "2026-08-27", "offering.category": "lighting"}, "as_of": FIXTURE_DAY});
    let (s, b) = evaluate(&fx, OBSERVER, "RET-01", lamp.clone()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["code"], "ineligible:RET-01", "{b}");
    assert_eq!(b["values_used"]["window_days"], "30");
    // … an override is used for this run only, by a designer too
    let mut wide = lamp;
    wide["values"] = json!({"window_days": "60"});
    let (s, b) = evaluate(&fx, DESIGNER, "RET-01", wide).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["code"], "eligible", "{b}");
    assert_eq!(b["reason"], "Day 52 of 60: the return can be opened.");

    assert_eq!(
        state(&fx).await,
        before,
        "ledger, rules, history, audit and draft_rev unchanged"
    );
    assert_eq!(stored(&fx).await, values_before);
    let items = list(&fx, OBSERVER).await;
    assert_eq!(value_of(rule(&items, "RET-01"), "window_days"), "30");
}

/// The fixture evaluator's answers (see the module doc): `values` are `[key, value]` pairs in slot order.
const GOLDENS: &str = r#"[
 {
  "id": "RET-01",
  "label": "Linen throw, delivered 9 days ago",
  "facts": {
   "order.delivered_at": "2026-10-09",
   "offering.category": "textiles"
  },
  "values": [
   [
    "window_days",
    "30"
   ],
   [
    "excluded_categories",
    "made-to-measure rugs, opened bedding"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "RET-01",
   "values_used": {
    "window_days": "30",
    "excluded_categories": "made-to-measure rugs, opened bedding"
   },
   "outcome": "eligible",
   "code": "eligible",
   "reason": "Day 9 of 30: the return can be opened.",
   "trace": [
    "category “textiles” ∉ excluded [made-to-measure rugs, opened bedding]",
    "9 days since delivery ≤ 30"
   ]
  }
 },
 {
  "id": "RET-01",
  "label": "Rug, delivered last week",
  "facts": {
   "order.delivered_at": "2026-10-11",
   "offering.category": "made-to-measure rugs"
  },
  "values": [
   [
    "window_days",
    "30"
   ],
   [
    "excluded_categories",
    "made-to-measure rugs, opened bedding"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "RET-01",
   "values_used": {
    "window_days": "30",
    "excluded_categories": "made-to-measure rugs, opened bedding"
   },
   "outcome": "ineligible",
   "code": "ineligible:RET-01",
   "reason": "made-to-measure rugs cannot be returned.",
   "trace": [
    "category “made-to-measure rugs” ∈ excluded [made-to-measure rugs, opened bedding]"
   ]
  }
 },
 {
  "id": "RET-01",
  "label": "Lamp, delivered 52 days ago",
  "facts": {
   "order.delivered_at": "2026-08-27",
   "offering.category": "lighting"
  },
  "values": [
   [
    "window_days",
    "30"
   ],
   [
    "excluded_categories",
    "made-to-measure rugs, opened bedding"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "RET-01",
   "values_used": {
    "window_days": "30",
    "excluded_categories": "made-to-measure rugs, opened bedding"
   },
   "outcome": "ineligible",
   "code": "ineligible:RET-01",
   "reason": "The return window closed 22 days ago (day 52 of 30).",
   "trace": [
    "category “lighting” ∉ excluded [made-to-measure rugs, opened bedding]",
    "52 days since delivery > 30"
   ]
  }
 },
 {
  "id": "WAR-01",
  "label": "Floor lamp, delivered in April",
  "facts": {
   "order.delivered_at": "2026-04-11",
   "asset.serial": "LB-4471"
  },
  "values": [
   [
    "period_months",
    "24"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "WAR-01",
   "values_used": {
    "period_months": "24"
   },
   "outcome": "eligible",
   "code": "eligible",
   "reason": "Month 7 of 24: a warranty claim can be opened.",
   "trace": [
    "6 months since delivery < 24"
   ]
  }
 },
 {
  "id": "WAR-01",
  "label": "Sofa, delivered 2023",
  "facts": {
   "order.delivered_at": "2023-09-02",
   "asset.serial": "SF-1180"
  },
  "values": [
   [
    "period_months",
    "24"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "WAR-01",
   "values_used": {
    "period_months": "24"
   },
   "outcome": "ineligible",
   "code": "ineligible:WAR-01",
   "reason": "Out of warranty: delivered 37 months ago, the guarantee is 24 months.",
   "trace": [
    "37 months since delivery ≥ 24"
   ]
  }
 },
 {
  "id": "PAY-04",
  "label": "Cancelled order, charged 2 days ago",
  "facts": {
   "payment.status": "pending",
   "payment.at": "2026-10-16",
   "order.status": "cancelled"
  },
  "values": [
   [
    "drop_off_days",
    "7"
   ],
   [
    "customer_text",
    "It is a reservation, not a payment; it drops off within 7 days."
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "PAY-04",
   "values_used": {
    "drop_off_days": "7",
    "customer_text": "It is a reservation, not a payment; it drops off within 7 days."
   },
   "outcome": "ineligible",
   "code": "ineligible:PAY-04",
   "reason": "It is a reservation, not a payment. Customer is told: “It is a reservation, not a payment; it drops off within 7 days.”",
   "trace": [
    "payment.status = pending",
    "2 days pending ≤ 7"
   ]
  }
 },
 {
  "id": "PAY-04",
  "label": "Still pending after 12 days",
  "facts": {
   "payment.status": "pending",
   "payment.at": "2026-10-06",
   "order.status": "cancelled"
  },
  "values": [
   [
    "drop_off_days",
    "7"
   ],
   [
    "customer_text",
    "It is a reservation, not a payment; it drops off within 7 days."
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "PAY-04",
   "values_used": {
    "drop_off_days": "7",
    "customer_text": "It is a reservation, not a payment; it drops off within 7 days."
   },
   "outcome": "escalate",
   "code": "escalate",
   "reason": "The reservation has been pending 12 days, longer than 7: a person checks it with the bank.",
   "trace": [
    "payment.status = pending",
    "12 days pending > 7"
   ]
  }
 },
 {
  "id": "PAY-04",
  "label": "Settled charge",
  "facts": {
   "payment.status": "settled",
   "payment.at": "2026-10-02",
   "order.status": "delivered"
  },
  "values": [
   [
    "drop_off_days",
    "7"
   ],
   [
    "customer_text",
    "It is a reservation, not a payment; it drops off within 7 days."
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "PAY-04",
   "values_used": {
    "drop_off_days": "7",
    "customer_text": "It is a reservation, not a payment; it drops off within 7 days."
   },
   "outcome": "eligible",
   "code": "eligible",
   "reason": "The charge is settled, not pending: PAY-04 does not block a refund.",
   "trace": [
    "payment.status = settled ≠ pending"
   ]
  }
 },
 {
  "id": "DIS-02",
  "label": "edge",
  "facts": {
   "party.vulnerable": "yes"
  },
  "values": [
   [
    "notice_days",
    "14"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "DIS-02",
   "values_used": {
    "notice_days": "14"
   },
   "outcome": "ineligible",
   "code": "ineligible:DIS-02",
   "reason": "Vulnerable customer: no disconnection notice is sent; a person takes over.",
   "trace": [
    "party.vulnerable = yes"
   ]
  }
 },
 {
  "id": "DIS-02",
  "label": "edge",
  "facts": {
   "party.vulnerable": "no"
  },
  "values": [
   [
    "notice_days",
    "14"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "DIS-02",
   "values_used": {
    "notice_days": "14"
   },
   "outcome": "eligible",
   "code": "eligible",
   "reason": "A notice of 14 days is required before disconnection.",
   "trace": [
    "party.vulnerable = no"
   ]
  }
 },
 {
  "id": "ARR-01",
  "label": "edge",
  "facts": {
   "arrangement.instalment_eur": "20"
  },
  "values": [
   [
    "min_instalment_eur",
    "25"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "ARR-01",
   "values_used": {
    "min_instalment_eur": "25"
   },
   "outcome": "ineligible",
   "code": "ineligible:ARR-01",
   "reason": "€20 a month is below the €25 minimum.",
   "trace": [
    "€20 < €25"
   ]
  }
 },
 {
  "id": "ARR-01",
  "label": "edge",
  "facts": {
   "arrangement.instalment_eur": "30"
  },
  "values": [
   [
    "min_instalment_eur",
    "25"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "ARR-01",
   "values_used": {
    "min_instalment_eur": "25"
   },
   "outcome": "eligible",
   "code": "eligible",
   "reason": "€30 a month meets the €25 minimum.",
   "trace": [
    "€30 ≥ €25"
   ]
  }
 },
 {
  "id": "ARR-01",
  "label": "edge",
  "facts": {},
  "values": [
   [
    "min_instalment_eur",
    "25"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "ARR-01",
   "values_used": {
    "min_instalment_eur": "25"
   },
   "outcome": "escalate",
   "code": "escalate",
   "reason": "Fact arrangement.instalment_eur is missing: the agent reads it first.",
   "trace": [
    "arrangement.instalment_eur = ?"
   ]
  }
 },
 {
  "id": "RET-01",
  "label": "edge",
  "facts": {},
  "values": [
   [
    "window_days",
    "30"
   ],
   [
    "excluded_categories",
    "made-to-measure rugs, opened bedding"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "RET-01",
   "values_used": {
    "window_days": "30",
    "excluded_categories": "made-to-measure rugs, opened bedding"
   },
   "outcome": "escalate",
   "code": "escalate",
   "reason": "Fact order.delivered_at is missing: the agent reads it first.",
   "trace": [
    "order.delivered_at = ?"
   ]
  }
 },
 {
  "id": "RET-01",
  "label": "edge",
  "facts": {
   "order.delivered_at": "soon"
  },
  "values": [
   [
    "window_days",
    "30"
   ],
   [
    "excluded_categories",
    "made-to-measure rugs, opened bedding"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "RET-01",
   "values_used": {
    "window_days": "30",
    "excluded_categories": "made-to-measure rugs, opened bedding"
   },
   "outcome": "escalate",
   "code": "escalate",
   "reason": "Fact order.delivered_at is missing: the agent reads it first.",
   "trace": [
    "order.delivered_at = ?"
   ]
  }
 },
 {
  "id": "PAY-04",
  "label": "edge",
  "facts": {},
  "values": [
   [
    "drop_off_days",
    "7"
   ],
   [
    "customer_text",
    "It is a reservation, not a payment; it drops off within 7 days."
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "PAY-04",
   "values_used": {
    "drop_off_days": "7",
    "customer_text": "It is a reservation, not a payment; it drops off within 7 days."
   },
   "outcome": "escalate",
   "code": "escalate",
   "reason": "Fact payment.status is missing: the agent reads it first.",
   "trace": [
    "payment.status = ?"
   ]
  }
 },
 {
  "id": "PAY-04",
  "label": "edge",
  "facts": {
   "payment.status": "pending"
  },
  "values": [
   [
    "drop_off_days",
    "7"
   ],
   [
    "customer_text",
    "It is a reservation, not a payment; it drops off within 7 days."
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "PAY-04",
   "values_used": {
    "drop_off_days": "7",
    "customer_text": "It is a reservation, not a payment; it drops off within 7 days."
   },
   "outcome": "ineligible",
   "code": "ineligible:PAY-04",
   "reason": "It is a reservation, not a payment. Customer is told: “It is a reservation, not a payment; it drops off within 7 days.”",
   "trace": [
    "payment.status = pending",
    "? days pending ≤ 7"
   ]
  }
 },
 {
  "id": "WAR-01",
  "label": "edge",
  "facts": {
   "order.delivered_at": "2024-10-19"
  },
  "values": [
   [
    "period_months",
    "24"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "WAR-01",
   "values_used": {
    "period_months": "24"
   },
   "outcome": "eligible",
   "code": "eligible",
   "reason": "Month 24 of 24: a warranty claim can be opened.",
   "trace": [
    "23 months since delivery < 24"
   ]
  }
 },
 {
  "id": "WAR-01",
  "label": "edge",
  "facts": {
   "order.delivered_at": "2024-10-18"
  },
  "values": [
   [
    "period_months",
    "24"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "WAR-01",
   "values_used": {
    "period_months": "24"
   },
   "outcome": "ineligible",
   "code": "ineligible:WAR-01",
   "reason": "Out of warranty: delivered 24 months ago, the guarantee is 24 months.",
   "trace": [
    "24 months since delivery ≥ 24"
   ]
  }
 },
 {
  "id": "XYZ-01",
  "label": "edge",
  "facts": {},
  "values": [
   [
    "a",
    "1"
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "XYZ-01",
   "values_used": {
    "a": "1"
   },
   "outcome": "escalate",
   "code": "escalate",
   "reason": "Rule XYZ-01 has no evaluation definition: a person must decide.",
   "trace": []
  }
 },
 {
  "id": "RET-01",
  "label": "placeholder",
  "facts": {},
  "values": [
   [
    "window_days",
    null
   ],
   [
    "excluded_categories",
    " "
   ]
  ],
  "today": "2026-10-18",
  "want": {
   "rule": "RET-01",
   "values_used": {
    "window_days": null,
    "excluded_categories": " "
   },
   "outcome": "placeholder",
   "code": "placeholder",
   "reason": "RET-01 has no value for window_days, excluded_categories: the agent cannot decide and hands the case to a person.",
   "trace": [
    "value window_days, excluded_categories = not set → cannot evaluate"
   ]
  }
 }
]"#;
