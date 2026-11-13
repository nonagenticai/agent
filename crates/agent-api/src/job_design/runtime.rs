//! What the runtime reads of the job design (plan "What the runtime (B4) reads", R8, B4 owner ruling 4): the fold
//! B4's `RuntimeDesign::read` calls ([`read`]) and [`JobDesign`], the field `RuntimeDesign.job_design` carries.
//!
//! - Bases come from `job_design::base::at` (never copied, R1): `Live` = the current release's pins / selection /
//!   bundle (a job never released: `jobs.pins` + the baseline), `Draft` = `jobs.pins` + the draft selection. The
//!   tenant's rows come from B2's read API for the same design (`values_of_kind`: the release snapshot for `Live`,
//!   the working rows for `Draft`) — the same functions the routes use, so Production and Test cannot drift.
//! - `spaces` = the effective context spaces (base + B2 `overlay` rows; `required` → the runtime's `slots`, the
//!   space's activity with the R15 seed fallback). `procedures` = the base versions with each name that has a
//!   PUBLISHED tenant version replaced by that version (it shadows the base `name@N`). A job with no job-design rows
//!   gets exactly its template content.
//! - [`JobDesign`]: the effective gate questions ([`GateSet`]: refinements, overlay hides, F-16/F-17 settings), the
//!   toolsets and the role settings (the one toolset source the assistant's offer and the runner's `ToolGate`
//!   consult, B4 owner ruling 5).
//!
//! The ontology (entities, relations) is not folded here yet: no runtime reader exists and B5 reads only the DRAFT
//! ontology rows today (`ontology::draft_rows`).
use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};
use sqlx::PgConnection;

use super::base::{self, Bases};
use super::context_space::{self, EffSpace};
use super::gates::engine::{self, ToolAsk};
use super::gates::library;
use super::gates::stats::Stats;
use super::procedures;
use super::toolset::{self, RoleSettings, Toolsets};
use super::types::{EffGateQuestion, GateFail, GatePoint};
use crate::design::{DesignOf, JobRef, Registry};
use crate::error::ApiResult;
use crate::release::compile::TemplateCompiler;
use crate::runtime::spaces::{ContextSpace, SpaceCatalog, TemplateProcedure};

/// The effective gate questions of one design, in design order (library first, then the tenant's).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GateSet {
    questions: Vec<EffGateQuestion>,
}

impl GateSet {
    pub fn new(questions: Vec<EffGateQuestion>) -> GateSet {
        GateSet { questions }
    }

    /// Every question (disabled ones included: the engine skips them, F-17).
    pub fn questions(&self) -> &[EffGateQuestion] {
        &self.questions
    }

    pub fn question(&self, key: &str) -> Option<&EffGateQuestion> {
        self.questions.iter().find(|q| q.key == key)
    }

    /// The questions asked at `p` (for P2: those covering `tool`, as the playground selects them).
    pub fn at(&self, p: GatePoint, tool: Option<&str>) -> Vec<&EffGateQuestion> {
        engine::select(&self.questions, p, Some(ToolAsk::Named(tool))).asked
    }

    /// Is an enabled question at `p` fail-closed? (A hook error then hands over; else it fails open.)
    pub fn has_closed(&self, p: GatePoint) -> bool {
        self.questions
            .iter()
            .any(|q| q.point == p && q.enabled && q.fail == GateFail::Closed)
    }
}

/// The job design as the runtime uses it (what has no B4 counterpart).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobDesign {
    /// P1 / P2 / R1 effective questions.
    pub gates: GateSet,
    /// (role, activity) → granted ops after N2 ∩ N3 − narrowings.
    pub toolsets: Toolsets,
    /// F-40: per-role enabled (job ∧ platform) and `max_tool_calls`.
    pub roles: RoleSettings,
}

/// What [`read`] gives `RuntimeDesign::read`.
#[derive(Clone, Debug)]
pub struct RuntimeParts {
    /// The bases the fold read (their `template_errors` say whether the template compiled).
    pub bases: Arc<Bases>,
    pub spaces: SpaceCatalog,
    pub procedures: Vec<TemplateProcedure>,
    pub design: JobDesign,
}

impl JobDesign {
    /// The job design of `of` (`Live` / `Draft`), read with the routes' functions so Production and Test cannot
    /// drift.
    pub async fn load(
        c: &mut PgConnection,
        reg: &Registry,
        compiler: &dyn TemplateCompiler,
        job: &JobRef,
        of: &DesignOf,
    ) -> ApiResult<JobDesign> {
        Ok(read(c, reg, compiler, job, of).await?.design)
    }
}

fn strings(m: &Map<String, Value>, f: &str) -> Vec<String> {
    m.get(f)
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn text(m: &Map<String, Value>, f: &str) -> String {
    m.get(f)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// An effective space as the runtime's [`ContextSpace`]: the effective `required` are its slots, its activity the
/// item's else the seed's (R15).
pub fn runtime_space(s: &EffSpace) -> ContextSpace {
    ContextSpace {
        key: s.key.clone(),
        label: text(&s.item, "label"),
        description: text(&s.item, "description"),
        area: text(&s.item, "area"),
        knowledge_kinds: strings(&s.item, "knowledge_kinds"),
        slots: s.required(),
        activity: s.activity(),
    }
}

/// The base procedures of `bases` with every name that has a PUBLISHED tenant version (`published`) replaced by
/// that version, sorted by (name, version). A tenant version keeps the connector slots of the newest base version
/// of its name.
pub fn effective_procedures(
    bases: &Bases,
    published: Vec<TemplateProcedure>,
) -> Vec<TemplateProcedure> {
    let base = TemplateProcedure::all_from_items(&bases.procedures);
    let tenant: BTreeSet<String> = published.iter().map(|p| p.name.clone()).collect();
    let mut out: Vec<TemplateProcedure> = published
        .into_iter()
        .map(|mut p| {
            if let Some(b) = base
                .iter()
                .filter(|b| b.name == p.name)
                .max_by_key(|b| b.version)
            {
                if p.uses_slots.is_empty() {
                    p.uses_slots = b.uses_slots.clone();
                }
                // the built-in steps that run under the name are the base's, recorded as the base version
                p.builtin_version = b.builtin_version;
            }
            p
        })
        .collect();
    out.extend(base.into_iter().filter(|p| !tenant.contains(&p.name)));
    out.sort_by_cached_key(|p| (p.name.clone(), p.version));
    out
}

/// The fold of design `of`: its bases (once), the effective spaces and procedures, the gate set, the toolsets and
/// the role settings. A template that does not compile gives empty bases (`bases.template_errors`), never an error;
/// database failures propagate.
pub async fn read(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<RuntimeParts> {
    let bases = base::at(&mut *c, reg, compiler, job, of).await?;
    let space_rows = context_space::rows(&mut *c, reg, job, of).await?;
    let spaces = SpaceCatalog::new(
        context_space::merge(&bases, &space_rows)
            .iter()
            .map(runtime_space)
            .collect(),
    );
    let published = procedures::published(&mut *c, reg, job, of).await?;
    let gate_rows = library::rows(&mut *c, reg, job, of).await?;
    let gates = GateSet::new(library::effective(&bases, &gate_rows, &Stats::default()));
    let narrowings = toolset::narrowings_of(&mut *c, reg, job, of).await?;
    let settings = toolset::role_settings_of(&mut *c, reg, job, of).await?;
    let own = super::targets::job_ops::of_design(&mut *c, reg, job, of).await?;
    let toolsets = Toolsets::build(&bases, narrowings, settings).with_own(&own);
    Ok(RuntimeParts {
        procedures: effective_procedures(&bases, published),
        spaces,
        design: JobDesign {
            gates,
            roles: toolsets.settings.clone(),
            toolsets,
        },
        bases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn tenant(name: &str, version: u32) -> TemplateProcedure {
        TemplateProcedure {
            name: name.into(),
            version,
            label: "mine".into(),
            uses_slots: Vec::new(),
            steps: 2,
            step_defs: Vec::new(),
            success_criteria: Vec::new(),
            tool_requirements: vec!["order.read".into()],
            context_space: None,
            builtin_version: None,
            tenant: true,
        }
    }

    #[test]
    fn a_published_tenant_version_shadows_the_base_name() {
        let mut b = Bases::default();
        b.procedures.insert(
            "return_request@2".into(),
            obj(json!({"label": "Return", "uses_slots": ["shop"], "steps": 6})),
        );
        b.procedures.insert(
            "return_request@1".into(),
            obj(json!({"label": "Return", "steps": 5})),
        );
        b.procedures.insert(
            "track_parcel@1".into(),
            obj(json!({"label": "Track", "steps": 3})),
        );
        // no tenant version: exactly the template's
        let none = effective_procedures(&b, Vec::new());
        assert_eq!(
            none,
            TemplateProcedure::all_from_items(&b.procedures),
            "no job-design rows: the template content"
        );
        let eff = effective_procedures(&b, vec![tenant("return_request", 3)]);
        let keys: Vec<(String, u32)> = eff.iter().map(|p| (p.name.clone(), p.version)).collect();
        assert_eq!(
            keys,
            [
                ("return_request".to_owned(), 3),
                ("track_parcel".to_owned(), 1)
            ]
        );
        assert_eq!(
            eff[0].uses_slots,
            ["shop"],
            "the newest base version's slots"
        );
        assert_eq!(eff[0].tool_requirements, ["order.read"]);
        assert_eq!(
            (eff[0].builtin_version, eff[1].builtin_version),
            (Some(2), Some(1)),
            "a tenant version records the base version it shadows; a base version its own"
        );
        let new_name = effective_procedures(&b, vec![tenant("refund_fix", 1)]);
        let p = new_name.iter().find(|p| p.name == "refund_fix").unwrap();
        assert_eq!(p.builtin_version, None, "no base declares the name");
    }

    #[test]
    fn runtime_spaces_take_required_as_slots_and_the_seed_activity() {
        let mut b = Bases {
            pins: vec![crate::platform::templates::TemplatePin {
                layer: "job/customer_care_agent".into(),
                version: 1,
            }],
            ..Bases::default()
        };
        b.context_spaces.insert(
            "retail:logistics:track_parcel".into(),
            obj(json!({"label": "Where is my parcel", "area": "logistics", "slots": ["tracking_ref"],
                       "knowledge_kinds": ["policy.delivery"], "activity": "delivery:where_is_my_order"})),
        );
        let rows = std::collections::BTreeMap::new();
        let s = runtime_space(&context_space::merge(&b, &rows)[0]);
        assert_eq!(s.key, "retail:logistics:track_parcel");
        assert_eq!(s.area, "logistics");
        assert_eq!(s.slots, ["tracking_ref"]);
        assert_eq!(s.knowledge_kinds, ["policy.delivery"]);
        assert_eq!(s.activity, "delivery:where_is_my_order");
        // the same space through B4's template reader: equal but for the activity
        let fields = b.context_spaces.values().next().unwrap();
        let mut b4 = ContextSpace::from_item("retail:logistics:track_parcel", fields);
        b4.activity = s.activity.clone();
        assert_eq!(b4, s);
    }
}
