//! B5's design targets (plan B5 "Snapshot targets registered with B2 (D62)", T5): the Rust form of the fixture
//! `TARGETS` entries `entity`, `entity_hidden`, `entity_synonyms`, `entity_attributes`, `relation`,
//! `space_required`, `procedure`, `gate`, `gate_question` and `narrowing` (`web/src/lib/fixtures/ledger.ts`). Keys
//! and ref shapes are the fixture's (checked against `tests/fixtures/design-kinds.json`).
//!
//! | kind | key | working rows | places |
//! |---|---|---|---|
//! | `entity` | `entity:<name>` | `ontology_entity` (tenant) / `ontology_entity_refinement` (base) | every name of both |
//! | `entity_hidden` / `_synonyms` / `_attributes` | `hide:` / `synonyms:` / `attributes:<name>` | as `entity` | projection of `entity` |
//! | `relation` | `relation:<s>:<p>:<o>` | `ontology_relation` | every tenant relation |
//! | `space_required` | `space:<key>` | B2 `job_overlay_row` (object `context_space`) | projection of `overlay` |
//! | `procedure` | `procedure:<name>` | `procedure_version` | every name with a tenant version |
//! | `gate` | `gate:<gate>:<option\|threshold>` | `job_gate_option_refinement` / `job_gate_rule_refinement` | every row |
//! | `gate_question` | `gate-question:<gate>` | `job_gate_question` | every tenant question |
//! | `narrowing` | `narrow:<role>:<activity>:<op>` | `toolset_narrowing` | every narrowing |
//! | `gate_setting` | `gate-setting:<gate>` | `job_gate_question_setting` (T8a, 0146) | every setting row |
//!
//! | `role_setting` | `role-setting:<role>` | `job_agent_role_setting` (T11a, 0148) | every setting row |
//! | `job_operation` | `job-op:<op>` | `job_operation` (0246) | every op the job added |
//!
//! As B2's targets (`release::targets`): `set` is the restore path (discard, follow-live after apply/rollback,
//! restore after a rollback) and writes exactly the value `get` returns; handlers write their rows in the `mutate`
//! closure they pass to `design::track`. A row `set` creates is stamped [`SET_BY`]. Values never hold who/when
//! columns (the ledger and audit trail name the person) nor data a B2 `overlay` place stores (plan "one place, one
//! kind").
pub mod declared;
pub mod gate_settings;
pub mod gates;
pub mod job_ops;
pub mod ontology;
pub mod procedures;
pub mod toolsets;

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::design::{DraftRef, Place, Registry, Snap};
use crate::error::{ApiError, ApiResult};
pub use crate::release::targets::SET_BY;

pub use ontology::entity_write_kind;

/// Register B5's targets (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(ontology::Entity));
    r.register(Arc::new(ontology::EntityHidden));
    r.register(Arc::new(ontology::EntitySynonyms));
    r.register(Arc::new(ontology::EntityAttributes));
    r.register(Arc::new(ontology::Relation));
    r.register(Arc::new(ontology::SpaceRequired));
    r.register(Arc::new(procedures::Procedure));
    r.register(Arc::new(gates::Gate));
    r.register(Arc::new(gates::GateQuestion));
    r.register(Arc::new(gate_settings::GateSettingTarget));
    r.register(Arc::new(toolsets::Narrowing));
    r.register(Arc::new(toolsets::RoleSettingTarget));
    r.register(Arc::new(declared::ToolPolicyTarget));
    r.register(Arc::new(declared::IamAttachmentTarget));
    r.register(Arc::new(job_ops::JobOperationTarget));
}

// ---- helpers --------------------------------------------------------------------------------------------------------

pub(crate) fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// The job of a place: every B5 kind is job-scoped.
pub(crate) fn job_of(at: Place<'_>) -> ApiResult<Uuid> {
    at.job
        .map(|j| j.id)
        .ok_or_else(|| internal("a B5 design target is read and written at a job place".into()))
}

/// A ref field as a string (`""` when absent or not a string).
pub(crate) fn field<'r>(r: &'r DraftRef, f: &str) -> &'r str {
    r.get(f).and_then(Value::as_str).unwrap_or("")
}

/// A ref of string fields.
pub(crate) fn place_ref(pairs: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), Value::String((*v).to_owned()));
    }
    m
}

/// A non-null value, else `None` (B5 kinds have no "null at the place" distinct from "nothing").
pub(crate) fn present(v: &Snap) -> Option<&Value> {
    v.as_ref().filter(|x| !x.is_null())
}

/// JavaScript `String(v)`.
pub(crate) fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Fixture `list`: a non-empty array joined with `, `, else `none`.
pub(crate) fn list(v: Option<&Value>) -> String {
    match v.and_then(Value::as_array) {
        Some(xs) if !xs.is_empty() => xs
            .iter()
            .map(|x| {
                if x.is_null() {
                    String::new()
                } else {
                    js_string(x)
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        _ => "none".to_string(),
    }
}

/// `v` decoded into `T`, else a 500 naming the kind (a corrupt stored value).
pub(crate) fn decode<T: serde::de::DeserializeOwned>(kind: &str, v: &Value) -> ApiResult<T> {
    serde_json::from_value(v.clone())
        .map_err(|e| internal(format!("{kind} value has another shape: {e}")))
}
