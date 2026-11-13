//! B7 T12: the P17 template diff, upgrade verdict and fault classification — a port of TS `diffTemplates`,
//! `upgradeVerdict`, `classifyCode` and `idempotencyLabel` (`web/src/lib/connectors/P17-diff.ts`, `ops.ts`) with
//! identical results. Rules: doc 10 §4.2 (template contents, pin semantics), doc 11 §6.1
//! pinning rule 4 (explicit re-pin, contract tests re-run), doc 10 §3.1 (system errors escalate, user outcomes reply).
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::instances::cmp_version;
use super::model::{ConnectorTemplate, ConnectorTemplateOp, Idempotency};
use super::opkey::op_key;

/// TS `idempotencyLabel`.
pub fn idempotency_label(i: &Idempotency) -> String {
    match i {
        Idempotency::Nat => "natural (read)".to_owned(),
        Idempotency::Header { header } => format!("header {header}"),
        Idempotency::DupSearch { on } => format!("dup_search on {on}"),
        Idempotency::None => "none".to_owned(),
    }
}

/// `OpDiff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpDiff {
    pub op: String,
    /// `added` | `removed` | `changed` | `same`.
    pub change: String,
    pub details: Vec<String>,
}

/// One template-level change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaChange {
    pub label: String,
    pub before: String,
    pub after: String,
}

/// `Record<OpChange, number>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub added: u32,
    pub removed: u32,
    pub changed: u32,
    pub same: u32,
}

/// `TemplateDiff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateDiff {
    pub against: Option<String>,
    pub meta: Vec<MetaChange>,
    pub ops: Vec<OpDiff>,
    pub counts: Counts,
}

/// The overrides half of `TemplateInstanceUse` (`{op, field}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideRef {
    pub op: String,
    pub field: String,
}

/// The parts of `TemplateInstanceUse` the verdict reads (the TS object carries more; serde ignores it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceUse {
    pub pinned_version: String,
    #[serde(default)]
    pub routed_ops: Vec<String>,
    #[serde(default)]
    pub overrides: Vec<OverrideRef>,
}

/// `UpgradeVerdict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpgradeVerdict {
    Current { note: String },
    CanUpgrade { to: String, warnings: Vec<String> },
    Blocked { to: String, reasons: Vec<String> },
    NoTarget { note: String },
}

/// `FaultClass.handling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handling {
    Reply,
    Escalate,
    Unknown,
}

/// One `error_state_def` row as the classifier reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorState {
    pub code: String,
    pub user_facing: bool,
    pub description: String,
}

/// TS `classifyCode(...).handling`: `invalid_state:in_transit` → `invalid_state`; not in the taxonomy → unknown.
pub fn classify_code(code: &str, taxonomy: &[ErrorState]) -> Handling {
    let base = code.split(':').next().unwrap_or_default().trim();
    match taxonomy.iter().find(|d| d.code == base) {
        None => Handling::Unknown,
        Some(d) if d.user_facing => Handling::Reply,
        Some(_) => Handling::Escalate,
    }
}

/// A JS `Map` built from pairs: first insertion fixes the position, a later pair overwrites the value.
fn js_map<V: Clone>(pairs: impl Iterator<Item = (String, V)>) -> Vec<(String, V)> {
    let mut out: Vec<(String, V)> = Vec::new();
    for (k, v) in pairs {
        match out.iter_mut().find(|(x, _)| *x == k) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    out
}

fn get<'m, V>(m: &'m [(String, V)], k: &str) -> Option<&'m V> {
    m.iter().find(|(x, _)| x == k).map(|(_, v)| v)
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// TS `normPath`: every `\b\d{4}-\d{2}\b` (a vendor API version segment) → `{api}`.
fn norm_path(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = String::with_capacity(p.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        let hit = i + 7 <= b.len()
            && (i == 0 || !is_word(b[i - 1]))
            && b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'-'
            && b[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && (i + 7 == b.len() || !is_word(b[i + 7]));
        if hit {
            out.push_str(&p[copied..i]);
            out.push_str("{api}");
            i += 7;
            copied = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&p[copied..]);
    out
}

fn op_details(a: &ConnectorTemplateOp, b: &ConnectorTemplateOp) -> Vec<String> {
    let mut out = Vec::new();
    if a.method != b.method || norm_path(&a.path) != norm_path(&b.path) {
        out.push(format!(
            "endpoint {} {} \u{2192} {} {}",
            a.method, a.path, b.method, b.path
        ));
    }
    if a.request != b.request {
        out.push("request template rewritten".to_owned());
    }
    let fa = js_map(a.field_map.iter().map(|f| (f.field.clone(), f)));
    let fb = js_map(b.field_map.iter().map(|f| (f.field.clone(), f)));
    for (field, f) in &fb {
        match get(&fa, field) {
            None => out.push(format!("+ maps {field} \u{2190} {}", f.path)),
            Some(old) => {
                let ta = old.transform.as_deref().unwrap_or("");
                let tb = f.transform.as_deref().unwrap_or("");
                if old.path != f.path || ta != tb {
                    out.push(format!("~ {field}: {} \u{2192} {}", old.path, f.path));
                }
            }
        }
    }
    for (field, _) in &fa {
        if get(&fb, field).is_none() {
            out.push(format!("\u{2212} no longer maps {field}"));
        }
    }
    let sa = js_map(
        a.status_map
            .iter()
            .map(|s| (s.system.clone(), s.canonical.clone())),
    );
    let sb = js_map(
        b.status_map
            .iter()
            .map(|s| (s.system.clone(), s.canonical.clone())),
    );
    for (sys, can) in &sb {
        match get(&sa, sys) {
            None => out.push(format!("+ status {sys} \u{2192} {can}")),
            Some(old) if old != can => {
                out.push(format!("~ status {sys}: {old} \u{2192} {can}"));
            }
            Some(_) => {}
        }
    }
    for (sys, _) in &sa {
        if get(&sb, sys).is_none() {
            out.push(format!("\u{2212} status {sys} unmapped"));
        }
    }
    let ka = js_map(
        a.fault_map
            .iter()
            .map(|f| (f.system.clone(), f.code.clone())),
    );
    let kb = js_map(
        b.fault_map
            .iter()
            .map(|f| (f.system.clone(), f.code.clone())),
    );
    for (sys, code) in &kb {
        if get(&ka, sys) != Some(code) {
            out.push(format!("+ fault {sys} \u{2192} {code}"));
        }
    }
    for (sys, _) in &ka {
        if get(&kb, sys).is_none() {
            out.push(format!("\u{2212} fault {sys} unmapped"));
        }
    }
    let (ia, ib) = (
        idempotency_label(&a.idempotency),
        idempotency_label(&b.idempotency),
    );
    if ia != ib {
        out.push(format!("idempotency {ia} \u{2192} {ib}"));
    }
    if a.verify_read != b.verify_read {
        out.push(format!(
            "verify_read {} \u{2192} {}",
            a.verify_read.as_deref().unwrap_or("\u{2014}"),
            b.verify_read.as_deref().unwrap_or("\u{2014}")
        ));
    }
    if a.pagination != b.pagination {
        out.push(format!(
            "pagination {} \u{2192} {}",
            a.pagination.as_deref().unwrap_or("none"),
            b.pagination.as_deref().unwrap_or("none")
        ));
    }
    if a.timeout_ms != b.timeout_ms {
        out.push(format!(
            "timeout {} \u{2192} {} ms",
            a.timeout_ms, b.timeout_ms
        ));
    }
    if a.side_effect != b.side_effect {
        out.push(format!(
            "side effect {} \u{2192} {}",
            a.side_effect.as_str(),
            b.side_effect.as_str()
        ));
    }
    out
}

/// A serde literal (`auth_kind`, `transport`) as its TS string.
fn lit<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// `rate_limit_model.value` as text.
fn rate_value(v: &Value) -> String {
    v.get("value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Webhook topics joined by `, `.
fn topics(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|w| w.get("topic").and_then(Value::as_str).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// TS `diffTemplates`: diff `cur` against `prev` (the next older version); with no previous every op is `added`.
pub fn diff_templates(prev: Option<&ConnectorTemplate>, cur: &ConnectorTemplate) -> TemplateDiff {
    let mut meta = Vec::new();
    let mut ops = Vec::new();
    if let Some(prev) = prev {
        let mut m = |label: &str, a: String, b: String| {
            if a != b {
                let dash = |s: String| {
                    if s.is_empty() {
                        "\u{2014}".to_owned()
                    } else {
                        s
                    }
                };
                meta.push(MetaChange {
                    label: label.to_owned(),
                    before: dash(a),
                    after: dash(b),
                });
            }
        };
        m(
            "Vendor API pin",
            prev.vendor_api_pin.clone(),
            cur.vendor_api_pin.clone(),
        );
        m(
            "Auth",
            format!("{} \u{b7} {}", lit(&prev.auth_kind), prev.auth_detail),
            format!("{} \u{b7} {}", lit(&cur.auth_kind), cur.auth_detail),
        );
        let names = |t: &ConnectorTemplate| {
            t.credential_schema
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        m("Credential fields", names(prev), names(cur));
        m(
            "Rate limit",
            rate_value(&prev.rate_limit_model),
            rate_value(&cur.rate_limit_model),
        );
        m(
            "Webhooks (inert)",
            topics(&prev.webhooks),
            topics(&cur.webhooks),
        );
        m("Transport", lit(&prev.transport), lit(&cur.transport));
    }
    let before = js_map(
        prev.map_or(&[][..], |p| p.ops.as_slice())
            .iter()
            .map(|o| (op_key(&o.op_ref()), o)),
    );
    for o in &cur.ops {
        let k = op_key(&o.op_ref());
        match get(&before, &k) {
            None => ops.push(OpDiff {
                op: k,
                change: "added".to_owned(),
                details: Vec::new(),
            }),
            Some(old) => {
                let details = op_details(old, o);
                let change = if details.is_empty() {
                    "same"
                } else {
                    "changed"
                };
                ops.push(OpDiff {
                    op: k,
                    change: change.to_owned(),
                    details,
                });
            }
        }
    }
    let now: Vec<String> = cur.ops.iter().map(|o| op_key(&o.op_ref())).collect();
    for (k, _) in &before {
        if !now.contains(k) {
            ops.push(OpDiff {
                op: k.clone(),
                change: "removed".to_owned(),
                details: Vec::new(),
            });
        }
    }
    let mut counts = Counts::default();
    for o in &ops {
        match o.change.as_str() {
            "added" => counts.added += 1,
            "removed" => counts.removed += 1,
            "changed" => counts.changed += 1,
            _ => counts.same += 1,
        }
    }
    TemplateDiff {
        against: prev.map(|p| p.version.clone()),
        meta,
        ops,
        counts,
    }
}

/// TS `op.replace(/\{[^}]*\}$/, '')`: the op key without a trailing `{variant}`.
fn strip_variant(op: &str) -> &str {
    let Some(body) = op.strip_suffix('}') else {
        return op;
    };
    let from = body.rfind('}').map_or(0, |i| i + 1);
    match body[from..].find('{') {
        Some(i) => &op[..from + i],
        None => op,
    }
}

/// TS `upgradeVerdict`: can this system move to the newest active version? Dropping a routed op blocks; an
/// override on a field the target no longer maps warns; a re-pin always re-runs T3.
pub fn upgrade_verdict(inst: &InstanceUse, latest: Option<&ConnectorTemplate>) -> UpgradeVerdict {
    let Some(latest) = latest else {
        return UpgradeVerdict::NoTarget {
            note: "No active version of this template exists to re-pin to.".to_owned(),
        };
    };
    match cmp_version(&inst.pinned_version, &latest.version) {
        std::cmp::Ordering::Equal => {
            return UpgradeVerdict::Current {
                note: "On the newest active version.".to_owned(),
            };
        }
        std::cmp::Ordering::Greater => {
            return UpgradeVerdict::Current {
                note: format!("Ahead of the newest active version (@{}).", latest.version),
            };
        }
        std::cmp::Ordering::Less => {}
    }
    let has: Vec<String> = latest.ops.iter().map(|o| op_key(&o.op_ref())).collect();
    let reasons: Vec<String> = inst
        .routed_ops
        .iter()
        .filter(|op| {
            let base = strip_variant(op.as_str());
            !has.iter().any(|h| h == *op || h.as_str() == base)
        })
        .map(|op| format!("@{} drops {op}, which a job routes here", latest.version))
        .collect();
    if !reasons.is_empty() {
        return UpgradeVerdict::Blocked {
            to: latest.version.clone(),
            reasons,
        };
    }
    let mut warnings = Vec::new();
    for ov in &inst.overrides {
        let kept = latest
            .ops
            .iter()
            .find(|o| op_key(&o.op_ref()) == ov.op)
            .is_some_and(|o| o.field_map.iter().any(|f| f.field == ov.field));
        if !kept {
            warnings.push(format!("override {}.{} would be dropped", ov.op, ov.field));
        }
    }
    warnings.push("T3 contract tests re-run on re-pin".to_owned());
    UpgradeVerdict::CanUpgrade {
        to: latest.version.clone(),
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tpl(version: &str, ops: Value) -> ConnectorTemplate {
        serde_json::from_value(json!({
            "key": "acme", "version": version, "transport": "rest", "auth_kind": "api_key_header",
            "status": "active", "rate_limit_model": {"kind": "none", "value": "none"}, "webhooks": [],
            "ops": ops
        }))
        .expect("template")
    }

    fn op(verb: &str, variant: &str, path: &str, fields: Value) -> Value {
        json!({
            "object": "case", "verb": verb, "variant": variant, "side_effect": "R", "method": "GET",
            "path": path, "request": "", "field_map": fields, "status_map": [], "fault_map": [],
            "idempotency": {"kind": "nat"}, "timeout_ms": 1000, "fixtures": []
        })
    }

    #[test]
    fn api_segment_is_a_pin_not_an_op_change() {
        assert_eq!(
            norm_path("/admin/api/2026-07/orders/x2026-07"),
            "/admin/api/{api}/orders/x2026-07"
        );
        let a = tpl(
            "1",
            json!([op(
                "read",
                "*",
                "/api/2026-07/c",
                json!([{"path": "$.a", "field": "status"}])
            )]),
        );
        let b = tpl(
            "2",
            json!([
                op(
                    "read",
                    "*",
                    "/api/2026-10/c",
                    json!([{"path": "$.b", "field": "status"}, {"path": "$.d", "field": "due_at"}])
                ),
                op("create", "return", "/c", json!([]))
            ]),
        );
        let d = diff_templates(Some(&a), &b);
        assert_eq!(d.against.as_deref(), Some("1"));
        assert!(d.meta.is_empty());
        assert_eq!(d.ops[0].change, "changed");
        assert_eq!(
            d.ops[0].details,
            vec![
                "~ status: $.a \u{2192} $.b".to_owned(),
                "+ maps due_at \u{2190} $.d".to_owned()
            ]
        );
        assert_eq!(d.ops[1].change, "added");
        assert_eq!(
            d.counts,
            Counts {
                added: 1,
                changed: 1,
                ..Counts::default()
            }
        );
        let back = diff_templates(Some(&b), &a);
        assert_eq!(back.ops[1].op, "case.create{return}");
        assert_eq!(back.ops[1].change, "removed");
        assert!(
            diff_templates(None, &a)
                .ops
                .iter()
                .all(|o| o.change == "added")
        );
    }

    #[test]
    fn verdicts() {
        let latest = tpl(
            "2",
            json!([op(
                "read",
                "*",
                "/c",
                json!([{"path": "$.a", "field": "status"}])
            )]),
        );
        let use_ = |v: &str, routed: &[&str], ov: &[(&str, &str)]| InstanceUse {
            pinned_version: v.to_owned(),
            routed_ops: routed.iter().map(|s| (*s).to_owned()).collect(),
            overrides: ov
                .iter()
                .map(|(o, f)| OverrideRef {
                    op: (*o).to_owned(),
                    field: (*f).to_owned(),
                })
                .collect(),
        };
        assert!(matches!(
            upgrade_verdict(&use_("1", &[], &[]), None),
            UpgradeVerdict::NoTarget { .. }
        ));
        assert!(matches!(
            upgrade_verdict(&use_("2", &[], &[]), Some(&latest)),
            UpgradeVerdict::Current { .. }
        ));
        assert_eq!(
            upgrade_verdict(&use_("1", &["case.create"], &[]), Some(&latest)),
            UpgradeVerdict::Blocked {
                to: "2".into(),
                reasons: vec!["@2 drops case.create, which a job routes here".into()]
            }
        );
        // a variant route is served by the target's `*` op
        assert_eq!(strip_variant("case.read{x}"), "case.read");
        assert_eq!(strip_variant("a{b{c}"), "a");
        assert_eq!(strip_variant("a{b}c}"), "a{b}c}");
        assert_eq!(
            upgrade_verdict(
                &use_("1", &["case.read{x}"], &[("case.read", "due_at")]),
                Some(&latest)
            ),
            UpgradeVerdict::CanUpgrade {
                to: "2".into(),
                warnings: vec![
                    "override case.read.due_at would be dropped".into(),
                    "T3 contract tests re-run on re-pin".into()
                ]
            }
        );
        let tax = vec![
            ErrorState {
                code: "not_found".into(),
                user_facing: true,
                description: String::new(),
            },
            ErrorState {
                code: "upstream_unavailable".into(),
                user_facing: false,
                description: String::new(),
            },
        ];
        assert_eq!(classify_code("not_found", &tax), Handling::Reply);
        assert_eq!(
            classify_code("upstream_unavailable:x", &tax),
            Handling::Escalate
        );
        assert_eq!(classify_code("odd", &tax), Handling::Unknown);
    }
}
