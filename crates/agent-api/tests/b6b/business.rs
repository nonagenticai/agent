//! B6b T10 / T12e / T12g: the business-model and dimension routes (`/api/axes*`, `/api/axis-rules`, `/api/profiles*`,
//! `/api/dimensions*`) over the booted platform seed — super_admin only, one test per refusal code (§5), insert-only
//! vocab, rule creation (profile layer + definition + entity + rule row, owned by `base` or an industry, D99),
//! profile create-or-revise, axis value retirement and cardinality (0160), and the 0160 trigger / grants. Fixtures
//! use their own keys (`t10_*`, `t12e_*`) so the seed cannot collide; comparisons sort in Rust.
use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as json_req, person, send, sqlstate, super_admin,
};

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    if method == "GET" {
        send(&e.app, get(uri, Some(&super_admin()))).await
    } else {
        send(&e.app, json_req(method, uri, Some(&super_admin()), body)).await
    }
}

async fn ok_get(e: &TestEnv, uri: &str) -> Value {
    let (s, b) = sa(e, "GET", uri, Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    b
}

async fn count(e: &TestEnv, sql: impl Into<String>) -> i64 {
    let sql: String = sql.into();
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.clone()))
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
}

async fn audits(e: &TestEnv, action: &str) -> Vec<Value> {
    sqlx::query_scalar("SELECT details FROM platform_audit WHERE action = $1 ORDER BY id")
        .bind(action)
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

fn rule_body(key: &str, condition: Value, entity: &str, layer: Option<&str>) -> Value {
    let mut b = json!({
        "key": key, "priority": 300, "condition": condition,
        "adds": {"key": entity, "label": "Added"}
    });
    if let Some(l) = layer {
        b["layer"] = json!(l);
    }
    b
}

async fn add_value(e: &TestEnv, axis: &str, key: &str, parent: Option<&str>) -> Value {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/axes/{axis}/values"),
        json!({"key": key, "label": key, "parent": parent}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{axis}·{key}: {b}");
    b
}

async fn save_profile(e: &TestEnv, key: &str, industry: Option<&str>, selection: Value) -> Value {
    let (s, b) = sa(
        e,
        "PUT",
        &format!("/api/profiles/{key}"),
        json!({"label": key, "industry": industry, "description": "", "selection": selection}),
    )
    .await;
    assert!(s.is_success(), "{key}: {s} {b}");
    b
}

fn blockers(b: &Value) -> BTreeSet<(String, String)> {
    b["error"]["blockers"]
        .as_array()
        .expect("blockers[]")
        .iter()
        .map(|x| {
            (
                x["kind"].as_str().unwrap().to_string(),
                x["key"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn untagged(e: &TestEnv, family: &str) -> BTreeSet<(String, String, String)> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT layer_key, kind, key FROM layer_untagged WHERE family = $1")
            .bind(family)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    rows.into_iter().collect()
}

fn t(a: &str, b: &str, c: &str) -> (String, String, String) {
    (a.into(), b.into(), c.into())
}

// --- access + reads -------------------------------------------------------------------------------------------

#[tokio::test]
async fn business_routes_are_super_admin_only() {
    let e = crate::common::env().await;
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, "Tenant b").await;
    insert_user(&e.owner, "u-admin", tenant, "tenant_admin").await;
    let admin = person("u-admin", &[]);
    for uri in [
        "/api/axes",
        "/api/axes/renewal-presets",
        "/api/axis-rules",
        "/api/profiles",
        "/api/dimensions",
    ] {
        let (s, b) = send(&e.app, get(uri, Some(&admin))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{uri}");
    }
    let (s, b) = send(
        &e.app,
        json_req(
            "POST",
            "/api/dimensions",
            Some(&admin),
            json!({"key": "x", "label": "X"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

#[tokio::test]
async fn reads_return_the_vocabulary() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let axes = ok_get(&e, "/api/axes").await;
    let pm = axes
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "payment_method")
        .expect("payment_method");
    assert!(
        pm["values"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["key"] == "card.debit" && v["parent"] == "card")
    );
    assert!(pm["values"][0].get("retired_at").is_none(), "not retired");
    // renewal presets have no authoring route: two rows through the owner pool
    for (key, order_type) in [
        ("continuous_provider_explicit", "continuous"),
        ("discrete_none", "discrete"),
    ] {
        sqlx::query(
            "INSERT INTO renewal_preset (key, label, order_type, initiator) VALUES ($1, $1, $2, 'provider')",
        )
        .bind(key)
        .bind(order_type)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let presets = ok_get(&e, "/api/axes/renewal-presets").await;
    assert_eq!(presets.as_array().unwrap().len(), 2);
    save_profile(
        &e,
        "internal_it",
        None,
        json!({"order_type": ["recurring"]}),
    )
    .await;
    let profiles = ok_get(&e, "/api/profiles").await;
    let keys: BTreeSet<&str> = profiles
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["key"].as_str().unwrap())
        .collect();
    for k in ["retail_ecommerce", "internal_it"] {
        assert!(keys.contains(k), "{k} in {keys:?}");
    }
    assert_eq!(
        profiles.as_array().unwrap().len(),
        keys.len(),
        "one current profile per key"
    );
    let dims = ok_get(&e, "/api/dimensions").await;
    let region = dims
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["key"] == "region")
        .expect("region");
    assert_eq!(region["kind"], "placeholder");
}

// --- one test per refusal code (§5, T12e, T12g) ---------------------------------------------------------------

#[tokio::test]
async fn refusal_codes() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let cond = json!({"eq": ["order_type", "discrete"]});
    let sel = json!({"order_type": ["discrete"]});
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axis-rules",
        rule_body("bundles", cond.clone(), "Bundle", None),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let cases: Vec<(&str, String, Value, StatusCode, &str)> = vec![
        // addAxisValue
        (
            "POST",
            "/api/axes/nope/values".into(),
            json!({"key": "a", "label": "A"}),
            StatusCode::NOT_FOUND,
            "axis_not_found",
        ),
        (
            "POST",
            "/api/axes/contract_type/values".into(),
            json!({"key": "Bad", "label": "A"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value_key",
        ),
        (
            "POST",
            "/api/axes/contract_type/values".into(),
            json!({"key": "a", "label": "A", "parent": "nope"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_parent",
        ),
        (
            "POST",
            "/api/axes/contract_type/values".into(),
            json!({"key": "sale", "label": "Sale"}),
            StatusCode::CONFLICT,
            "value_exists",
        ),
        (
            "POST",
            "/api/axes/contract_type/values".into(),
            json!({"key": "t10_x", "label": "  "}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_label",
        ),
        // createAxisRule (+ D99 layer)
        (
            "POST",
            "/api/axis-rules".into(),
            rule_body("X", cond.clone(), "MeterBox", None),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_rule_key",
        ),
        (
            "POST",
            "/api/axis-rules".into(),
            rule_body("bundles", cond.clone(), "MeterBox", None),
            StatusCode::CONFLICT,
            "rule_exists",
        ),
        (
            "POST",
            "/api/axis-rules".into(),
            rule_body(
                "t10_r",
                json!({"eq": ["order_type", "nope"]}),
                "MeterBox",
                None,
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_axis_value",
        ),
        (
            "POST",
            "/api/axis-rules".into(),
            rule_body("t10_r", cond.clone(), "meterBox", None),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_entity",
        ),
        (
            "POST",
            "/api/axis-rules".into(),
            rule_body(
                "t10_r",
                cond.clone(),
                "MeterBox",
                Some("job/customer_care_agent"),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_rule_layer",
        ),
        // saveBusinessProfile
        (
            "PUT",
            "/api/profiles/B".into(),
            json!({"label": "B", "selection": sel}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_profile_key",
        ),
        (
            "PUT",
            "/api/profiles/t10_p".into(),
            json!({"label": " ", "selection": sel}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_label",
        ),
        (
            "PUT",
            "/api/profiles/t10_p".into(),
            json!({"label": "P", "selection": {"nope": ["x"]}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_axis",
        ),
        (
            "PUT",
            "/api/profiles/t10_p".into(),
            json!({"label": "P", "selection": {"order_type": ["nope"]}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_axis_value",
        ),
        (
            "PUT",
            "/api/profiles/t10_p".into(),
            json!({"label": "P", "selection": {"order_type": []}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_selection",
        ),
        (
            "PUT",
            "/api/profiles/t10_p".into(),
            json!({"label": "P", "industry": "nope", "selection": sel}),
            StatusCode::NOT_FOUND,
            "industry_not_found",
        ),
        // addDimensionValue
        (
            "POST",
            "/api/dimensions/nope/values".into(),
            json!({"key": "a", "label": "A"}),
            StatusCode::NOT_FOUND,
            "dimension_not_found",
        ),
        (
            "POST",
            "/api/dimensions/area/values".into(),
            json!({"key": "Bad Key", "label": "A"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value_key",
        ),
        (
            "POST",
            "/api/dimensions/area/values".into(),
            json!({"key": "t10-a", "label": ""}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_label",
        ),
        (
            "POST",
            "/api/dimensions/channel/values".into(),
            json!({"key": "email", "label": "Email"}),
            StatusCode::CONFLICT,
            "value_exists",
        ),
        // createAxis
        (
            "POST",
            "/api/axes".into(),
            json!({"key": "Bad", "label": "A", "cardinality": "one", "object": "Order"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_axis_key",
        ),
        (
            "POST",
            "/api/axes".into(),
            json!({"key": "t12e_a", "label": "", "cardinality": "one", "object": "Order"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_label",
        ),
        (
            "POST",
            "/api/axes".into(),
            json!({"key": "t12e_a", "label": "A", "cardinality": "some", "object": "Order"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_cardinality",
        ),
        (
            "POST",
            "/api/axes".into(),
            json!({"key": "t12e_a", "label": "A", "cardinality": "one", "object": "Nope"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_object",
        ),
        (
            "POST",
            "/api/axes".into(),
            json!({"key": "contract_type", "label": "A", "cardinality": "one", "object": "Contract"}),
            StatusCode::CONFLICT,
            "axis_exists",
        ),
        // retireAxisValue / setAxisCardinality
        (
            "POST",
            "/api/axes/nope/values/a/retire".into(),
            Value::Null,
            StatusCode::NOT_FOUND,
            "axis_not_found",
        ),
        (
            "POST",
            "/api/axes/contract_type/values/nope/retire".into(),
            Value::Null,
            StatusCode::NOT_FOUND,
            "value_not_found",
        ),
        (
            "PATCH",
            "/api/axes/nope".into(),
            json!({"cardinality": "many"}),
            StatusCode::NOT_FOUND,
            "axis_not_found",
        ),
        (
            "PATCH",
            "/api/axes/order_type".into(),
            json!({"cardinality": "few"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_cardinality",
        ),
        // createDimension
        (
            "POST",
            "/api/dimensions".into(),
            json!({"key": "Tier", "label": "Tier"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_dimension_key",
        ),
        (
            "POST",
            "/api/dimensions".into(),
            json!({"key": "tier", "label": " "}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_label",
        ),
        (
            "POST",
            "/api/dimensions".into(),
            json!({"key": "tier", "label": "Tier", "kind": "switch"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_kind",
        ),
        (
            "POST",
            "/api/dimensions".into(),
            json!({"key": "region", "label": "Region"}),
            StatusCode::CONFLICT,
            "dimension_exists",
        ),
    ];
    for (method, uri, body, status, want) in cases {
        let (s, b) = sa(&e, method, &uri, body).await;
        assert_eq!((s, code(&b)), (status, want), "{method} {uri}: {b}");
    }
}

// --- T10 ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn rule_creates_profile_layer_untagged_in_base_family() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axis-rules",
        rule_body(
            "t10_meters",
            json!({"eq": ["order_type", "discrete"]}),
            "MeterBox",
            None,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["layer_key"], "profile/t10_meters");
    assert_eq!(b["owner"], "base");
    assert_eq!(b["adds"], json!(["MeterBox"]));
    assert_eq!(b["rows"], 1);
    let family: String =
        sqlx::query_scalar("SELECT family FROM layer WHERE key = 'profile/t10_meters'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(family, "base");
    let drafts = untagged(&e, "base").await;
    assert!(drafts.contains(&t("profile/t10_meters", "entity", "MeterBox")));
    assert!(drafts.contains(&t("base", "axis_rule", "t10_meters")));
    let rules = ok_get(&e, "/api/axis-rules").await;
    let listed = rules
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "t10_meters")
        .expect("listed");
    assert_eq!(listed["adds"], json!(["MeterBox"]));
    let priorities: Vec<i64> = rules
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["priority"].as_i64().unwrap())
        .collect();
    assert!(priorities.windows(2).all(|w| w[0] <= w[1]), "by priority");
    let a = audits(&e, "business_model.rule_create").await;
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["layer"], "base");
}

#[tokio::test]
async fn profile_save_revises_not_duplicates() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let sel = json!({"order_type": ["discrete"], "customer_type": ["natural_person", "", "natural_person"], "vulnerability": []});
    let (s, first) = sa(
        &e,
        "PUT",
        "/api/profiles/t10_shop",
        json!({"key": "t10_shop", "label": "Shop", "industry": "retail", "description": "one", "selection": sel}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{first}");
    assert_eq!(first["layer"], "industry/retail");
    assert_eq!(first["rev"], 1);
    assert_eq!(
        first["selection"],
        json!({"customer_type": ["natural_person"], "order_type": ["discrete"]}),
        "empty axes dropped, repeats collapsed"
    );
    let (s, second) = sa(
        &e,
        "PUT",
        "/api/profiles/t10_shop",
        json!({"label": "Shop", "industry": "retail", "description": "two", "selection": sel}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{second}");
    assert_eq!(second["rev"], 2);
    assert_eq!(second["description"], "two");
    let current = count(
        &e,
        "SELECT count(*) FROM layer_row WHERE kind = 'profile' AND key = 't10_shop' AND is_current",
    )
    .await;
    assert_eq!(current, 1);
    // A move to base tombstones the industry row; still one live profile.
    let moved = save_profile(&e, "t10_shop", None, json!({"order_type": ["discrete"]})).await;
    assert_eq!(moved["layer"], "base");
    let profiles = ok_get(&e, "/api/profiles").await;
    let mine: Vec<&Value> = profiles
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["key"] == "t10_shop")
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["layer"], "base");
    assert_eq!(audits(&e, "business_model.save").await.len(), 3);
}

#[tokio::test]
async fn axis_value_insert_only_409() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let a = add_value(&e, "payment_method", "voucher", Some("card")).await;
    assert!(
        a["values"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["key"] == "card.voucher" && v["added_by"] == "u-super")
    );
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axes/payment_method/values",
        json!({"key": "voucher", "label": "Again", "parent": "card"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "value_exists"));
    // retired values are still taken
    let (s, _) = sa(
        &e,
        "POST",
        "/api/axes/payment_method/values/card.voucher/retire",
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axes/payment_method/values",
        json!({"key": "voucher", "label": "Again", "parent": "card"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "value_exists"));
    let a = audits(&e, "business_model.axis_value_add").await;
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["value"], "card.voucher");
}

#[tokio::test]
async fn dimension_value_audited_dimension_value_add() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let (s, b) = sa(
        &e,
        "POST",
        "/api/dimensions/area/values",
        json!({"key": "t10-area", "label": "T10 area"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(
        b["values"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["key"] == "t10-area")
    );
    let a: Vec<Value> = audits(&e, "dimension.value_add")
        .await
        .into_iter()
        .filter(|d| d["dimension"] == "area")
        .collect();
    assert_eq!(a, vec![json!({"dimension": "area", "value": "t10-area"})]);
    let (s, b) = sa(
        &e,
        "POST",
        "/api/dimensions/area/values",
        json!({"key": "t10-area", "label": "Again"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "value_exists"));
}

// --- T12g (D99) -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn rule_without_layer_is_owned_by_base() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    for (key, layer) in [("t12g_none", None), ("t12g_base", Some("base"))] {
        let (s, b) = sa(
            &e,
            "POST",
            "/api/axis-rules",
            rule_body(
                key,
                json!({"eq": ["order_type", "discrete"]}),
                "Thing",
                layer,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{key}: {b}");
        let owner: String = sqlx::query_scalar("SELECT owner FROM axis_rule WHERE key = $1")
            .bind(key)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        assert_eq!(owner, "base", "{key}");
    }
}

#[tokio::test]
async fn rule_with_industry_layer_is_owned_by_that_family() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axis-rules",
        rule_body(
            "t12g_retail",
            json!({"eq": ["order_type", "discrete"]}),
            "GiftCard",
            Some("industry/retail"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["owner"], "industry/retail");
    let family: String =
        sqlx::query_scalar("SELECT family FROM layer WHERE key = 'profile/t12g_retail'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(family, "industry/retail");
    let retail = untagged(&e, "industry/retail").await;
    assert!(retail.contains(&t("profile/t12g_retail", "entity", "GiftCard")));
    assert!(retail.contains(&t("industry/retail", "axis_rule", "t12g_retail")));
    let base = untagged(&e, "base").await;
    assert!(
        base.iter()
            .all(|(l, _, k)| l != "profile/t12g_retail" && k != "t12g_retail"),
        "nothing of the rule is untagged in base"
    );
    assert_eq!(
        audits(&e, "business_model.rule_create").await[0]["layer"],
        "industry/retail"
    );
}

async fn rule_layer_refused(e: &TestEnv, layer: &str) {
    let (s, b) = sa(
        e,
        "POST",
        "/api/axis-rules",
        rule_body(
            "t12g_bad",
            json!({"eq": ["order_type", "discrete"]}),
            "Thing",
            Some(layer),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_rule_layer"),
        "{layer}: {b}"
    );
    assert_eq!(b["error"]["layer"], layer);
}

#[tokio::test]
async fn rule_layer_unknown_422_invalid_rule_layer() {
    let e = crate::common::env().await;
    rule_layer_refused(&e, "industry/nope").await;
    rule_layer_refused(&e, "nope").await;
}

#[tokio::test]
async fn rule_layer_industry_dept_422() {
    let e = crate::common::env().await;
    rule_layer_refused(&e, "industry_dept/retail/customer_service").await;
}

#[tokio::test]
async fn rule_layer_profile_or_job_422() {
    let e = crate::common::env().await;
    for l in [
        "profile/bundles",
        "job/customer_care_agent",
        "job/customer_care_agent/channel=email",
    ] {
        rule_layer_refused(&e, l).await;
    }
}

#[tokio::test]
async fn invalid_rule_layer_writes_nothing() {
    let e = crate::common::env().await;
    let tables = ["layer", "layer_row", "axis_rule", "platform_audit"];
    let mut before = Vec::new();
    for t in tables {
        before.push(count(&e, &format!("SELECT count(*) FROM {t}")).await);
    }
    rule_layer_refused(&e, "industry_dept/retail/customer_service").await;
    for (t, n) in tables.iter().zip(before) {
        assert_eq!(
            count(&e, &format!("SELECT count(*) FROM {t}")).await,
            n,
            "{t}"
        );
    }
    let layer = count(
        &e,
        "SELECT count(*) FROM layer WHERE key = 'profile/t12g_bad'",
    )
    .await;
    assert_eq!(layer, 0);
}

// --- T12e -----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_axis_exists_409() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let body = json!({"key": "offering.t12e_tier", "label": "Tier", "description": "d",
                      "cardinality": "one", "object": "Offering", "milestone": "M4"});
    let (s, b) = sa(&e, "POST", "/api/axes", body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["values"], json!([]));
    assert_eq!(b["milestone"], "M4");
    // rule-added objects (entities) classify too
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axes",
        json!({"key": "t12e_renewal", "label": "R", "cardinality": "many", "object": "Renewal"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["milestone"], "M1");
    let (s, b) = sa(&e, "POST", "/api/axes", body).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "axis_exists"));
    assert_eq!(audits(&e, "business_model.axis_create").await.len(), 2);
}

#[tokio::test]
async fn retire_value_in_profile_409_lists_blockers() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    add_value(&e, "contract_type", "t12e_held", None).await;
    add_value(&e, "contract_type", "t12e_kid", Some("t12e_held")).await;
    save_profile(
        &e,
        "t12e_holder",
        None,
        json!({"contract_type": ["t12e_held"]}),
    )
    .await;
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axis-rules",
        rule_body(
            "t12e_rule",
            json!({"not": {"in": ["contract_type", ["sale", "t12e_held"]]}}),
            "Thing",
            None,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axes/contract_type/values/t12e_held/retire",
        Value::Null,
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "value_in_use"), "{b}");
    let want: BTreeSet<(String, String)> = [
        ("profile", "t12e_holder"),
        ("axis_rule", "t12e_rule"),
        ("child_value", "t12e_held.t12e_kid"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(blockers(&b), want);
    let retired = count(
        &e,
        "SELECT count(*) FROM axis_value WHERE retired_at IS NOT NULL",
    )
    .await;
    assert_eq!(retired, 0, "nothing retired");
}

#[tokio::test]
async fn retire_value_unused_ok_and_hidden_from_options() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    add_value(&e, "contract_type", "t12e_gone", None).await;
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axes/contract_type/values/t12e_gone/retire",
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v = b["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "t12e_gone")
        .expect("kept in reads");
    assert!(v["retired_at"].is_string());
    assert_eq!(v["retired_by"], "u-super");
    // hidden from every picker: profiles, rules, parents
    let (s, b) = sa(
        &e,
        "PUT",
        "/api/profiles/t12e_p",
        json!({"label": "P", "selection": {"contract_type": ["t12e_gone"]}}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_axis_value")
    );
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axis-rules",
        rule_body(
            "t12e_r",
            json!({"eq": ["contract_type", "t12e_gone"]}),
            "Thing",
            None,
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_axis_value")
    );
    let (s, b) = sa(
        &e,
        "POST",
        "/api/axes/contract_type/values",
        json!({"key": "child", "label": "C", "parent": "t12e_gone"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_parent")
    );
    assert_eq!(
        audits(&e, "business_model.axis_value_retire").await,
        vec![json!({"axis": "contract_type", "value": "t12e_gone"})]
    );
}

#[tokio::test]
async fn retire_twice_409() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    add_value(&e, "contract_type", "t12e_twice", None).await;
    let uri = "/api/axes/contract_type/values/t12e_twice/retire";
    let (s, _) = sa(&e, "POST", uri, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = sa(&e, "POST", uri, Value::Null).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "value_retired"));
}

#[tokio::test]
async fn cardinality_many_to_one_blocked_by_profile() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    // customer_type is `many`; a profile holding two values blocks `one`.
    save_profile(
        &e,
        "t12e_two",
        None,
        json!({"customer_type": ["natural_person", "household"]}),
    )
    .await;
    let (s, b) = sa(
        &e,
        "PATCH",
        "/api/axes/customer_type",
        json!({"cardinality": "one"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "cardinality_in_use"),
        "{b}"
    );
    assert!(blockers(&b).contains(&("profile".into(), "t12e_two".into())));
    let still: String =
        sqlx::query_scalar("SELECT cardinality FROM axis WHERE key = 'customer_type'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(still, "many");
    // one → many always
    let (s, b) = sa(
        &e,
        "PATCH",
        "/api/axes/order_type",
        json!({"cardinality": "many"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["cardinality"], "many");
    assert_eq!(
        audits(&e, "business_model.axis_cardinality").await,
        vec![json!({"axis": "order_type", "from": "one", "to": "many"})]
    );
}

#[tokio::test]
async fn create_dimension_insert_only() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    let (s, b) = sa(
        &e,
        "POST",
        "/api/dimensions",
        json!({"key": "t12e-tier", "label": "Tier", "description": "service tier"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["kind"], "dimension");
    assert_eq!(b["values"], json!([]));
    let (s, b) = sa(
        &e,
        "POST",
        "/api/dimensions",
        json!({"key": "t12e-tier", "label": "Again", "kind": "placeholder"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "dimension_exists"));
    let label: String = sqlx::query_scalar("SELECT label FROM dimension WHERE key = 't12e-tier'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(label, "Tier", "unchanged");
    let created: Vec<Value> = audits(&e, "dimension.create")
        .await
        .into_iter()
        .filter(|d| d["dimension"] == "t12e-tier")
        .collect();
    assert_eq!(created.len(), 1);
}

#[tokio::test]
async fn vocab_0160_retire_once_trigger() {
    let e = crate::common::env().await;
    crate::b6b::support::vocabulary(&e).await;
    sqlx::query(
        "INSERT INTO axis_value (axis, key, label, added_by) VALUES ('contract_type', 't12e_trig', 'T', 'test')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let owner = &e.owner;
    let upd = |sql: &'static str| async move {
        sqlx::query(sql)
            .execute(owner)
            .await
            .map(|_| ())
            .map_err(|err| sqlstate(&err))
    };
    assert_eq!(
        upd("UPDATE axis_value SET label = 'X' WHERE key = 't12e_trig'").await,
        Err(Some("23514".into())),
        "an update may only retire"
    );
    assert_eq!(
        upd("UPDATE axis_value SET retired_at = now() WHERE key = 't12e_trig'").await,
        Err(Some("23514".into())),
        "retired_at and retired_by together"
    );
    assert_eq!(
        upd(
            "UPDATE axis_value SET retired_at = now(), retired_by = 'test' WHERE key = 't12e_trig'"
        )
        .await,
        Ok(())
    );
    assert_eq!(
        upd("UPDATE axis_value SET retired_at = NULL, retired_by = NULL WHERE key = 't12e_trig'")
            .await,
        Err(Some("23514".into())),
        "never un-retired"
    );
    assert_eq!(
        upd(
            "UPDATE axis_value SET retired_at = now(), retired_by = 'again' WHERE key = 't12e_trig'"
        )
        .await,
        Err(Some("23514".into())),
        "retired once"
    );
}

#[tokio::test]
async fn app_role_cannot_update_axis_value() {
    let e = crate::common::env().await;
    for (role, sql) in [
        (
            "agent_app",
            "UPDATE axis_value SET retired_at = now(), retired_by = 'x' WHERE key = 'sale'",
        ),
        (
            "agent_app",
            "UPDATE axis SET cardinality = 'many' WHERE key = 'order_type'",
        ),
        (
            "agent_platform",
            "UPDATE axis_value SET label = 'x' WHERE key = 'sale'",
        ),
        (
            "agent_platform",
            "UPDATE axis SET label = 'x' WHERE key = 'order_type'",
        ),
        (
            "agent_platform",
            "DELETE FROM axis_value WHERE key = 'sale'",
        ),
    ] {
        let mut tx = e.owner.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
            .execute(&mut *tx)
            .await
            .unwrap();
        let err = sqlx::query(sql).execute(&mut *tx).await.expect_err(sql);
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{role}: {sql}");
    }
    // agent_platform may retire (the route's path).
    let mut tx = e.owner.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE agent_platform")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE axis_value SET retired_at = now(), retired_by = 'x' WHERE axis = 'contract_type' AND key = 'mixed'")
        .execute(&mut *tx)
        .await
        .expect("agent_platform retires");
}
