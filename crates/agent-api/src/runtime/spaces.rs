//! A job's context spaces and procedures as the runtime sees them (port of `cb:spaces.rs`, read from
//! [`super::design::RuntimeDesign`] instead of carebus-api over HTTP).
//!
//! Until B5 registers the `space_required` / `procedure` design kinds (both still in `design::PENDING`), both come
//! from the job's pinned template (B6a `compile_pinned(..).effective.job_content`), the same for prod and test. A
//! context space's `area` names the department stream (`bus:customer:{area}`); its key's last segment is the space
//! inside the area (`retail:order-support:returns` → `order-support.returns`).
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One context space (template `context_space` row: `label`, `area`, `knowledge_kinds`, `slots`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSpace {
    /// Ontology key, e.g. `retail:order-support:returns`.
    pub key: String,
    #[serde(default)]
    pub label: String,
    /// The qualifier's disambiguating sentence for this space (template `description`, KH-F1); empty = none, the
    /// label stands in ([`ContextSpace::qualifier_text`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Department, the `{area}` in stream keys.
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub knowledge_kinds: Vec<String>,
    /// The slots the qualifier fills for this space.
    #[serde(default)]
    pub slots: Vec<String>,
    /// The activity this space serves (B5 R15: the item's own `activity`; B5 T12 fills it from the job design's
    /// effective space). Empty = not known. The route hinter (B5 T10a, F-19) maps an activity to its space by it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
}

impl ContextSpace {
    /// From a compiled item (`key` → fields); unreadable fields fall back to empty.
    pub fn from_item(key: &str, fields: &Map<String, Value>) -> ContextSpace {
        let strings = |name: &str| -> Vec<String> {
            fields
                .get(name)
                .and_then(Value::as_array)
                .map(|xs| {
                    xs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let text = |name: &str| -> String {
            fields
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        ContextSpace {
            key: key.to_owned(),
            label: text("label"),
            description: text("description"),
            area: text("area"),
            knowledge_kinds: strings("knowledge_kinds"),
            slots: strings("slots"),
            activity: text("activity"),
        }
    }

    /// What the qualifier is shown for this space in `[task qualify]`: its description, else its label, else its key.
    pub fn qualifier_text(&self) -> &str {
        [self.description.trim(), self.label.trim()]
            .into_iter()
            .find(|t| !t.is_empty())
            .unwrap_or(&self.key)
    }

    /// The space inside the area: the key's last segment (`returns`).
    pub fn space(&self) -> &str {
        self.key.rsplit(':').next().unwrap_or_default()
    }

    /// Topic prefix, e.g. `order-support.returns` (topics are `{prefix}.{event}`).
    pub fn topic_prefix(&self) -> String {
        format!("{}.{}", self.area, self.space())
    }

    /// Topic for `event` in this space, e.g. `order-support.returns.request.qualified`.
    pub fn topic(&self, event: &str) -> String {
        format!("{}.{event}", self.topic_prefix())
    }
}

/// The context spaces of one job, sorted by key, one entry per key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpaceCatalog {
    spaces: Vec<ContextSpace>,
}

impl SpaceCatalog {
    pub fn new(mut spaces: Vec<ContextSpace>) -> Self {
        spaces.sort_by_cached_key(|s| s.key.clone());
        spaces.dedup_by(|a, b| a.key == b.key);
        Self { spaces }
    }

    /// From the compiled `job_content.context_spaces` (`key` → fields).
    pub fn from_items<'a>(
        items: impl IntoIterator<Item = (&'a String, &'a Map<String, Value>)>,
    ) -> Self {
        Self::new(
            items
                .into_iter()
                .map(|(k, f)| ContextSpace::from_item(k, f))
                .collect(),
        )
    }

    pub fn spaces(&self) -> &[ContextSpace] {
        &self.spaces
    }

    pub fn by_key(&self, key: &str) -> Option<&ContextSpace> {
        self.spaces.iter().find(|s| s.key == key)
    }

    /// The space that serves `activity`, the first by key; `None` when none does (or `activity` is empty).
    pub fn for_activity(&self, activity: &str) -> Option<&ContextSpace> {
        if activity.is_empty() {
            return None;
        }
        self.spaces.iter().find(|s| s.activity == activity)
    }

    /// Distinct areas, in order of first appearance by space key.
    pub fn areas(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in &self.spaces {
            if !s.area.is_empty() && !out.contains(&s.area) {
                out.push(s.area.clone());
            }
        }
        out
    }
}

/// One procedure of the job's template (`procedure` row `name@version`: `label`, `uses_slots`, `steps` = the step
/// count). Its steps (`ProcedureStep[]`) arrive with B5's `procedure` kind; until then the orchestrator plans from
/// the name only. A tenant version (B5 T7a) also carries its step bodies, success criteria and tool requirements;
/// a base version keeps them empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateProcedure {
    pub name: String,
    pub version: u32,
    #[serde(default)]
    pub label: String,
    /// The connector slots its steps use.
    #[serde(default)]
    pub uses_slots: Vec<String>,
    /// The number of steps the template declares.
    #[serde(default)]
    pub steps: u32,
    /// B5 T7a: the step bodies of a tenant version (pre/postconditions, `performed_by`, `max_per_case`).
    #[serde(default)]
    pub step_defs: Vec<crate::job_design::types::ProcedureStep>,
    /// B5 T7a (F-20): checked when the last phase finished.
    #[serde(default)]
    pub success_criteria: Vec<super::conditions::Condition>,
    /// B5 T7a: operations (`object.verb{variant}`) the procedure needs.
    #[serde(default)]
    pub tool_requirements: Vec<String>,
    /// B4 design procedures: the D61 context-space key a tenant version serves (`procedure_version.context_space`);
    /// `None` for a template procedure (B4 maps those by [`crate::runtime::orchestrator::procedure_for_space`]).
    #[serde(default)]
    pub context_space: Option<String>,
    /// B4: the template version whose BUILT-IN steps `StaticProcedures` runs under this name — its own version for
    /// a template procedure, the newest base version it shadows for a PUBLISHED tenant version
    /// (`job_design::runtime::effective_procedures`), `None` for a tenant name no base declares.
    #[serde(default)]
    pub builtin_version: Option<u32>,
    /// A version the tenant designed and published (`job_design::procedures::runtime_procedure`), not one the
    /// template declares. Only a tenant version is a DESIGNED procedure ([`crate::runtime::orchestrator::DesignProcedures`]):
    /// a template procedure carrying `context_space` and step bodies (the KC layer contract) is still run by
    /// `StaticProcedures`.
    #[serde(default)]
    pub tenant: bool,
}

/// The step bodies of a base procedure row's `steps`, all or nothing: one entry that is not a full `ProcedureStep`
/// (a `note`, a null `operation`) makes the whole list a count only. Dropping just that entry would run the rest
/// without it — a procedure whose eligibility gate fails to decode would reach its write step ungated.
pub fn base_steps(
    key: &str,
    fields: &Map<String, Value>,
) -> Vec<crate::job_design::types::ProcedureStep> {
    let Some(xs) = fields.get("steps").and_then(Value::as_array) else {
        return Vec::new();
    };
    let parsed: Result<Vec<_>, _> = xs
        .iter()
        .map(|x| serde_json::from_value(x.clone()))
        .collect();
    match parsed {
        Ok(defs) => defs,
        Err(e) => {
            if xs.iter().any(Value::is_object) {
                tracing::warn!(procedure = key, error = %e, "procedure_steps_not_runnable");
            }
            Vec::new()
        }
    }
}

/// A base procedure row's `success_criteria`: a list of step names, each read as "that step's outcome is `ok`"
/// (`steps.<name>.outcome eq "ok"`). An entry that is no step key (`^[a-z][a-z0-9_]{0,31}$`) is skipped — the older
/// seed rows hold prose here, and their steps are not runnable anyway. [`TemplateProcedure::from_item`] warns about a
/// skipped entry of a procedure that does run.
pub fn base_criteria(fields: &Map<String, Value>) -> Vec<super::conditions::Condition> {
    let Some(xs) = fields.get("success_criteria").and_then(Value::as_array) else {
        return Vec::new();
    };
    xs.iter()
        .filter_map(Value::as_str)
        .filter(|n| super::conditions::valid_id(n))
        .map(|n| super::conditions::Condition {
            id: n.to_owned(),
            fact: format!("steps.{n}.outcome"),
            op: super::conditions::CondOp::Eq,
            value: Some(Value::String("ok".to_owned())),
        })
        .collect()
}

/// A base procedure row's `tool_requirements` (`object.verb{variant}` strings; other entries are ignored).
pub fn base_requirements(fields: &Map<String, Value>) -> Vec<String> {
    fields
        .get("tool_requirements")
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl TemplateProcedure {
    /// From a compiled item keyed `name@version` (a key without `@` is version 1).
    pub fn from_item(key: &str, fields: &Map<String, Value>) -> TemplateProcedure {
        let (name, version) = match key.rsplit_once('@') {
            Some((n, v)) => (n, v.parse().unwrap_or(1)),
            None => (key, 1),
        };
        let step_defs = base_steps(key, fields);
        let criteria = base_criteria(fields);
        let listed = fields
            .get("success_criteria")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if !step_defs.is_empty() && criteria.len() < listed {
            tracing::warn!(procedure = key, "procedure_criterion_skipped");
        }
        TemplateProcedure {
            name: name.to_owned(),
            version,
            label: fields
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            uses_slots: fields
                .get("uses_slots")
                .and_then(Value::as_array)
                .map(|xs| {
                    xs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            // `steps` is a count or a step list (`Ty::CountOrList`); a list whose entries ALL have the full
            // `ProcedureStep` shape becomes the step bodies, otherwise it only counts (see `base_steps`)
            steps: match fields.get("steps") {
                Some(Value::Array(xs)) => u32::try_from(xs.len()).unwrap_or(u32::MAX),
                Some(v) => v.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
                None => 0,
            },
            step_defs,
            success_criteria: criteria,
            tool_requirements: base_requirements(fields),
            context_space: fields
                .get("context_space")
                .and_then(Value::as_str)
                .filter(|k| !k.is_empty())
                .map(str::to_owned),
            builtin_version: Some(version),
            tenant: false,
        }
    }

    /// From the compiled `job_content.procedures`, sorted by (name, version).
    pub fn all_from_items<'a>(
        items: impl IntoIterator<Item = (&'a String, &'a Map<String, Value>)>,
    ) -> Vec<TemplateProcedure> {
        let mut out: Vec<TemplateProcedure> = items
            .into_iter()
            .map(|(k, f)| TemplateProcedure::from_item(k, f))
            .collect();
        out.sort_by_cached_key(|p| (p.name.clone(), p.version));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn spaces_and_procedures_from_compiled_items() {
        let a = "retail:order-support:returns".to_string();
        let b = "retail:faq:shipping".to_string();
        let fa = obj(
            json!({"label": "Returns", "area": "order-support", "slots": ["order_id", "reason"]}),
        );
        let fb = obj(
            json!({"label": "Shipping", "area": "faq", "knowledge_kinds": ["policy.shipping"]}),
        );
        let c = SpaceCatalog::from_items([(&a, &fa), (&b, &fb)]);
        assert_eq!(c.spaces().len(), 2);
        assert_eq!(c.areas(), ["faq", "order-support"]);
        let r = c.by_key(&a).unwrap();
        assert_eq!(
            r.topic("request.qualified"),
            "order-support.returns.request.qualified"
        );
        assert_eq!(r.slots, ["order_id", "reason"]);

        let p = "return_request@2".to_string();
        let fp = obj(json!({"label": "Return request", "uses_slots": ["shop"], "steps": 6}));
        let procs = TemplateProcedure::all_from_items([(&p, &fp)]);
        assert_eq!(procs[0].name, "return_request");
        assert_eq!(procs[0].version, 2);
        assert_eq!(procs[0].steps, 6);
        assert_eq!(procs[0].context_space, None, "no space field reads as none");

        let q = "refund_request@1".to_string();
        let fq = obj(json!({"label": "Refund", "context_space": "retail:support:refund_tracking"}));
        let fe = obj(json!({"label": "Refund", "context_space": ""}));
        let procs = TemplateProcedure::all_from_items([(&q, &fq)]);
        assert_eq!(
            procs[0].context_space.as_deref(),
            Some("retail:support:refund_tracking"),
            "a seed row's top-level context_space is kept"
        );
        assert_eq!(
            TemplateProcedure::from_item(&q, &fe).context_space,
            None,
            "empty reads as none"
        );

        // a step list counts its entries; entries not in the full step shape are not step bodies
        let fl = obj(json!({"label": "Complaint", "steps": [
            {"step_number": 1, "phase": 0, "name": "listen", "operation": null},
            {"step_number": 2, "phase": 0, "name": "log", "operation": "case.create"}
        ]}));
        let p = TemplateProcedure::from_item("complaint_intake@1", &fl);
        assert_eq!(p.steps, 2);
        assert!(p.step_defs.is_empty());

        // all or nothing: a gate step that does not decode never leaves the later write runnable on its own
        let step = |n: u32, op: &str, se: &str, gate: bool| {
            json!({"step_number": n, "phase": n - 1, "name": format!("s{n}"), "operation": op, "variant": null,
                   "modality": "observe", "side_effect": se, "identity_level": "2", "confirm_back": false,
                   "gate": gate, "args": {}})
        };
        let mut gate = step(2, "rule.evaluate", "R", true);
        gate["note"] = json!("builtin");
        let mixed = obj(json!({"label": "Return", "steps": [
            step(1, "order.read", "R", false), gate, step(3, "case.create", "W", false)
        ]}));
        let p = TemplateProcedure::from_item("return_request@2", &mixed);
        assert_eq!(p.steps, 3);
        assert!(
            p.step_defs.is_empty(),
            "one undecodable step leaves no step bodies"
        );
        let full = obj(json!({"label": "Return", "steps": [
            step(1, "order.read", "R", false), step(2, "rule.evaluate", "R", true), step(3, "case.create", "W", false)
        ]}));
        let p = TemplateProcedure::from_item("return_request@2", &full);
        assert_eq!(p.step_defs.len(), 3);
        assert!(p.step_defs[1].gate);
    }

    /// A full base-row step in the exact shape the seed holds (`ProcedureStep`).
    fn full_step(n: u32, name: &str, op: &str, se: &str, gate: bool) -> Value {
        json!({"step_number": n, "phase": n - 1, "name": name, "operation": op, "variant": null,
               "modality": "observe", "side_effect": se, "identity_level": "2", "confirm_back": false,
               "gate": gate, "args": {"order_id": "$order_id"}, "preconditions": [], "postconditions": [],
               "performed_by": null, "max_per_case": null})
    }

    #[test]
    fn base_procedure_with_steps_reads_them() {
        let f = obj(
            json!({"label": "Cancel", "context_space": "retail:order-support:cancel_order",
            "steps": [full_step(1, "order", "order.read", "R", false), full_step(2, "cancel", "order.cancel", "X", false)]}),
        );
        let p = TemplateProcedure::from_item("cancel_order@1", &f);
        assert_eq!(p.steps, 2);
        assert_eq!(
            p.step_defs
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["order", "cancel"]
        );
        assert_eq!(p.step_defs[0].args["order_id"], "$order_id");
        assert_eq!(
            p.context_space.as_deref(),
            Some("retail:order-support:cancel_order")
        );
        assert_eq!(base_steps("cancel_order@1", &f).len(), 2);
    }

    #[test]
    fn base_procedure_reads_criteria_and_requirements() {
        let f = obj(json!({"label": "Return",
            "steps": [full_step(1, "order", "order.read", "R", false), full_step(2, "rma", "case.create", "W", false)],
            "success_criteria": ["order", "rma"],
            "tool_requirements": ["order.read", "case.create{return}"]}));
        let p = TemplateProcedure::from_item("return_request@3", &f);
        assert_eq!(p.success_criteria.len(), 2);
        let c = &p.success_criteria[1];
        assert_eq!(c.id, "rma");
        assert_eq!(c.fact, "steps.rma.outcome");
        assert_eq!(c.op, super::super::conditions::CondOp::Eq);
        assert_eq!(c.value, Some(json!("ok")));
        assert_eq!(p.tool_requirements, ["order.read", "case.create{return}"]);
        assert_eq!(base_requirements(&f), p.tool_requirements);
    }

    #[test]
    fn invalid_criterion_name_is_skipped() {
        // prose (the older seed rows), an upper-case id, an empty id, an over-long id and a non-string: all skipped
        let long = "a".repeat(33);
        let f = obj(json!({"label": "X", "success_criteria": [
            "no bank details were typed into the conversation", "Order", "", long, 7, "ok_1"]}));
        let got = base_criteria(&f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "ok_1");
        // a 32-character id is the longest allowed
        let max = "b".repeat(32);
        let f = obj(json!({"success_criteria": [max]}));
        assert_eq!(base_criteria(&f).len(), 1);
        assert!(base_criteria(&obj(json!({"label": "X"}))).is_empty());
    }

    #[test]
    fn base_procedure_count_only_keeps_builtins() {
        // a step count, or prose steps: no step bodies, so the built-in steps run; prose criteria add no check
        let counted = obj(json!({"label": "Order status", "steps": 4,
            "success_criteria": ["the customer was told where the parcel is"]}));
        let p = TemplateProcedure::from_item("order_status@1", &counted);
        assert_eq!(p.steps, 4);
        assert!(p.step_defs.is_empty());
        assert!(p.success_criteria.is_empty());
        let prose = obj(json!({"label": "Order status", "steps": [
            {"step_number": 1, "phase": 0, "name": "ask", "operation": null, "note": "builtin"}]}));
        let p = TemplateProcedure::from_item("order_status@1", &prose);
        assert_eq!(p.steps, 1);
        assert!(p.step_defs.is_empty());
    }

    #[test]
    fn a_base_procedure_with_any_undecodable_step_runs_none_of_them() {
        let mut bad = full_step(2, "gate", "rule.evaluate", "R", true);
        bad["operation"] = Value::Null;
        let f = obj(json!({"label": "Return", "steps": [
            full_step(1, "order", "order.read", "R", false), bad, full_step(3, "rma", "case.create", "W", false)]}));
        assert!(base_steps("return_request@2", &f).is_empty());
        let mut extra = full_step(2, "gate", "rule.evaluate", "R", true);
        extra["note"] = json!("builtin");
        let f = obj(json!({"label": "Return", "steps": [
            full_step(1, "order", "order.read", "R", false), extra]}));
        assert!(base_steps("return_request@2", &f).is_empty());
    }
}
