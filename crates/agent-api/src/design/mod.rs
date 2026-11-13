//! The design spine (B2, D54/D62/D69): every tenant design location is a place `(kind, ref)` with a stable key.
//! Working rows are the draft; the live design is the snapshot of the job's current release. B3–B8 write design
//! rows only through this module.
pub mod coverage;
pub mod diff;
pub mod drafts;
pub mod fingerprint;
pub mod hooks;
pub mod ledger;
pub mod lifecycle;
pub mod read;
pub mod registry;
pub mod snapshot;
pub mod target;
pub mod track;

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::release::compile::TemplateCompiler;
pub use diff::{DesignDelta, diff};
pub use fingerprint::fingerprint;
pub use hooks::{default_selection, ensure_job, on_job_created};
pub use ledger::LedgerRow;
pub use lifecycle::{KeysLifecycle, lifecycle};
pub use read::{
    CaseDesign, DesignPlace, SnapshotCache, case_design, same, selection, selection_of_value,
    typed, value, values_of_kind,
};
pub use registry::Registry;
pub use snapshot::{DesignRow, Snapshot, ensure_baseline, take_snapshot};
pub use target::DesignTarget;
pub use track::{DraftWrite, Label, Tracked, draft_rev, track, track_tenant};

/// Every ledger kind (spec B2 §2.5): the 24 of the fixture `TARGETS` (`web/src/lib/fixtures/ledger.ts`) plus
/// `embedding_pin` (D83), `gate_setting`, `role_setting` (B5 T8a/T11a) and `tenant_calendar` (B4 T16a), which their
/// owning plans add to the fixture. 17 job snapshot kinds + 3 = 20 job kinds, 5 tenant kinds, 3 projections; then A2/A3 `tool_policy`,
/// `iam_attachment` and `job_operation` (0246, a tenant's own tools on its job).
pub const KINDS: [&str; 31] = [
    "languages",
    "setting",
    "overlay",
    "rule_values",
    "rule_passage",
    "selection",
    "gate",
    "gate_question",
    "entity",
    "relation",
    "space_required",
    "knowledge",
    "narrowing",
    "procedure",
    "route",
    "activity",
    "handover_policy",
    "embedding_pin",
    "gate_setting",
    "role_setting",
    "llm_use",
    "provider_slot",
    "mapping_override",
    "tenant_template",
    "tenant_calendar",
    "entity_hidden",
    "entity_synonyms",
    "entity_attributes",
    "tool_policy",
    "iam_attachment",
    "job_operation",
];

/// The kinds whose owning plan has not registered a target yet. Invariant (tests/design_registry.rs): the kinds
/// registered by [`registry()`] and `PENDING` are disjoint and together are exactly [`KINDS`]. Each owning plan moves
/// its kinds from here into its `register()`. B5 T5 registered its ten, B5 T8a/T11a `gate_setting` and
/// `role_setting`, and B8 T7 the last one, `activity`: nothing is pending any more.
pub const PENDING: &[&str] = &[];

/// The production registry (`AppState.design`, built in `state::build`). Each plan adds one line here,
/// `crate::<module>::targets::register(&mut r, ..);`, and moves its kinds out of [`PENDING`]. `compiler` is the
/// template compiler of `AppState.compiler`, for targets whose places come from the draft-compiled bundle (B2
/// `rule_values` / `rule_passage`).
pub fn registry(compiler: Arc<dyn TemplateCompiler>) -> Registry {
    let mut r = Registry::default();
    r.set_compiler(compiler.clone());
    crate::release::targets::register(&mut r, compiler);
    crate::connectors::places::register(&mut r);
    crate::providers::targets::register(&mut r);
    crate::knowledge::kinds::register(&mut r);
    crate::knowledge::targets::register(&mut r);
    crate::handover::policy::register(&mut r);
    crate::handover::calendar::register(&mut r);
    crate::job_design::targets::register(&mut r);
    crate::journey::refine::register(&mut r);
    r
}

/// A place's reference within its kind (fixture `DraftRef`). Values are `Value::String` or `Value::Null` only.
pub type DraftRef = serde_json::Map<String, Value>;

/// The value at a place. `None` = nothing at the place; `Some(v)` ⇔ fixture `{ v }`, and `v` may itself be JSON
/// null. Stored as jsonb `{"v": v}` or SQL NULL: convert with [`snap_to_json`] / [`snap_from_json`].
pub type Snap = Option<Value>;

/// A job, by id and key.
#[derive(Clone, Debug)]
pub struct JobRef {
    pub id: Uuid,
    pub key: String,
}

/// Where a target reads or writes: one tenant, and a job (`None` for a tenant-wide get/set).
#[derive(Clone, Copy, Debug)]
pub struct Place<'a> {
    pub tenant: Uuid,
    pub job: Option<&'a JobRef>,
}

/// A ledger kind's scope. `Tenant` = the D69 tenant-wide kinds (llm_use, provider_slot, mapping_override,
/// tenant_template, tenant_calendar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Job,
    Tenant,
}

/// Which design a read sees: the live snapshot, the working rows, or a release's stored snapshot (by id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesignOf {
    Live,
    Draft,
    Release(String),
}

/// Which environment a case runs in (D63, [`case_design`]): Production runs the live design, Test the draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Env {
    Production,
    Test,
}

/// A [`Snap`] as its stored jsonb: `Some(v)` → `{"v": v}`, `None` → SQL NULL. `Some(Value::Null)` stays distinct
/// from `None` (`{"v": null}` vs NULL).
pub fn snap_to_json(snap: &Snap) -> Option<Value> {
    snap.as_ref().map(|v| {
        let mut m = serde_json::Map::new();
        m.insert("v".to_string(), v.clone());
        Value::Object(m)
    })
}

/// The inverse of [`snap_to_json`]: SQL NULL → `None`, `{"v": v}` → `Some(v)`. Anything else is a corrupt row and
/// a 500.
pub fn snap_from_json(stored: Option<Value>) -> ApiResult<Snap> {
    match stored {
        None => Ok(None),
        Some(Value::Object(mut m)) if m.len() == 1 && m.contains_key("v") => Ok(m.remove("v")),
        Some(_) => Err(ApiError::Internal(anyhow::anyhow!(
            "stored snapshot value is not of the form {{\"v\": …}}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snap_roundtrip_keeps_inner_null() {
        let inner_null: Snap = Some(Value::Null);
        let stored = snap_to_json(&inner_null);
        assert_eq!(stored, Some(json!({"v": null})));
        assert_eq!(snap_to_json(&None), None);
        assert_ne!(stored, snap_to_json(&None));

        assert_eq!(snap_from_json(stored).unwrap(), Some(Value::Null));
        assert_eq!(snap_from_json(None).unwrap(), None);

        let v = json!({"a": [1, null], "b": "x"});
        assert_eq!(
            snap_from_json(snap_to_json(&Some(v.clone()))).unwrap(),
            Some(v)
        );

        // Not the stored shape: a bare value, JSON null, an extra key, a missing `v`.
        assert!(snap_from_json(Some(json!(5))).is_err());
        assert!(snap_from_json(Some(Value::Null)).is_err());
        assert!(snap_from_json(Some(json!({"v": 1, "w": 2}))).is_err());
        assert!(snap_from_json(Some(json!({"w": 1}))).is_err());
    }
}
