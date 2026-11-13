//! B8 phase 2 T5/T6 (R9, R11): what a job's journey reads from the other lanes, composed over [`Bases`] so no lane
//! changes for it.
//!
//! - **Routes (R11).** [`route_data`] is the J6 [`RouteData`] of the job's draft (working routes) or Production
//!   (current release), through `connectors::routes_store`; [`route_info`] resolves one operation on one env with
//!   B7's own `route_readiness`, so the journey and J6 cannot disagree.
//! - **Operations.** [`op_meta`] = the catalogue row of an operation (its variant's own, else the `*` row).
//! - **Gates (R9).** [`use_case_gates`]: the gates on the way into an activity, answered for it. The activity-routing
//!   gates (GS stage, GA activity, GO object, GM modality, GI identifier) answer with the activity's own values; any
//!   other library gate belongs to the activity when one of its options names the activity (its key or its slug).
//! - **Procedures (R9).** [`procedure_for`]: the procedure the activity's context space names
//!   ([`Bases::context_spaces`] `procedure`), the PUBLISHED tenant version shadowing the template's newest version.
//!   The space is [`base::context_space_for`], else the D61 key made from the activity's area ([`space_key`]).
//! - **Use-case card.** [`pinned_card`] folds the `use_case_card` rows of the pinned tags (later scope wins, a
//!   tombstone clears); [`Card::overlay`] puts the tenant's `card` over it.
use agent_model::compile::Selection;
use agent_model::kinds::catalog::{Modality, SideEffect};
use agent_model::kinds::journey::{CardScenario, UseCaseCardFields};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::model::{JourneyStage, OperationExpectation};
use crate::cases::signals;
use crate::connectors::model::OpRef;
use crate::connectors::resolve::{self, RouteData, RouteTarget};
use crate::connectors::routes_store::{self, ExpectedCatalogue};
use crate::design::{DesignOf, JobRef, Registry};
use crate::error::ApiResult;
use crate::job_design::base::{self, Bases};
use crate::job_design::gates::stats::Stats;
use crate::job_design::procedures;
use crate::job_design::types::CatalogueOperation;
use crate::job_design::{catalogue_operations, op_label};
use crate::platform::db::tags as db_tags;
use crate::platform::templates::TemplatePin;

/// The two environments every operation is routed in, in the order the pages show them.
pub const ENVS: [&str; 2] = ["prod", "test"];

// ---- routes (R11) ---------------------------------------------------------------------------------------------------

/// `OperationRouteInfo` (web `types/journey.ts`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OperationRouteInfo {
    pub env: String,
    /// `instance | builtin | fallback | deep_link | not_offered | unrouted`.
    pub kind: &'static str,
    /// Instance id, `builtin`, `agent_cases`, the deep-link URL; null when unrouted or not offered.
    pub target: Option<String>,
    pub contract_tested: bool,
}

/// The J6 route data of `job` `of` the draft (working rows) or Production (its current release; never released →
/// none). A pinned release (`DesignOf::Release`) reads as Production: B7 has no per-release route read.
pub async fn route_data(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<RouteData> {
    let row = routes_store::job_row(&mut *c, job.id).await?;
    let routes = match of {
        DesignOf::Draft => routes_store::working_routes(&mut *c, job.id).await?,
        DesignOf::Live => {
            routes_store::routes_of(&mut *c, reg, job, routes_store::DesignOf::Live).await?
        }
        DesignOf::Release(_) => {
            let rows = crate::design::values_of_kind(&mut *c, reg, job, of, "route").await?;
            crate::connectors::places::routes_of(&rows)?
        }
    };
    let view = routes_store::view_of(&mut *c, row, ExpectedCatalogue::default(), routes).await?;
    Ok(view.data)
}

/// How `op` (`object.verb`, with its `variant`) is routed on `env`.
pub fn route_info(d: &RouteData, op: &str, variant: Option<&str>, env: &str) -> OperationRouteInfo {
    let (object, verb) = op.split_once('.').unwrap_or((op, ""));
    let opref = OpRef {
        object: object.to_owned(),
        verb: verb.to_owned(),
        variant: variant.filter(|v| !v.is_empty()).unwrap_or("*").to_owned(),
    };
    let (route, _inherited, readiness) = resolve::route_readiness(d, &opref, env);
    let (kind, target, tested) = match route.map(|r| &r.target) {
        None => ("unrouted", None, false),
        Some(RouteTarget::NotOffered) => ("not_offered", None, false),
        Some(RouteTarget::Instance { instance_id, .. }) => {
            ("instance", Some(instance_id.clone()), readiness == "ok")
        }
        Some(RouteTarget::Builtin) => ("builtin", Some("builtin".to_owned()), true),
        Some(RouteTarget::AgentCases) => ("fallback", Some("agent_cases".to_owned()), true),
        Some(RouteTarget::DeepLink { url }) => ("deep_link", Some(url.clone()), true),
    };
    OperationRouteInfo {
        env: env.to_owned(),
        kind,
        target,
        contract_tested: tested,
    }
}

/// [`route_info`] on both environments.
pub fn routes_for(d: &RouteData, op: &str, variant: Option<&str>) -> Vec<OperationRouteInfo> {
    ENVS.iter()
        .map(|env| route_info(d, op, variant, env))
        .collect()
}

/// The required operations of `ops` with no route, as `(op label, env)`.
pub fn unrouted_required(
    d: &RouteData,
    ops: &[OperationExpectation],
) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    for o in ops.iter().filter(|o| o.required) {
        for env in ENVS {
            if route_info(d, &o.op, o.variant.as_deref(), env).kind == "unrouted" {
                out.push((op_label(&o.op, o.variant.as_deref()), env));
            }
        }
    }
    out
}

// ---- operations -------------------------------------------------------------------------------------------------------

/// The catalogue attributes of one operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpMeta {
    pub modality: &'static str,
    pub side_effect: &'static str,
    pub identity_level: String,
}

pub fn modality_str(m: Modality) -> &'static str {
    match m {
        Modality::Observe => "observe",
        Modality::Do => "do",
        Modality::Request => "request",
    }
}

pub fn side_effect_str(s: SideEffect) -> &'static str {
    match s {
        SideEffect::R => "R",
        SideEffect::W => "W",
        SideEffect::X => "X",
        SideEffect::P => "P",
    }
}

/// The job's operation catalogue (`Bases::operations`) as the pages list it.
pub fn catalogue(bases: &Bases) -> Vec<CatalogueOperation> {
    catalogue_operations(&bases.operations)
}

/// The catalogue row of `op` with `variant` (else its `*` row). An operation the catalogue does not have reads as
/// the fixture's `observe` / `R` / `—`. The `by_tracking_ref` variant is identity level `1` (T6 table).
pub fn op_meta(cat: &[CatalogueOperation], op: &str, variant: Option<&str>) -> OpMeta {
    let variant = variant.filter(|v| !v.is_empty());
    let hit = variant
        .and_then(|v| {
            cat.iter()
                .find(|c| c.op == op && c.variant.as_deref() == Some(v))
        })
        .or_else(|| cat.iter().find(|c| c.op == op && c.variant.is_none()));
    let (modality, side_effect, level) = match hit {
        Some(c) => (
            modality_str(c.modality),
            side_effect_str(c.side_effect),
            c.identity_level.clone(),
        ),
        None => ("observe", "R", "\u{2014}".to_owned()),
    };
    OpMeta {
        modality,
        side_effect,
        identity_level: if variant == Some("by_tracking_ref") {
            "1".to_owned()
        } else {
            level
        },
    }
}

// ---- context space key (D61) -------------------------------------------------------------------------------------------

fn is_snake(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && !s.ends_with('_')
        && !s.contains("__")
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn is_dashed(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && !s.ends_with('-')
        && !s.contains("--")
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// D61 `<industry>:<area>:<slug>` (web `contextSpaceKey`); `None` when a part breaks the scheme.
pub fn space_key(industry: &str, area: &str, slug: &str) -> Option<String> {
    (is_snake(industry) && is_dashed(area) && is_snake(slug))
        .then(|| format!("{industry}:{area}:{slug}"))
}

/// The context space of an activity: the base space that serves it, else the D61 key from its `area` (`None`
/// without an area, or when the parts break the scheme).
pub fn context_space(
    bases: &Bases,
    industry: &str,
    activity: &str,
    area: Option<&str>,
) -> Option<String> {
    base::context_space_for(bases, activity).or_else(|| {
        let slug = activity.split_once(':').map_or(activity, |(_, s)| s);
        space_key(industry, area.filter(|a| !a.is_empty())?, slug)
    })
}

// ---- gates (R9) -------------------------------------------------------------------------------------------------------

/// `UseCaseGate` (web `types/journey.ts`): one question on the way into the activity and the answer that leads here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UseCaseGate {
    pub key: String,
    pub point: String,
    pub question: String,
    pub answer: String,
    pub threshold: String,
    /// How often the gate gave this answer in the last 7 days; null without a log for the gate.
    pub verdicts_7d: Option<u64>,
}

/// What [`use_case_gates`] needs to know of the activity.
pub struct GateSubject<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub stage: &'a JourneyStage,
    /// The key of the activity's subject object, if it has one.
    pub object: Option<&'a str>,
    pub modality: &'a str,
    /// The slots the activity's objects require (GI pre-fills them).
    pub slots: &'a [String],
}

fn text_of(f: &Map<String, Value>, name: &str) -> String {
    f.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The gates on the way into the activity (module docs), in library order.
pub fn use_case_gates(bases: &Bases, stats: &Stats, s: &GateSubject<'_>) -> Vec<UseCaseGate> {
    let slug = s.key.split_once(':').map_or(s.key, |(_, x)| x);
    let mut out = Vec::new();
    for g in &bases.gates {
        let answered: Option<(String, Option<String>)> = match g.key.as_str() {
            "GS" => Some((s.stage.label.clone(), Some(s.stage.key.as_str().to_owned()))),
            "GA" => Some((s.label.to_owned(), Some(s.key.to_owned()))),
            "GO" => Some(match s.object {
                Some(o) => (o.to_owned(), Some(o.to_owned())),
                None => ("none / general".to_owned(), Some("none".to_owned())),
            }),
            "GM" => Some((s.modality.to_owned(), Some(s.modality.to_owned()))),
            "GI" => (!s.slots.is_empty()).then(|| {
                (
                    format!("yes \u{2192} pre-fills {}", s.slots.join(", ")),
                    None,
                )
            }),
            _ => base::gate_options_of(g).find_map(|o| {
                let key = o.get("key").and_then(Value::as_str)?;
                (key == s.key || key == slug).then(|| {
                    let label = o
                        .get("label")
                        .and_then(Value::as_str)
                        .filter(|l| !l.is_empty())
                        .unwrap_or(key);
                    (label.to_owned(), Some(key.to_owned()))
                })
            }),
        };
        let Some((answer, option)) = answered else {
            continue;
        };
        let verdicts_7d = option.and_then(|opt| {
            stats
                .by_gate
                .contains_key(&g.key)
                .then(|| stats.verdicts(&g.key, &opt))
        });
        let threshold = text_of(&g.fields, "threshold");
        out.push(UseCaseGate {
            key: g.key.clone(),
            point: text_of(&g.fields, "point"),
            question: text_of(&g.fields, "question"),
            answer,
            threshold: if threshold.is_empty() {
                "\u{2014}".to_owned()
            } else {
                threshold
            },
            verdicts_7d,
        });
    }
    out
}

// ---- procedures (R9) --------------------------------------------------------------------------------------------------

/// `UseCaseStep` (web): one procedure step with the system its operation routes to in prod.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UseCaseStep {
    pub n: u32,
    pub phase: u32,
    pub name: String,
    /// `delivery.read{by_tracking_ref}`
    pub op: String,
    pub side_effect: String,
    pub identity_level: String,
    pub confirm_back: bool,
    pub gate: bool,
    pub prod: OperationRouteInfo,
}

/// The procedure of the activity's use case (web `ActivityUseCase.procedure`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UseCaseProcedure {
    pub name: String,
    pub version: u32,
    /// `base` (the template's) or `tenant` (its published version).
    pub origin: &'static str,
    pub context_space: String,
    pub description: String,
    pub runs_7d: i64,
    /// Contained / cases over the last 7 days; null without a case.
    pub success_rate: Option<f64>,
    pub steps: Vec<UseCaseStep>,
}

/// The procedure name a base context space names (`context_space.procedure`).
pub fn space_procedure(bases: &Bases, space: &str) -> Option<String> {
    bases
        .context_spaces
        .get(space)?
        .get("procedure")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

fn json_u32(o: &Map<String, Value>, name: &str) -> Option<u32> {
    o.get(name)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// A template step as the base procedure item holds it (lenient: a step without a side effect or level takes the
/// catalogue's).
fn base_steps(
    item: &Map<String, Value>,
    routes: &RouteData,
    cat: &[CatalogueOperation],
) -> Vec<UseCaseStep> {
    let Some(steps) = item.get("steps").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, s) in steps.iter().filter_map(Value::as_object).enumerate() {
        let Some(op) = s.get("operation").and_then(Value::as_str) else {
            continue;
        };
        let variant = s
            .get("variant")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty());
        let meta = op_meta(cat, op, variant);
        let side_effect = s
            .get("side_effect")
            .and_then(Value::as_str)
            .map_or_else(|| meta.side_effect.to_owned(), str::to_owned);
        let identity_level = match s.get("identity_level") {
            Some(Value::String(l)) => l.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => meta.identity_level.clone(),
        };
        out.push(UseCaseStep {
            n: json_u32(s, "step_number")
                .unwrap_or_else(|| u32::try_from(i + 1).unwrap_or(u32::MAX)),
            phase: json_u32(s, "phase").unwrap_or(0),
            name: s
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(op)
                .to_owned(),
            op: op_label(op, variant),
            side_effect,
            identity_level,
            confirm_back: s
                .get("confirm_back")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            gate: s.get("gate").and_then(Value::as_bool).unwrap_or(false),
            prod: route_info(routes, op, variant, "prod"),
        });
    }
    out
}

/// The procedure of `space` for the activity: the PUBLISHED tenant version of the procedure the space names, else
/// the template's newest version of it; `None` when the space names none (or the design has no such procedure).
/// `runs` are the activity's last-7-days Production `(cases, contained)`.
#[allow(clippy::too_many_arguments)]
pub async fn procedure_for(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
    bases: &Bases,
    space: &str,
    routes: &RouteData,
    runs: (i64, i64),
) -> ApiResult<Option<UseCaseProcedure>> {
    let Some(name) = space_procedure(bases, space) else {
        return Ok(None);
    };
    let cat = catalogue(bases);
    let (cases, contained) = runs;
    let success_rate = (cases > 0).then(|| contained as f64 / cases as f64);
    let tenant = procedures::published(&mut *c, reg, job, of)
        .await?
        .into_iter()
        .find(|p| p.name == name);
    if let Some(p) = tenant {
        let steps = p
            .step_defs
            .iter()
            .map(|s| UseCaseStep {
                n: s.step_number,
                phase: s.phase,
                name: s.name.clone(),
                op: op_label(&s.operation, s.variant.as_deref()),
                side_effect: side_effect_str(s.side_effect).to_owned(),
                identity_level: s.identity_level.clone(),
                confirm_back: s.confirm_back,
                gate: s.gate,
                prod: route_info(routes, &s.operation, s.variant.as_deref(), "prod"),
            })
            .collect();
        return Ok(Some(UseCaseProcedure {
            context_space: p.context_space.clone().unwrap_or_else(|| space.to_owned()),
            description: p.label.clone(),
            version: p.version,
            name: p.name,
            origin: "tenant",
            runs_7d: cases,
            success_rate,
            steps,
        }));
    }
    let Some(version) = bases.procedure_versions(&name).last().copied() else {
        return Ok(None);
    };
    let empty = Map::new();
    let item = bases
        .procedures
        .get(&format!("{name}@{version}"))
        .unwrap_or(&empty);
    let description = ["description", "label"]
        .iter()
        .find_map(|f| {
            item.get(*f)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(&name)
        .to_owned();
    Ok(Some(UseCaseProcedure {
        steps: base_steps(item, routes, &cat),
        name,
        version,
        origin: "base",
        context_space: space.to_owned(),
        description,
        runs_7d: cases,
        success_rate,
    }))
}

/// The activity's last-7-days Production `(cases, contained)` (B4 signals).
pub async fn runs_7d(c: &mut PgConnection, job: &JobRef, activity: &str) -> ApiResult<(i64, i64)> {
    let live = signals::use_case_live(&mut *c, job, activity, 7).await?;
    Ok(live.live.map_or((0, 0), |l| (l.cases, l.contained)))
}

// ---- the use-case card ------------------------------------------------------------------------------------------------

/// The text fields of a use-case card, as the page shows them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub summary: Option<String>,
    pub examples: Option<Vec<String>>,
    pub metric: Option<Value>,
    pub sample_dialogue: Option<Vec<Value>>,
    pub success_example: Option<Value>,
    pub error_example: Option<Value>,
    /// The platform's test scenarios (B8-4): never refined by a tenant, so [`Card::overlay`] leaves them.
    pub scenarios: Option<Vec<CardScenario>>,
}

impl Card {
    fn of(f: UseCaseCardFields) -> Self {
        Self {
            summary: f.summary,
            examples: f.examples,
            metric: f.metric,
            sample_dialogue: f.sample_dialogue,
            success_example: f.success_example,
            error_example: f.error_example,
            scenarios: f.scenarios,
        }
    }

    /// Put the tenant's `card` over this one: a field it carries (not null) replaces the template's.
    pub fn overlay(&mut self, card: &Map<String, Value>) {
        let live = |n: &str| card.get(n).filter(|v| !v.is_null());
        if let Some(s) = live("summary").and_then(Value::as_str) {
            self.summary = Some(s.to_owned());
        }
        if let Some(xs) = live("examples").and_then(Value::as_array) {
            self.examples = Some(
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            );
        }
        if let Some(v) = live("metric") {
            self.metric = Some(v.clone());
        }
        if let Some(xs) = live("sample_dialogue").and_then(Value::as_array) {
            self.sample_dialogue = Some(xs.clone());
        }
        if let Some(v) = live("success_example") {
            self.success_example = Some(v.clone());
        }
        if let Some(v) = live("error_example") {
            self.error_example = Some(v.clone());
        }
    }
}

fn scope_rank(scope: &str) -> u8 {
    match scope {
        "base" => 0,
        "profile" => 1,
        "industry" => 2,
        "base_dept" => 3,
        "industry_dept" => 4,
        _ => 5,
    }
}

/// The cards of every activity over the pinned stack: the `use_case_card` rows of the members of the pinned tags,
/// folded base → industry → department archetype → department → job (a later layer's row sets the whole card; a
/// tombstone clears it). An activity no layer gives a card is absent.
pub async fn pinned_cards(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
) -> ApiResult<std::collections::BTreeMap<String, Card>> {
    let mut rows = Vec::new();
    for p in pins {
        rows.extend(
            db_tags::tag_members(&mut *c, &p.layer, p.version)
                .await?
                .into_iter()
                .filter(|r| r.kind == "use_case_card"),
        );
    }
    // a department layer of a department the job did not select is not on its stack
    rows.retain(|r| {
        r.scope != "industry_dept"
            || sel.departments.is_empty()
            || sel.departments.contains(&r.layer_key)
    });
    rows.sort_by(|a, b| {
        (scope_rank(&a.scope), &a.layer_key).cmp(&(scope_rank(&b.scope), &b.layer_key))
    });
    let mut cards: std::collections::BTreeMap<String, Card> = std::collections::BTreeMap::new();
    for r in rows {
        if r.op == "tombstone" {
            cards.remove(&r.key);
            continue;
        }
        match serde_json::from_value::<UseCaseCardFields>(r.fields.clone()) {
            Ok(f) => {
                cards.insert(r.key.clone(), Card::of(f));
            }
            Err(e) => {
                tracing::warn!(activity = %r.key, layer = %r.layer_key, error = %e, "use_case_card skipped");
            }
        }
    }
    Ok(cards)
}

/// The card of `activity` over the pinned stack ([`pinned_cards`]). `None` when no layer gives one.
pub async fn pinned_card(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
    activity: &str,
) -> ApiResult<Option<Card>> {
    Ok(pinned_cards(c, pins, sel).await?.remove(activity))
}

/// A sample-dialogue turn as the web has it (`DialogueTurn`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DialogueTurn {
    pub from: &'static str,
    pub text: String,
}

/// Card turns (`{from | who | role | speaker, text}`) as [`DialogueTurn`]s; a turn without text is left out.
pub fn dialogue(turns: &[Value]) -> Vec<DialogueTurn> {
    turns
        .iter()
        .filter_map(|t| {
            let o = t.as_object()?;
            let text = o.get("text")?.as_str()?.to_owned();
            let who = ["from", "who", "role", "speaker"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str))
                .unwrap_or("customer");
            let from = match who {
                "agent" | "assistant" | "bot" => "agent",
                "system" => "system",
                _ => "customer",
            };
            Some(DialogueTurn { from, text })
        })
        .collect()
}

/// An example a card does not have: a content activity (`outcome: none`).
pub fn no_example() -> Value {
    json!({"outcome": "none"})
}
