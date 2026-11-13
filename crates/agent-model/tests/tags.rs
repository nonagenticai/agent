//! B6 T3: tag logic (plan B6-4, B6-5): contents, diff order, the draft op (Ruling 16: tombstone first), the content
//! hash, the composition stack and the catch-up counts.

use std::collections::BTreeSet;

use agent_model::canonical::{canonical_json, fields_hash, sha256_hex};
use agent_model::compile::Row;
use agent_model::kinds::{Kind, Op};
use agent_model::layer::Scope;
use agent_model::tags::{
    DiffChange, StackLayer, TagItem, TagMember, catch_up, composed_on_by, content_hash, contents,
    diff, draft_op, layer_stack, member_keys,
};
use serde_json::{Value, json};

/// (change, kind, key, before, after) of one diff row.
type DiffTuple<'a> = (
    DiffChange,
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
);
fn row(kind: Kind, key: &str, op: Op, fields: Value) -> Row {
    Row {
        id: format!("{}:{key}:{}", kind.as_str(), op.as_str()),
        layer_id: "L".to_owned(),
        kind,
        key: key.to_owned(),
        op,
        fields,
        rev: 1,
    }
}

fn item(kind: Kind, key: &str, detail: &str) -> TagItem {
    TagItem {
        kind,
        key: key.to_owned(),
        label: key.to_owned(),
        detail: detail.to_owned(),
        fields_hash: None,
        region: None,
    }
}

fn member(layer: &str, kind: &str, key: &str, op: &str, fields: &Value) -> TagMember {
    TagMember {
        layer: layer.to_owned(),
        kind: kind.to_owned(),
        key: key.to_owned(),
        op: op.to_owned(),
        fields_hash: fields_hash(fields),
    }
}

#[test]
fn contents_fold_add_patch_tombstone() {
    let rows = [
        row(
            Kind::Entity,
            "Order",
            Op::Add,
            json!({"label": "Order", "summary": "Order · thing"}),
        ),
        row(Kind::Entity, "Party", Op::Add, json!({"label": "Party"})),
        row(
            Kind::Object,
            "order",
            Op::Add,
            json!({"label": "Order", "tier": "base"}),
        ),
        row(Kind::Entity, "Party", Op::Tombstone, json!({})),
        row(
            Kind::Entity,
            "Order",
            Op::Patch,
            json!({"label": "Purchase", "summary": "Purchase · thing"}),
        ),
    ];
    let items = contents(&rows);
    let got: Vec<(Kind, &str, &str, &str)> = items
        .iter()
        .map(|i| (i.kind, i.key.as_str(), i.label.as_str(), i.detail.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (Kind::Entity, "Order", "Purchase", "Purchase · thing"),
            // no stored summary: the kind's one-line summary
            (
                Kind::Object,
                "order",
                "Order",
                "Order · 0 fields · 0 states"
            ),
        ]
    );
    assert_eq!(
        items[0].fields_hash.as_deref(),
        Some(fields_hash(&rows[4].fields).as_str())
    );
}

#[test]
fn diff_orders_added_changed_removed() {
    let before = [
        item(Kind::Object, "order", "Order · 5 fields"),
        item(Kind::Entity, "Party", "Party"),
        item(Kind::Entity, "Basket", "Basket"),
        item(Kind::Entity, "Same", "Same"),
    ];
    let after = [
        item(Kind::Entity, "Zeta", "Zeta"),
        item(Kind::Object, "order", "Order · 6 fields"),
        item(Kind::Entity, "Same", "Same"),
        item(Kind::Entity, "Alpha", "Alpha"),
        item(Kind::Entity, "Party", "Person"),
    ];
    let d = diff(&before, &after);
    let got: Vec<DiffTuple<'_>> = d
        .iter()
        .map(|r| {
            (
                r.change,
                r.kind.as_str(),
                r.key.as_str(),
                r.before.as_deref(),
                r.after.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (DiffChange::Added, "entity", "Alpha", None, Some("Alpha")),
            (DiffChange::Added, "entity", "Zeta", None, Some("Zeta")),
            (
                DiffChange::Changed,
                "entity",
                "Party",
                Some("Party"),
                Some("Person")
            ),
            (
                DiffChange::Changed,
                "object",
                "order",
                Some("Order · 5 fields"),
                Some("Order · 6 fields")
            ),
            (
                DiffChange::Removed,
                "entity",
                "Basket",
                Some("Basket"),
                None
            ),
        ]
    );
    assert_eq!(
        serde_json::to_value(DiffChange::Changed).expect("change"),
        json!("changed")
    );
    // nothing changed → no rows
    assert!(diff(&after, &after).is_empty());
}

#[test]
fn diff_reports_a_fields_change_the_detail_hides() {
    let mut old = item(Kind::Object, "order", "Order · 5 fields · 3 states");
    old.fields_hash = Some(fields_hash(&json!({"label": "Order", "fields": ["a"]})));
    let mut new = old.clone();
    new.fields_hash = Some(fields_hash(&json!({"label": "Order", "fields": ["b"]})));
    let d = diff(std::slice::from_ref(&old), std::slice::from_ref(&new));
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].change, DiffChange::Changed);
    assert_eq!(d[0].before, d[0].after, "the one-line detail did not move");
    assert_eq!(
        (d[0].before_hash.as_deref(), d[0].after_hash.as_deref()),
        (old.fields_hash.as_deref(), new.fields_hash.as_deref())
    );
    assert!(diff(std::slice::from_ref(&old), std::slice::from_ref(&old)).is_empty());
}

/// Ruling 19: with both fields hashes present only they decide; a detail-only change is not a change.
#[test]
fn diff_ignores_a_detail_change_when_the_hashes_agree() {
    let mut old = item(Kind::Object, "order", "Order · 5 fields");
    old.fields_hash = Some(fields_hash(&json!({"label": "Order"})));
    let mut new = old.clone();
    new.detail = "Order · five fields".to_owned();
    assert!(diff(std::slice::from_ref(&old), std::slice::from_ref(&new)).is_empty());
    // a missing hash on either side falls back to the detail
    let mut unhashed = new.clone();
    unhashed.fields_hash = None;
    let d = diff(std::slice::from_ref(&old), std::slice::from_ref(&unhashed));
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].change, DiffChange::Changed);
}

/// Ruling 16 / T7 decision 4: tombstone first, then `add` when (layer, kind, key) is not in the latest tag, else
/// `patch` — the same order as the SQL `CASE` of `agent-api` `untagged_rows`.
#[test]
fn draft_op_add_patch_tombstone() {
    let tagged = [
        member(
            "job/x",
            "entity",
            "Order",
            "add",
            &json!({"label": "Order"}),
        ),
        member(
            "job/x",
            "entity",
            "Party",
            "add",
            &json!({"label": "Party"}),
        ),
    ];
    let latest = member_keys(&tagged);
    assert_eq!(
        draft_op(Op::Add, "job/x", "entity", "Basket", &latest),
        Op::Add
    );
    assert_eq!(
        draft_op(Op::Add, "job/x", "entity", "Order", &latest),
        Op::Patch
    );
    assert_eq!(
        draft_op(Op::Patch, "job/x", "entity", "Order", &latest),
        Op::Patch
    );
    assert_eq!(
        draft_op(Op::Tombstone, "job/x", "entity", "Party", &latest),
        Op::Tombstone
    );
    // the order-fixing case: a tombstone of a key the tag does not hold is a tombstone, not an add
    assert_eq!(
        draft_op(Op::Tombstone, "job/x", "entity", "Ghost", &latest),
        Op::Tombstone
    );
    // the same key in another layer of the family is a different member
    assert_eq!(
        draft_op(Op::Patch, "job/x/channel=email", "entity", "Order", &latest),
        Op::Add
    );
    // a family with no tag: everything that is not a tombstone is an add
    let none = BTreeSet::new();
    assert_eq!(
        draft_op(Op::Patch, "job/x", "entity", "Order", &none),
        Op::Add
    );
    assert_eq!(
        draft_op(Op::Tombstone, "job/x", "entity", "Order", &none),
        Op::Tombstone
    );
}

#[test]
fn content_hash_is_stable_and_order_independent() {
    let a = member(
        "job/x",
        "entity",
        "Order",
        "add",
        &json!({"label": "Order"}),
    );
    let b = member(
        "job/x",
        "entity",
        "Party",
        "add",
        &json!({"label": "Party"}),
    );
    let h = content_hash(&[a.clone(), b.clone()]);
    assert_eq!(h, content_hash(&[b.clone(), a.clone()]));
    assert!(
        h.starts_with("sha256:") && h.len() == "sha256:".len() + 64,
        "{h}"
    );
    assert_ne!(
        h,
        content_hash(&[
            a.clone(),
            member("job/x", "entity", "Party", "tombstone", &json!({}))
        ])
    );
    // the exact formula: canonical JSON of the sorted member list
    let want = format!(
        "sha256:{}",
        sha256_hex(&canonical_json(&json!([
            {"layer": "job/x", "kind": "entity", "key": "Order", "op": "add", "fields_hash": a.fields_hash},
            {"layer": "job/x", "kind": "entity", "key": "Party", "op": "add", "fields_hash": b.fields_hash},
        ])))
    );
    assert_eq!(h, want);
    // pinned value of the empty tag: any change to the algorithm moves every stored tag hash
    assert_eq!(content_hash(&[]), format!("sha256:{}", sha256_hex("[]")));
}

fn layer(key: &str, scope: Scope, industry: Option<&str>) -> StackLayer {
    StackLayer {
        layer_key: key.to_owned(),
        scope,
        industry: industry.map(str::to_owned),
    }
}

#[test]
fn layer_stack_and_composed_on_by() {
    let layers = [
        layer("job/customer_care_agent", Scope::Job, Some("retail")),
        layer(
            "industry_dept/retail/customer_service",
            Scope::IndustryDept,
            Some("retail"),
        ),
        layer("industry/utilities", Scope::Industry, Some("utilities")),
        layer("base", Scope::Base, None),
        layer("industry/retail", Scope::Industry, Some("retail")),
        layer("job/faq_agent", Scope::Job, None),
        layer("profile/pickup_location", Scope::Profile, None),
    ];
    let keys = |ls: Vec<&StackLayer>| -> Vec<String> {
        ls.into_iter().map(|l| l.layer_key.clone()).collect()
    };
    assert_eq!(
        keys(layer_stack(&layers, &layers[0])),
        vec![
            "base",
            "industry/retail",
            "industry_dept/retail/customer_service",
            "job/customer_care_agent"
        ]
    );
    // a job without an industry composes on base only
    assert_eq!(
        keys(layer_stack(&layers, &layers[5])),
        vec!["base", "job/faq_agent"]
    );
    assert_eq!(
        keys(composed_on_by(&layers, &layers[4])),
        vec![
            "industry_dept/retail/customer_service",
            "job/customer_care_agent"
        ]
    );
    assert_eq!(
        keys(composed_on_by(&layers, &layers[3])),
        vec![
            "industry/retail",
            "industry/utilities",
            "industry_dept/retail/customer_service",
            "job/customer_care_agent",
            "job/faq_agent"
        ]
    );
    // a profile layer is not a template layer
    assert_eq!(
        keys(layer_stack(&layers, &layers[6])),
        vec!["profile/pickup_location"]
    );
    assert!(composed_on_by(&layers, &layers[6]).is_empty());
}

#[test]
fn catch_up_counts_per_pinned_version() {
    let v1 = vec![
        item(Kind::Entity, "Order", "Order"),
        item(Kind::Entity, "Party", "Party"),
    ];
    let v2 = vec![
        item(Kind::Entity, "Order", "Purchase"),
        item(Kind::Entity, "Party", "Party"),
    ];
    let v3 = vec![
        item(Kind::Entity, "Order", "Purchase"),
        item(Kind::Entity, "Basket", "Basket"),
    ];
    let at = |v: i32| match v {
        1 => v1.clone(),
        2 => v2.clone(),
        3 => v3.clone(),
        _ => panic!("no tag v{v}"),
    };
    let got = catch_up(&[2, 1, 3, 1], 3, at);
    let counts: Vec<(i32, i32, usize, usize, usize)> = got
        .iter()
        .map(|c| (c.from, c.to, c.added, c.changed, c.removed))
        .collect();
    assert_eq!(counts, vec![(1, 3, 1, 1, 1), (2, 3, 1, 0, 1)]);
    assert!(catch_up(&[3], 3, |_| Vec::new()).is_empty());
}
