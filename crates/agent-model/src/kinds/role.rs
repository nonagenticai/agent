//! `agent_role` rows (P15 agent roles, 12 §8.2): the web `AgentRole` minus `job` and `role` (row layer and row
//! key), grants folded in (B6-2). The edit rules are ports of `editableRole`, `addAgentRoleGrant` and
//! `setAgentRoleMaxSideEffect` in `web/src/lib/api/areas/ontology.ts`: side effects rank R < W < X < P.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::catalog::OperationRow;

use super::catalog::{Modality, SideEffect};
use super::{FieldError, Refusal, done, require_text};

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRoleGrant {
    /// `order` (or `*` with `derive_from_activity`).
    pub object: String,
    pub verb: String,
    #[serde(default)]
    pub variant: Option<String>,
    pub modalities: Vec<Modality>,
    /// Procedure role: the grant is the activity's operations, never hand-authored.
    #[serde(default)]
    pub derive_from_activity: bool,
    /// D119: derived from the folded operation expectations by [`derive_role_grants`], not a stored row's grant.
    /// Distinct from `derive_from_activity` (which makes the role allow everything the activity runs).
    #[serde(default, skip_serializing_if = "is_false")]
    pub derived: bool,
}

impl AgentRoleGrant {
    /// `object.verb`
    pub fn op(&self) -> String {
        format!("{}.{}", self.object, self.verb)
    }
}

/// `agent_role` row fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRoleFields {
    pub label: String,
    #[serde(default)]
    pub used_by: String,
    pub max_side_effect: SideEffect,
    pub enabled: bool,
    #[serde(default)]
    pub grants: Vec<AgentRoleGrant>,
    #[serde(default)]
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl AgentRoleFields {
    /// The procedure role: its grants derive from each activity.
    pub fn derives_grants(&self) -> bool {
        self.grants.iter().any(|g| g.derive_from_activity)
    }
}

/// D119: the roles whose grants derive from the job's folded operation expectations when no layer sets them.
pub const DERIVED_ROLES: [&str; 2] = ["qualifier", "post_gate"];
/// The operation the post-gate always keeps (its own grant, never an expectation of the job).
const KNOWLEDGE_SEARCH: &str = "knowledge.search";

/// One folded `operation_expectation` (D119): the operation, whether the job requires it, and the variants its
/// summary lists (`required · {return, complaint}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedOp {
    pub op: String,
    pub required: bool,
    #[serde(default)]
    pub variants: Vec<String>,
}

impl ExpectedOp {
    /// An expectation row `key` / `summary` (`required · …` or `optional · …`).
    pub fn from_summary(op: &str, summary: &str) -> ExpectedOp {
        ExpectedOp {
            op: op.to_owned(),
            required: summary.trim_start().starts_with("required"),
            variants: variants_in(summary),
        }
    }
}

/// The variants an expectation summary lists (`required · {return, complaint}` gives `return`, `complaint`).
pub fn variants_in(summary: &str) -> Vec<String> {
    let Some(open) = summary.find('{') else {
        return Vec::new();
    };
    let Some(close) = summary[open..].find('}') else {
        return Vec::new();
    };
    summary[open + 1..open + close]
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*")
        .map(str::to_owned)
        .collect()
}

/// D119: the grants of a qualifier or post-gate role, derived from what the job expects. One observe grant per
/// expected operation that is observe-modality and within `max`; the post-gate also keeps `knowledge.search`, the
/// qualifier never has it. Any other role derives nothing. Derived grants carry `derived: true`.
pub fn derive_role_grants(
    role_key: &str,
    max: SideEffect,
    expectations: &[ExpectedOp],
    operations: &[OperationRow],
) -> Vec<AgentRoleGrant> {
    if !DERIVED_ROLES.contains(&role_key) {
        return Vec::new();
    }
    let grant = |op: &str| {
        let (object, verb) = op.split_once('.').unwrap_or((op, ""));
        AgentRoleGrant {
            object: object.to_owned(),
            verb: verb.to_owned(),
            variant: None,
            modalities: vec![Modality::Observe],
            derive_from_activity: false,
            derived: true,
        }
    };
    let mut ops: Vec<&str> = Vec::new();
    if role_key == "post_gate" {
        ops.push(KNOWLEDGE_SEARCH);
    }
    for e in expectations {
        let observed = operations
            .iter()
            .find(|o| o.key == e.op)
            .and_then(|o| o.typed_attrs().ok())
            .is_some_and(|a| {
                a.modality == Some(Modality::Observe) && a.side_effect.is_some_and(|s| s <= max)
            });
        if observed && e.op != KNOWLEDGE_SEARCH && !ops.contains(&e.op.as_str()) {
            ops.push(e.op.as_str());
        }
    }
    ops.into_iter().map(grant).collect()
}

/// `order.read`, `case.create{return}` (web `opLabel`).
pub fn op_label(op: &str, variant: Option<&str>) -> String {
    match variant {
        Some(v) if !v.is_empty() => format!("{op}{{{v}}}"),
        _ => op.to_owned(),
    }
}

/// A role whose grants may be hand-edited (web `editableRole`): 409 `derived_grants` for the procedure role,
/// 409 `role_disabled` for a disabled one.
pub fn editable(role: &AgentRoleFields) -> Result<(), Refusal> {
    if role.derives_grants() {
        return Err(Refusal::new(
            409,
            "derived_grants",
            "The procedure role’s grants are derived from each activity; edit the activity.",
        ));
    }
    if !role.enabled {
        return Err(Refusal::new(
            409,
            "role_disabled",
            format!("{} is disabled in v1.", role.label),
        ));
    }
    Ok(())
}

/// The grant `addAgentRoleGrant` would append, after `editable` and the caller's catalogue lookup
/// (`op_side_effect` is the operation's side effect; an unknown operation is the caller's 422
/// `unknown_operation`): 422 `exceeds_max_side_effect`, 422 `invalid_modalities`, 409 `grant_exists`.
pub fn check_add_grant(
    role: &AgentRoleFields,
    op: &str,
    op_side_effect: SideEffect,
    variant: Option<&str>,
    modalities: &[Modality],
) -> Result<AgentRoleGrant, Refusal> {
    if op_side_effect > role.max_side_effect {
        return Err(Refusal::new(
            422,
            "exceeds_max_side_effect",
            format!(
                "{op} has side effect {op_side_effect}; {} allows at most {}.",
                role.label, role.max_side_effect
            ),
        ));
    }
    if modalities.is_empty() {
        return Err(Refusal::new(
            422,
            "invalid_modalities",
            "Pick at least one modality.",
        ));
    }
    let (object, verb) = op.split_once('.').unwrap_or((op, ""));
    let exists = role
        .grants
        .iter()
        .any(|g| g.object == object && g.verb == verb && g.variant.as_deref() == variant);
    if exists {
        return Err(Refusal::new(
            409,
            "grant_exists",
            format!("{} already has {}.", role.label, op_label(op, variant)),
        ));
    }
    Ok(AgentRoleGrant {
        object: object.to_owned(),
        verb: verb.to_owned(),
        variant: variant.map(str::to_owned),
        modalities: modalities.to_vec(),
        derive_from_activity: false,
        derived: false,
    })
}

/// Lowering the role's maximum (web `setAgentRoleMaxSideEffect`, after `editable`): every grant above `max` must
/// go first, 422 `grants_exceed_max`. `side_effect_of(object.verb)` looks an operation up; an unknown one counts
/// as R, as the web does.
pub fn check_max_side_effect(
    role: &AgentRoleFields,
    max: SideEffect,
    side_effect_of: impl Fn(&str) -> Option<SideEffect>,
) -> Result<(), Refusal> {
    let over: Vec<String> = role
        .grants
        .iter()
        .filter(|g| side_effect_of(g.op().as_str()).unwrap_or(SideEffect::R) > max)
        .map(|g| op_label(&g.op(), g.variant.as_deref()))
        .collect();
    if over.is_empty() {
        return Ok(());
    }
    let verb = if over.len() == 1 { "it is" } else { "they are" };
    Err(Refusal::new(
        422,
        "grants_exceed_max",
        format!("Remove {} first; {verb} above {max}.", over.join(", ")),
    ))
}

/// The stored role: a label, every grant with a modality, no grant twice.
pub(crate) fn check(role: &AgentRoleFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&role.label, "label", &mut errors);
    let mut seen = BTreeSet::new();
    for (i, g) in role.grants.iter().enumerate() {
        if g.modalities.is_empty() {
            errors.push(FieldError::at(
                "invalid_modalities",
                format!("grants[{i}].modalities"),
                "Pick at least one modality.",
            ));
        }
        if !seen.insert((g.object.as_str(), g.verb.as_str(), g.variant.as_deref())) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("grants[{i}]"),
                format!(
                    "{} is granted twice",
                    op_label(&g.op(), g.variant.as_deref())
                ),
            ));
        }
    }
    done(errors)
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, validate_fields};
    use super::*;
    use serde_json::json;

    fn grant(op: &str, variant: Option<&str>) -> AgentRoleGrant {
        let (object, verb) = op.split_once('.').expect("object.verb");
        AgentRoleGrant {
            object: object.to_owned(),
            verb: verb.to_owned(),
            variant: variant.map(str::to_owned),
            modalities: vec![Modality::Observe],
            derive_from_activity: false,
            derived: false,
        }
    }

    /// `rolesFor(customer_care_agent, …)` in `web/src/lib/fixtures/ontology.ts`.
    fn qualifier() -> AgentRoleFields {
        AgentRoleFields {
            label: "Qualifier".into(),
            used_by: "edge (chat_q)".into(),
            max_side_effect: SideEffect::R,
            enabled: true,
            grants: vec![grant("party.list", None), grant("order.list", None)],
            note: "Slot pre-fill; never writes.".into(),
            updated_by: None,
            updated_at: None,
            summary: None,
        }
    }

    fn procedure() -> AgentRoleFields {
        AgentRoleFields {
            label: "Procedure".into(),
            max_side_effect: SideEffect::P,
            grants: vec![AgentRoleGrant {
                object: "*".into(),
                verb: "*".into(),
                variant: None,
                modalities: vec![Modality::Do, Modality::Request, Modality::Observe],
                derive_from_activity: true,
                derived: false,
            }],
            ..qualifier()
        }
    }

    fn assistant() -> AgentRoleFields {
        AgentRoleFields {
            label: "Assistant".into(),
            max_side_effect: SideEffect::W,
            enabled: false,
            grants: vec![],
            ..qualifier()
        }
    }

    fn code(r: Result<impl Sized, Refusal>) -> Option<(u16, &'static str)> {
        r.err().map(|e| (e.status, e.code))
    }

    #[test]
    fn editable_refuses_derived_and_disabled_roles() {
        assert_eq!(code(editable(&procedure())), Some((409, "derived_grants")));
        assert_eq!(code(editable(&assistant())), Some((409, "role_disabled")));
        assert_eq!(
            editable(&assistant()).err().map(|e| e.message),
            Some("Assistant is disabled in v1.".to_owned())
        );
        assert_eq!(editable(&qualifier()), Ok(()));
    }

    #[test]
    fn grants_respect_the_side_effect_rank() {
        let q = qualifier();
        let refused = check_add_grant(&q, "order.cancel", SideEffect::X, None, &[Modality::Do]);
        assert_eq!(
            code(refused.clone()),
            Some((422, "exceeds_max_side_effect"))
        );
        assert_eq!(
            refused.err().map(|e| e.message),
            Some("order.cancel has side effect X; Qualifier allows at most R.".to_owned())
        );
        assert_eq!(
            code(check_add_grant(
                &q,
                "delivery.read",
                SideEffect::R,
                None,
                &[]
            )),
            Some((422, "invalid_modalities"))
        );
        assert_eq!(
            code(check_add_grant(
                &q,
                "party.list",
                SideEffect::R,
                None,
                &[Modality::Observe]
            )),
            Some((409, "grant_exists"))
        );
        let added = check_add_grant(
            &q,
            "delivery.read",
            SideEffect::R,
            Some("by_tracking_ref"),
            &[Modality::Observe],
        )
        .expect("grant");
        assert_eq!(added, grant("delivery.read", Some("by_tracking_ref")));
        assert_eq!(
            op_label("case.create", Some("return")),
            "case.create{return}"
        );
        assert_eq!(op_label("order.read", None), "order.read");
    }

    #[test]
    fn lowering_max_needs_grants_removed() {
        let mut role = qualifier();
        role.max_side_effect = SideEffect::P;
        role.grants.push(grant("order.cancel", None));
        role.grants.push(grant("payment.refund", Some("full")));
        let catalogue = |op: &str| match op {
            "order.cancel" => Some(SideEffect::X),
            "payment.refund" => Some(SideEffect::P),
            _ => None,
        };
        let refused = check_max_side_effect(&role, SideEffect::W, catalogue);
        assert_eq!(code(refused.clone()), Some((422, "grants_exceed_max")));
        assert_eq!(
            refused.err().map(|e| e.message),
            Some("Remove order.cancel, payment.refund{full} first; they are above W.".to_owned())
        );
        assert_eq!(
            check_max_side_effect(&role, SideEffect::X, catalogue)
                .err()
                .map(|e| e.message),
            Some("Remove payment.refund{full} first; it is above X.".to_owned())
        );
        assert_eq!(
            check_max_side_effect(&role, SideEffect::P, catalogue),
            Ok(())
        );
    }

    fn operations() -> Vec<OperationRow> {
        let defs: Vec<_> = [
            ("order.list", "observe", "R"),
            ("knowledge.search", "observe", "R"),
            ("order.cancel", "do", "X"),
            ("payment.list", "observe", "W"),
        ]
        .iter()
        .map(|(key, modality, side_effect)| {
            crate::catalog::OperationDefRow::from_row(
                key,
                &json!({"attrs": {"modality": modality, "side_effect": side_effect}}),
            )
            .expect("operation")
        })
        .collect();
        crate::catalog::resolve_operations(&defs, &[])
    }

    fn expected(op: &str, summary: &str) -> ExpectedOp {
        ExpectedOp::from_summary(op, summary)
    }

    #[test]
    fn expectation_summaries_parse() {
        let e = expected("case.create", "required \u{b7} {return, complaint, *}");
        assert!(e.required);
        assert_eq!(e.variants, ["return", "complaint"]);
        let o = expected("order.list", "optional");
        assert!(!o.required && o.variants.is_empty());
    }

    #[test]
    fn grants_derive_from_observe_expectations_within_the_max() {
        let want = [
            expected("order.list", "required"),
            expected("order.cancel", "optional"),
            expected("payment.list", "optional"),
            expected("knowledge.search", "required"),
            expected("unknown.read", "optional"),
        ];
        let ops = operations();
        let names = |g: Vec<AgentRoleGrant>| -> Vec<String> { g.iter().map(|g| g.op()).collect() };
        // observe, within R: order.list only; the post-gate adds knowledge.search first
        assert_eq!(
            names(derive_role_grants("qualifier", SideEffect::R, &want, &ops)),
            ["order.list"]
        );
        let post = derive_role_grants("post_gate", SideEffect::R, &want, &ops);
        assert_eq!(names(post.clone()), ["knowledge.search", "order.list"]);
        assert!(post.iter().all(|g| g.derived && !g.derive_from_activity));
        assert_eq!(post[1].modalities, [Modality::Observe]);
        // a higher max admits the observe operation with a W side effect, never the do-modality one
        assert_eq!(
            names(derive_role_grants("qualifier", SideEffect::W, &want, &ops)),
            ["order.list", "payment.list"]
        );
        // other roles derive nothing
        assert!(derive_role_grants("procedure", SideEffect::P, &want, &ops).is_empty());
        assert!(derive_role_grants("assistant", SideEffect::W, &want, &ops).is_empty());
    }

    #[test]
    fn agent_role_rows() {
        let row = serde_json::to_value(procedure()).expect("role");
        assert_eq!(validate_fields(Kind::AgentRole, &row), Ok(()));
        let fixture = json!({
            "label": "Assistant",
            "used_by": "interaction",
            "max_side_effect": "W",
            "enabled": false,
            "grants": [],
            "note": "Defined, disabled in v1.",
            "updated_by": "u-owner",
            "updated_at": "2026-10-12T09:00:00Z"
        });
        assert_eq!(validate_fields(Kind::AgentRole, &fixture), Ok(()));
        let mut dup = serde_json::to_value(qualifier()).expect("role");
        dup["grants"][1] = dup["grants"][0].clone();
        dup["grants"][0]["modalities"] = json!([]);
        let codes: Vec<&str> = validate_fields(Kind::AgentRole, &dup)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.code)
            .collect();
        assert_eq!(codes, vec!["invalid_modalities", "invalid_value"]);
    }
}
