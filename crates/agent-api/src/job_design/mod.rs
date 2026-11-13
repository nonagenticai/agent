//! Job design (B5): the job's ontology (J11), context spaces and procedures (J12), gates (J7, answered by Laya) and
//! toolset narrowings (J8). Every tenant write is a D54 draft through B2's engine (`crate::design::track`); bases are
//! read at the design's pins (B6a `compile_pinned`, B2's release bundle), never copied into B5 tables (plan R1).
//!
//! B5 has no catalogue or route trait of its own: operations come from B4's
//! [`OperationCatalog`](crate::runtime::catalog::OperationCatalog) (the job's pinned compile) and routes from B4's
//! [`RouteResolver`](crate::runtime::routes::RouteResolver). This module adds the web's operation-label helpers
//! (`opLabel`, `splitOp`, `grantCovers` of `areas/ontology.ts`) and the N1 list J8 shows
//! ([`catalogue_operations`]). The web `SIDE_EFFECT_RANK` is agent-model's `SideEffect::rank`.
pub mod base;
pub mod conflicts;
pub mod context_space;
pub mod gates;
pub mod keys;
pub mod ontology;
pub mod procedures;
pub mod readiness;
pub mod role_switch;
pub mod runtime;
pub mod targets;
pub mod toolset;
pub mod types;

use agent_model::catalog::OperationRow;
use agent_model::kinds::catalog::{IdentityLevel, Modality, SideEffect};

use self::types::CatalogueOperation;

/// `order.read`, `case.create{return}` (web `opLabel`).
pub fn op_label(op: &str, variant: Option<&str>) -> String {
    match variant {
        Some(v) if !v.is_empty() => format!("{op}{{{v}}}"),
        _ => op.to_owned(),
    }
}

/// `case.create{return}` → (`case.create`, `return`); a label without a variant → (label, None) (web `splitOp`,
/// `^([^{]+)(?:\{(.+)\})?$`; a label that does not match comes back whole).
pub fn split_op(label: &str) -> (&str, Option<&str>) {
    match label.find('{') {
        Some(i) if i > 0 && label.len() > i + 2 && label.ends_with('}') => {
            (&label[..i], Some(&label[i + 1..label.len() - 1]))
        }
        _ => (label, None),
    }
}

/// The read-only runtime builtins every activity grants the procedure role without listing them: knowledge reads
/// and `rule.evaluate` (a pure comparison of facts already read by authorized steps). A tenant admin cannot edit an activity's grant, so a
/// procedure step of one must not depend on it; the static knowledge plan runs the same operation. No write builtin
/// is implicit. A tenant's narrowing (N4) still applies at runtime.
pub fn implicit_read_builtin(label: &str) -> bool {
    matches!(
        split_op(label).0,
        "knowledge.search" | "knowledge.answer" | "rule.evaluate"
    )
}

/// Whether `grant` (an activity's `object.verb{variant}` entries) covers step `label`: a [`grant_covers`] entry, or
/// an [`implicit_read_builtin`].
pub fn grant_allows(grant: &[String], label: &str) -> bool {
    implicit_read_builtin(label) || grant.iter().any(|g| grant_covers(g, label))
}

/// A grant entry `object.verb{variant}` covers step `op{variant}` when they are equal, or the op matches and the
/// grant has no variant (web `grantCovers`).
pub fn grant_covers(grant: &str, step: &str) -> bool {
    if grant == step {
        return true;
    }
    let (grant_op, grant_variant) = split_op(grant);
    let (step_op, _) = split_op(step);
    grant_op == step_op && grant_variant.is_none()
}

/// The wire value of an identity level (`0`, `1`, `2`, `3`, `S`).
fn identity_label(level: IdentityLevel) -> String {
    match level {
        IdentityLevel::Public => "0",
        IdentityLevel::Identified => "1",
        IdentityLevel::Verified => "2",
        IdentityLevel::StepUp => "3",
        IdentityLevel::Staff => "S",
    }
    .to_owned()
}

/// The job's operations as J8 lists them (web `CatalogueOperation`), from B6a's resolved rows
/// (`Compiled.catalogue.operations`), in row order. Unset columns take B4's defaults
/// ([`OpDef`](crate::runtime::catalog::OpDef): side effect `P`, identity level `2`) and modality `do`, so an
/// unknown operation is never shown as a read. A row whose attributes do not parse is left out, as B4 does.
pub fn catalogue_operations(rows: &[OperationRow]) -> Vec<CatalogueOperation> {
    rows.iter()
        .filter_map(|row| {
            let attrs = row.typed_attrs().ok()?;
            // the level the runner applies: the stricter of the pinned row's and the contract's floor
            let level = crate::runtime::catalog::effective_level(&row.key, attrs.identity_level)?;
            Some(CatalogueOperation {
                op: format!("{}.{}", row.object, row.verb),
                variant: (row.variant != agent_model::catalog::STAR).then(|| row.variant.clone()),
                modality: attrs.modality.unwrap_or(Modality::Do),
                side_effect: attrs.side_effect.unwrap_or(SideEffect::P),
                identity_level: identity_label(level),
            })
        })
        .collect()
}

/// [`catalogue_operations`] plus the runtime built-ins the job's rows lack (`knowledge.search`, `knowledge.answer`,
/// `rule.evaluate`: [`builtin_family`](crate::runtime::routes::builtin_family)). They run inside the
/// agent for every job, with no connector, so a procedure step may name them whether or not the job's pinned compile
/// carries a row for them. A row of the job wins (same op and variant); effect and level come from the built-in table.
pub fn catalogue_with_builtins(rows: &[OperationRow]) -> Vec<CatalogueOperation> {
    let mut ops = catalogue_operations(rows);
    for d in crate::runtime::catalog::builtin_defs() {
        if crate::runtime::routes::builtin_family(&d.key).is_none() {
            continue;
        }
        let variant = (d.variant != agent_model::catalog::STAR).then(|| d.variant.clone());
        let op = format!("{}.{}", d.object, d.verb);
        if ops.iter().any(|o| o.op == op && o.variant == variant) {
            continue;
        }
        ops.push(CatalogueOperation {
            op,
            variant,
            modality: if d.side_effect == SideEffect::R {
                Modality::Observe
            } else {
                Modality::Do
            },
            side_effect: d.side_effect,
            identity_level: identity_label(d.identity_level),
        });
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_labels_split_and_join() {
        assert_eq!(op_label("order.read", None), "order.read");
        assert_eq!(
            op_label("case.create", Some("return")),
            "case.create{return}"
        );
        assert_eq!(
            split_op("case.create{return}"),
            ("case.create", Some("return"))
        );
        assert_eq!(split_op("order.read"), ("order.read", None));
        assert_eq!(split_op("case.create{}"), ("case.create{}", None));
        assert_eq!(split_op("{x}"), ("{x}", None));
        for (op, variant) in [("a.b", None), ("rule.evaluate", Some("RET-01"))] {
            let label = op_label(op, variant);
            assert_eq!(split_op(&label), (op, variant));
        }
    }

    /// Readiness and procedure steps read the level the runner applies: a contract op stored below its contract
    /// level is listed at the contract's.
    #[test]
    fn the_catalogue_lists_the_effective_level() {
        let row: OperationRow = serde_json::from_value(serde_json::json!({
            "modality": "observe", "side_effect": "R", "identity_level": "1",
            "input_fields": ["order_id", "email", "postal_code"], "output_fields": ["identity_level"],
            "key": "order.read", "object": "order", "verb": "read", "variant": "*",
            "discriminator": null, "overridden": [], "inherited": [], "description": "rev 1",
            "retail": null, "stages": [], "error_states": []
        }))
        .unwrap();
        // a contract op stored below its contract level (order.read is 2) is listed at the contract's level: the
        // catalogue never shows, and the runner never applies, a lowered level
        assert_eq!(
            catalogue_operations(std::slice::from_ref(&row))[0].identity_level,
            "2"
        );
        // an operation the contract does not list keeps the level its row declares
        let mut custom = row;
        custom.key = "zz_custom.thing".into();
        custom.object = "zz_custom".into();
        custom.verb = "thing".into();
        assert_eq!(catalogue_operations(&[custom])[0].identity_level, "1");
    }

    /// A job with no operation rows still offers the runtime built-ins, and only those (not `case.create{handover}`).
    #[test]
    fn builtins_are_in_every_procedure_catalogue() {
        let ops = catalogue_with_builtins(&[]);
        let find = |op: &str| ops.iter().find(|o| o.op == op);
        let answer = find("knowledge.answer").expect("knowledge.answer");
        assert_eq!(answer.side_effect, SideEffect::R);
        assert_eq!(answer.modality, Modality::Observe);
        assert_eq!(answer.identity_level, "0");
        assert!(find("knowledge.search").is_some() && find("rule.evaluate").is_some());
        assert!(find("case.create").is_none());
        assert!(find("order.read").is_none());
    }

    #[test]
    fn grants_cover_as_the_web() {
        assert!(grant_covers("case.create", "case.create{return}"));
        assert!(grant_covers("case.create{return}", "case.create{return}"));
        assert!(!grant_covers(
            "case.create{return}",
            "case.create{complaint}"
        ));
        assert!(!grant_covers("case.create{return}", "case.create"));
        assert!(!grant_covers("case.read", "case.create"));
    }
}
