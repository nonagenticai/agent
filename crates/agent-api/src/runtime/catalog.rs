//! The operation catalogue (B4 R9): the one place the runtime learns what an operation is — its side effect
//! (R/W/X/P), identity level, idempotency, verify read, inputs and outputs. B5 and B7 read it through
//! [`OperationCatalog`]; there is no second catalogue trait.
//!
//! [`PinnedCatalog`] is built from B6a's pinned compile of the job (`platform::templates::compile_pinned(..)
//! .catalogue.operations`, resolved: every variant carries its `*` row's columns) plus a small static table of the
//! built-ins (`knowledge.search`, `knowledge.answer`, `rule.evaluate`,
//! `case.create{handover}`, `case.create{complaint}`) for a pin set whose seed lacks them. A compiled row wins over
//! the static one. `outputs` is what B7's `OpRequest.outputs` is
//! filled from: everything else a connector returns is dropped.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, Mutex, PoisonError};

use agent_model::catalog::{OperationRow, op_key, parse_op_key};
use agent_model::kinds::catalog::{Idempotency, IdentityLevel, OperationAttrs, SideEffect};

/// Where a catalogue entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpSource {
    /// The job's pinned template compile.
    Template,
    /// The static built-in table ([`builtin_defs`]).
    Builtin,
}

/// One operation as the runtime sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct OpDef {
    /// `object.verb` or `object.verb{variant}`.
    pub key: String,
    pub object: String,
    pub verb: String,
    /// `*` for the base row.
    pub variant: String,
    /// Unset in the row → `P` (the strongest gating: an unknown effect is never treated as a read).
    pub side_effect: SideEffect,
    /// Always declared by the row (or the built-in); a row without one is not an operation (see [`OpDef::from_row`]).
    pub identity_level: IdentityLevel,
    /// Unset in the row → `Natural`.
    pub idempotency: Idempotency,
    /// The read that confirms an X write (`order.read`), if any.
    pub verify_read: Option<String>,
    pub confirm_back: bool,
    /// Runs inside agent (K49), never through a connector.
    pub builtin: bool,
    /// The canonical input fields.
    pub inputs: Vec<String>,
    /// The declared outputs (B7 `OpRequest.outputs`).
    pub outputs: Vec<String>,
    /// The outcome codes the operation can end in (B6a `error_states`).
    pub error_states: Vec<String>,
    pub source: OpSource,
}

impl OpDef {
    /// From the merged attribute columns of a resolved row.
    fn from_attrs(
        object: &str,
        verb: &str,
        variant: &str,
        identity_level: IdentityLevel,
        a: &OperationAttrs,
        error_states: Vec<String>,
        source: OpSource,
    ) -> OpDef {
        OpDef {
            key: op_key(object, verb, variant),
            object: object.to_owned(),
            verb: verb.to_owned(),
            variant: variant.to_owned(),
            side_effect: a.side_effect.unwrap_or(SideEffect::P),
            identity_level,
            idempotency: a.idempotency.unwrap_or(Idempotency::Natural),
            verify_read: a.verify_read.clone().filter(|v| !v.is_empty()),
            confirm_back: a.confirm_back.unwrap_or(false),
            builtin: a.builtin.unwrap_or(false),
            inputs: a.input_fields.clone().unwrap_or_default(),
            outputs: a.output_fields.clone().unwrap_or_default(),
            error_states,
            source,
        }
    }

    /// A resolved compile row; `None` when its attribute columns do not parse or it declares no `identity_level`
    /// (logged by the caller). There is no default level: an operation whose level is unknown is not callable,
    /// never callable at level 2.
    pub fn from_row(row: &OperationRow) -> Option<OpDef> {
        let attrs = row.typed_attrs().ok()?;
        let identity_level = effective_level(&row.key, attrs.identity_level)?;
        Some(Self::from_attrs(
            &row.object,
            &row.verb,
            &row.variant,
            identity_level,
            &attrs,
            row.error_states.clone(),
            OpSource::Template,
        ))
    }

    /// Anything but a read (W, X or P): the runner's idempotency, cap and confirm rules apply.
    pub fn writes(&self) -> bool {
        self.side_effect != SideEffect::R
    }
}

/// What the runtime asks of a catalogue. B5 (gates, toolsets) and B7 (`OpRequest.outputs`) use this trait.
pub trait OperationCatalog: Send + Sync {
    /// The operation `key` (exact: `rule.evaluate{RET-01}` is not `rule.evaluate`), if the job has it.
    fn get(&self, key: &str) -> Option<&OpDef>;
    /// Every operation key, sorted.
    fn keys(&self) -> Vec<&str>;
    /// The declared outputs of `key` (empty for an unknown operation: nothing passes).
    fn outputs(&self, key: &str) -> Vec<String> {
        self.get(key).map(|d| d.outputs.clone()).unwrap_or_default()
    }
}

/// The catalogue of one job's pin set (R9): the compiled operations, then the static built-ins it lacks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PinnedCatalog {
    ops: BTreeMap<String, OpDef>,
}

impl PinnedCatalog {
    /// From B6a's resolved operation rows (`Compiled.catalogue.operations`) plus [`builtin_defs`]. A row whose
    /// attributes do not parse is skipped (and logged by key only): the runtime never guesses an effect.
    pub fn from_rows(rows: &[OperationRow]) -> PinnedCatalog {
        let mut cat = PinnedCatalog::default();
        for row in rows {
            cat.put_row(row);
        }
        let mut ops = cat.ops;
        for d in builtin_defs() {
            ops.entry(d.key.clone()).or_insert(d);
        }
        PinnedCatalog { ops }
    }

    /// Add (or replace) the operation of `row`, at its [`effective_level`]; a row that is not an operation is skipped
    /// and logged by key only.
    pub fn put_row(&mut self, row: &OperationRow) {
        match OpDef::from_row(row) {
            Some(d) => {
                self.ops.insert(d.key.clone(), d);
            }
            None => {
                tracing::warn!(op = %row.key, "catalogue row skipped: unreadable attributes or no identity_level")
            }
        }
    }

    /// The static built-ins only (a job whose template does not compile).
    pub fn builtins_only() -> PinnedCatalog {
        Self::from_rows(&[])
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

impl OperationCatalog for PinnedCatalog {
    fn get(&self, key: &str) -> Option<&OpDef> {
        self.ops.get(key)
    }

    fn keys(&self) -> Vec<&str> {
        self.ops.keys().map(String::as_str).collect()
    }
}

/// The identity level `contracts/opdefs.json` declares for `op_key` (`object.verb` or `object.verb{variant}`): the
/// FLOOR no authored row may go below. A variant the contract does not list falls back to its base operation's level.
pub fn contract_floor(op_key: &str) -> Option<IdentityLevel> {
    static FLOORS: std::sync::LazyLock<BTreeMap<String, IdentityLevel>> =
        std::sync::LazyLock::new(|| {
            crate::connectors::tenant_templates::op_defs()
                .iter()
                .map(|d| {
                    (
                        crate::connectors::opkey::op_key(&d.op_ref()),
                        d.identity_level,
                    )
                })
                .collect()
        });
    FLOORS.get(op_key).copied().or_else(|| {
        op_key
            .split_once('{')
            .and_then(|(base, _)| FLOORS.get(base).copied())
    })
}

/// Authoring guard: an operation whose key is in the contract (or a variant of one) may not be authored BELOW the
/// contract's level (422 `identity_level_below_contract`, `minimum` = the contract's); an equal or higher level
/// tightens and is allowed; a key the contract lacks is the author's. Every operation-row writer calls this
/// (`platform::db::rows::write_revision`, and the edit path via `apply_operation_edit`).
pub fn require_contract_floor(op_key: &str, level: IdentityLevel) -> crate::error::ApiResult<()> {
    match contract_floor(op_key) {
        Some(floor) if floor.rank() > level.rank() => Err(crate::error::ApiError::Unprocessable(
            "identity_level_below_contract",
            format!(
                "{op_key} is in the operation contract at identity level {}; it cannot be authored at {}.",
                level_wire(floor),
                level_wire(level)
            ),
        )
        .with("minimum", level_wire(floor))),
        _ => Ok(()),
    }
}

/// The contract entry (`contracts/opdefs.json`) of `op_key` (`object.verb` or `object.verb{variant}`): the exact
/// key, else a variant's base operation. `None`: a custom operation, which is the author's.
pub fn contract_def(op_key: &str) -> Option<&'static crate::connectors::model::OpDef> {
    static DEFS: std::sync::LazyLock<BTreeMap<String, &'static crate::connectors::model::OpDef>> =
        std::sync::LazyLock::new(|| {
            crate::connectors::tenant_templates::op_defs()
                .iter()
                .map(|d| (crate::connectors::opkey::op_key(&d.op_ref()), d))
                .collect()
        });
    DEFS.get(op_key).copied().or_else(|| {
        op_key
            .split_once('{')
            .and_then(|(base, _)| DEFS.get(base).copied())
    })
}

/// Authoring guard for a contract operation's declared fields (the runtime keeps ONLY declared outputs, so an
/// operation authored with none answers `{}`): an empty list takes the contract's, a given list must be a subset of
/// the contract's (422 `field_not_in_contract`, naming the field; fewer is allowed, as a template's `field_map`
/// may map a subset of the outputs). A key the contract lacks is untouched. Every operation-row writer calls this
/// (`write_revision`, and the edit path via `apply_operation_edit`).
pub fn fill_contract_io(
    op_key: &str,
    input: &mut Vec<String>,
    output: &mut Vec<String>,
) -> crate::error::ApiResult<()> {
    let Some(def) = contract_def(op_key) else {
        return Ok(());
    };
    for (what, given, declared) in [
        ("input", &mut *input, &def.input_fields),
        ("output", &mut *output, &def.output_fields),
    ] {
        if given.is_empty() {
            *given = declared.clone();
        } else if let Some(bad) = given.iter().find(|f| !declared.contains(f)) {
            return Err(crate::error::ApiError::Unprocessable(
                "field_not_in_contract",
                format!(
                    "{bad} is not an {what} field of {op_key} in the operation contract (declared: {}).",
                    declared.join(", ")
                ),
            )
            .with("field", bad.clone())
            .with("declared", declared.clone()));
        }
    }
    Ok(())
}

/// `(op_key, row level)` pairs already reported below their contract floor (logged once per process: every
/// catalogue build re-reads the rows).
static BELOW_FLOOR_LOGGED: LazyLock<Mutex<BTreeSet<(String, &'static str)>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));

/// The level the runner applies to `op_key` given its row's `declared` level: the stricter of the row's and the
/// contract's floor ([`contract_floor`]), so no row (old,
/// authored, or written by SQL) runs a contract operation below its contract level. `None`: no level declared, not an
/// operation. Read by [`OpDef::from_row`] and `job_design::catalogue_operations`.
pub fn effective_level(op_key: &str, declared: Option<IdentityLevel>) -> Option<IdentityLevel> {
    let declared = declared?;
    Some(match contract_floor(op_key) {
        Some(floor) if floor.rank() > declared.rank() => {
            let fresh = BELOW_FLOOR_LOGGED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert((op_key.to_owned(), level_wire(declared)));
            if fresh {
                tracing::warn!(
                    op = op_key,
                    row = level_wire(declared),
                    contract = level_wire(floor),
                    "catalogue row below its contract identity level; the contract level applies"
                );
            }
            floor
        }
        _ => declared,
    })
}

/// The wire value of an identity level (`0`, `1`, `2`, `3`, `S`).
pub fn level_wire(l: IdentityLevel) -> &'static str {
    match l {
        IdentityLevel::Public => "0",
        IdentityLevel::Identified => "1",
        IdentityLevel::Verified => "2",
        IdentityLevel::StepUp => "3",
        IdentityLevel::Staff => "S",
    }
}

/// The keys of the static built-in table.
pub const BUILTIN_KEYS: [&str; 5] = [
    "knowledge.search",
    "knowledge.answer",
    "rule.evaluate",
    "case.create{handover}",
    "case.create{complaint}",
];

#[allow(clippy::too_many_arguments)]
fn builtin(
    key: &str,
    side_effect: SideEffect,
    identity_level: IdentityLevel,
    idempotency: Idempotency,
    builtin: bool,
    inputs: &[&str],
    outputs: &[&str],
    error_states: &[&str],
) -> OpDef {
    let (object, verb, variant) = parse_op_key(key).unwrap_or_else(|| {
        // BUILTIN_KEYS are literals; a malformed one is a programming error caught by the unit test below
        (key.to_owned(), String::new(), "*".to_owned())
    });
    let attrs = OperationAttrs {
        side_effect: Some(side_effect),
        identity_level: Some(identity_level),
        idempotency: Some(idempotency),
        builtin: Some(builtin),
        confirm_back: Some(side_effect != SideEffect::R),
        input_fields: Some(inputs.iter().map(|s| (*s).to_owned()).collect()),
        output_fields: Some(outputs.iter().map(|s| (*s).to_owned()).collect()),
        ..OperationAttrs::default()
    };
    let errors = error_states.iter().map(|s| (*s).to_owned()).collect();
    OpDef::from_attrs(
        &object,
        &verb,
        &variant,
        identity_level,
        &attrs,
        errors,
        OpSource::Builtin,
    )
}

/// The static built-ins (R9), with the base layer's columns (platform seed `base@v3`).
pub fn builtin_defs() -> Vec<OpDef> {
    use Idempotency::{Key, KeyDup, Natural};
    use IdentityLevel::{Identified, Public, Verified};
    use SideEffect::{R, W};
    vec![
        builtin(
            "knowledge.search",
            R,
            Public,
            Natural,
            false,
            &["query", "kind", "store"],
            &["passages", "citations"],
            &["upstream_unavailable", "timeout", "rate_limited"],
        ),
        builtin(
            "knowledge.answer",
            R,
            Public,
            Natural,
            false,
            &["question", "kind", "scope"],
            // D109 (KH T7): the answer, its citations (`AnswerSource` + layer/pack/provenance) and whether it is grounded
            &["answer", "citations", "grounded"],
            &["upstream_unavailable", "timeout", "rate_limited"],
        ),
        builtin(
            "rule.evaluate",
            R,
            Verified,
            Natural,
            true,
            &["rule_id", "subject_object"],
            &["outcome", "reason", "options"],
            &["identity_required"],
        ),
        builtin(
            "case.create{handover}",
            W,
            Identified,
            Key,
            false,
            &["subject", "transcript_ref"],
            &["case_id", "status", "due_at"],
            &[
                "identity_required",
                "invalid_input",
                "conflict",
                "outside_hours",
            ],
        ),
        builtin(
            "case.create{complaint}",
            W,
            Verified,
            KeyDup,
            false,
            &["case_type", "subject", "subject_object", "description"],
            &["case_id", "status", "due_at"],
            &[
                "identity_required",
                "invalid_input",
                "conflict",
                "duplicate",
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_parses_and_matches_its_keys() {
        let defs = builtin_defs();
        let keys: Vec<&str> = defs.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, BUILTIN_KEYS);
        for d in &defs {
            assert!(!d.verb.is_empty(), "{} parses as an operation key", d.key);
            assert!(!d.outputs.is_empty(), "{} declares outputs", d.key);
        }
        let c = PinnedCatalog::builtins_only();
        assert_eq!(c.len(), BUILTIN_KEYS.len());
        assert_eq!(
            c.outputs("knowledge.search"),
            ["passages".to_string(), "citations".to_string()]
        );
        assert_eq!(
            c.outputs("knowledge.answer"),
            [
                "answer".to_string(),
                "citations".to_string(),
                "grounded".to_string()
            ]
        );
        assert!(c.outputs("spaceship.launch").is_empty());
        assert!(c.get("case.create{handover}").is_some_and(OpDef::writes));
        assert!(!c.get("rule.evaluate").is_some_and(OpDef::writes));
    }
    /// Operations of the contract that must be public (level 0) and name no customer's record: pinned by name so a
    /// level dropped from `contracts/opdefs.json` cannot quietly move one into (or out of) the public set.
    const PUBLIC_CONTRACT_OPS: [&str; 4] = [
        "offering.list",
        "offering.read",
        "promotion.read",
        "service_event.list",
    ];

    /// The identity level `contracts/opdefs.json` declares for `key` (`object.verb` or `object.verb{variant}`).
    fn contract_level_of(key: &str) -> IdentityLevel {
        use crate::connectors::opkey::op_key;
        use crate::connectors::tenant_templates::op_defs;
        op_defs()
            .iter()
            .find(|d| op_key(&d.op_ref()) == key)
            .unwrap_or_else(|| panic!("{key} is not in the operation contract"))
            .identity_level
    }

    /// The security-relevant levels, pinned to the v16 values: a contact change, porting, MFA reset, party update
    /// and arrangement are step-up (3); revoking an access grant is staff-only; verifying and public knowledge /
    /// offering reads are level 0; the slot list is level 1; cancelling and refunding stay 2. Editing the contract
    /// without meaning to change one of these fails here.
    #[test]
    fn security_levels_are_pinned() {
        use IdentityLevel::{Identified, Public, Staff, StepUp, Verified};
        for (key, level) in [
            ("contact_point.update", StepUp),
            ("porting.create", StepUp),
            ("credential.reset_mfa", StepUp),
            ("party.update", StepUp),
            ("arrangement.create", StepUp),
            ("access_grant.revoke", Staff),
            ("knowledge.search", Public),
            ("knowledge.answer", Public),
            ("slot.list", Identified),
            ("offering.list", Public),
            ("offering.read", Public),
            ("order.cancel", Verified),
            ("payment.refund", Verified),
        ] {
            assert_eq!(contract_level_of(key), level, "{key}");
        }
        for key in PUBLIC_CONTRACT_OPS {
            assert_eq!(contract_level_of(key), Public, "{key} is public");
        }
    }

    /// Returns and warranty need dates from the owned order, alongside its ownership proof.
    #[test]
    fn order_read_contract_carries_policy_dates_and_owner() {
        let mut input = Vec::new();
        let mut output = Vec::new();
        fill_contract_io("order.read", &mut input, &mut output).unwrap();
        assert_eq!(input, ["order_id"]);
        for field in ["placed_at", "delivered_at", "customer_id"] {
            assert!(output.iter().any(|f| f == field), "missing {field}");
        }
        let mut undeclared = vec!["internal_notes".to_owned()];
        assert!(fill_contract_io("order.read", &mut input, &mut undeclared).is_err());
    }

    /// Every contract operation, variants included, declares a level the wire encodes.
    #[test]
    fn every_contract_op_has_a_level() {
        use crate::connectors::tenant_templates::{OPDEFS_JSON, op_defs};
        let raw: Vec<serde_json::Value> = serde_json::from_str(OPDEFS_JSON).unwrap();
        assert!(raw.len() > 40, "the contract was read: {}", raw.len());
        assert_eq!(op_defs().len(), raw.len(), "every entry parses as an OpDef");
        for r in &raw {
            let level = r["identity_level"].as_str().unwrap_or("");
            assert!(
                ["0", "1", "2", "3", "S"].contains(&level),
                "{}.{}{{{}}} has identity_level {:?}",
                r["object"],
                r["verb"],
                r["variant"],
                r["identity_level"]
            );
        }
    }

    /// A catalogue row that declares no `identity_level` is not an operation: no silent level 2.
    #[test]
    fn a_row_without_a_level_is_skipped() {
        let row: OperationRow = serde_json::from_value(serde_json::json!({
            "modality": "observe", "is_derive": false, "side_effect": "W", "idempotency": "key",
            "confirm_back": true, "verify_read": null, "builtin": false, "op_kind": "standard",
            "guard_rules": [], "input_fields": [], "output_fields": [],
            "key": "contact_point.update", "object": "contact_point", "verb": "update", "variant": "*",
            "discriminator": null, "overridden": [], "inherited": [], "description": "",
            "retail": null, "stages": [], "error_states": []
        }))
        .unwrap();
        assert_eq!(row.typed_attrs().unwrap().identity_level, None);
        assert!(OpDef::from_row(&row).is_none());
        assert!(
            PinnedCatalog::from_rows(&[row])
                .get("contact_point.update")
                .is_none()
        );
    }

    /// The ordering is 0 < 1 < 2 < 3 < S, and `stricter` takes the higher.
    #[test]
    fn identity_levels_are_ordered() {
        use IdentityLevel::{Identified, Public, Staff, StepUp, Verified};
        let all = [Public, Identified, Verified, StepUp, Staff];
        for w in all.windows(2) {
            assert!(w[0].rank() < w[1].rank());
            assert_eq!(w[0].stricter(w[1]), w[1]);
            assert_eq!(w[1].stricter(w[0]), w[1]);
        }
    }

    /// A row below its contract level runs at the contract's (max); a row above keeps its own; a custom op keeps its
    /// row's; a variant the contract lacks inherits its base's floor; no declared level stays `None`.
    #[test]
    fn effective_level_is_the_stricter_of_row_and_contract() {
        use IdentityLevel::{Public, Staff, StepUp, Verified};
        assert_eq!(
            effective_level("contact_point.update", Some(Verified)),
            Some(StepUp)
        );
        assert_eq!(
            effective_level("contact_point.update", Some(Public)),
            Some(StepUp)
        );
        assert_eq!(
            effective_level("contact_point.update", Some(StepUp)),
            Some(StepUp)
        );
        assert_eq!(
            effective_level("contact_point.update", Some(Staff)),
            Some(Staff)
        );
        assert_eq!(
            effective_level("contact_point.update{brand_new}", Some(Verified)),
            Some(StepUp)
        );
        assert_eq!(
            effective_level("zz_custom.thing", Some(Public)),
            Some(Public)
        );
        assert_eq!(effective_level("contact_point.update", None), None);
    }
}
