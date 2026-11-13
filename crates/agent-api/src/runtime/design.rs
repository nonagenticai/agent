//! How the runtime reads a job's design (B4 T4, plan "How the runtime reads the design").
//!
//! - [`RuntimeDesign::load`] reads, through B2's `design::read::values_of_kind`, only the kinds the runtime needs
//!   ([`KINDS`]: `rule_values`, `llm_use`, `setting`, `languages`, and `overlay` for the tenant's prompt refinements,
//!   KH T5; the `selection` through `design::read::selection`), as
//!   `DesignOf::Live` (prod: the current release's snapshot, or the baseline of a job never released) or
//!   `DesignOf::Draft` (test: the working rows at `jobs.draft_rev`). It never reads the ledger. The operation
//!   catalogue comes from the job's pinned template compile (B6a `compile_pinned`). Context spaces and procedures
//!   are the JOB DESIGN's (B5 T12, R8, B4 owner ruling 4: `crate::job_design::runtime::read`, the bases at the
//!   design's pins with the design's B2 `overlay` / B5 `procedure` rows; `Live` and `Draft` alike), and
//!   `job_design` carries B5's gates, toolsets and role settings. A job with no job-design rows gets exactly its
//!   template content; when the job design's bases do not compile, spaces and procedures fall back to the template
//!   compile (never an empty window that B4 would not have had). The result carries the [`CaseDesign`] it was read
//!   at, and a read that raced a design change is re-read, so the stamp is always the design the values belong to.
//! - A2: [`RuntimeDesign::policies`] / [`RuntimeDesign::producers`] / [`RuntimeDesign::outputs`] come from the kinds
//!   `tool_policy` and `iam_attachment` (release snapshot for Live, working rows for Draft) plus the attached
//!   `iam_procedure` steps, read as they are when the design is read (unversioned).
//! - [`DesignCache`] keeps one design per scope for [`REFRESH`] (30 s) and drops it on a config nudge
//!   (`{root}config:changed`, crate::runtime::config_nudge): the runtime processes follow it with
//!   [`DesignCache::follow`].
//! - Nudges are sent AFTER COMMIT: the design writers (`design::track`, `track_tenant`, `drafts` discard / restore,
//!   `release::apply`, `release::follow::follow_live`) call [`nudge_on_commit`] inside their transaction, which
//!   queues a Postgres `NOTIFY` (delivered only if the transaction commits, never for a rollback). agent-api relays
//!   each to Redis ([`spawn_nudge_relay`]); a Redis outage is logged and counted there and never fails the write.
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent_model::compile::{Compiled, CompiledKnowledge, Selection};
use agent_model::kinds::instruction::FoldedInstruction;
use serde_json::{Map, Value};
use sqlx::postgres::PgListener;
use sqlx::{PgConnection, PgPool};
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::bus::Bus;
use super::catalog::PinnedCatalog;
use super::config_nudge;
use super::declared::{OutputDecl, ProducerIndex, ToolPolicy};
use super::prompt::{Refinement, refinements_of};
use super::scope::{Env, Scope};
use super::spaces::{SpaceCatalog, TemplateProcedure};
use crate::design::read::{case_design, values_of_kind};
use crate::design::{self, CaseDesign, DesignOf, DesignRow, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::{self, TemplatePin};
use crate::release::bundle::{BundleSettings, instructions_of};
use crate::release::compile::settings_of;
use crate::release::targets::BASE_SETTING_KEYS;

/// The design kinds the runtime reads through `values_of_kind` (B4's four, then B5's job-design kinds and B2's
/// `overlay`, read by `crate::job_design::runtime::read`).
pub const KINDS: [&str; 16] = [
    "rule_values",
    "llm_use",
    "setting",
    "languages",
    "entity",
    "relation",
    "procedure",
    "gate",
    "gate_question",
    "narrowing",
    "gate_setting",
    "role_setting",
    "overlay",
    "tool_policy",
    "iam_attachment",
    "job_operation",
];

/// How long a cached design serves without a nudge (the plan's "plus every 30 s").
pub const REFRESH: Duration = Duration::from_secs(30);

/// How often [`RuntimeDesign::load`] re-reads a design that changed under it before giving up.
const LOAD_ATTEMPTS: usize = 3;

/// The design a scope env reads: prod → Live, test → Draft.
pub fn design_of(env: Env) -> DesignOf {
    match env {
        Env::Prod => DesignOf::Live,
        Env::Test => DesignOf::Draft,
    }
}

/// One job's design as the runtime uses it, read at [`RuntimeDesign::case`].
#[derive(Clone, Debug)]
pub struct RuntimeDesign {
    pub job: JobRef,
    pub of: DesignOf,
    /// The release (Live) or draft revision (Draft) every value below belongs to (D63).
    pub case: CaseDesign,
    pub languages: Vec<String>,
    /// The settings in force by key (`preamble`, `actor`, `stance`, …): the template's base (`setting_bases`: the
    /// release bundle's, or what the bundle of this design would carry, `release::compile::settings_of` — the
    /// `preamble` base is the folded `identity` instruction, D108) with the job's overrides on top. A hidden setting
    /// (an override of null) and a key with neither a base nor an override are left out.
    pub settings: BTreeMap<String, Value>,
    /// D108: the folded `instruction` slots of the release snapshot (the bundle's `objects.instruction`), or of
    /// this design's compile when there is no release (Draft, or a job never released). In slot order; each section
    /// names its layer and pinned tag. Tenant refinements are not applied here: [`Self::refinements`].
    pub instructions: Vec<FoldedInstruction>,
    /// KH T5 (D108, D8): the tenant's `instruction` refinements (`overlay` places of object `instruction`) of THIS
    /// design — the release's snapshot for Live, the working rows for Draft — which crate::runtime::prompt::assemble
    /// applies over [`Self::instructions`].
    pub refinements: Vec<Refinement>,
    pub rule_values: Vec<DesignRow>,
    /// Platform definitions at the pinned release plus tenant definitions from this design's overlay snapshot.
    pub rules: Vec<crate::release::bundle::RuleSlot>,
    /// The tenant-wide `llm_use` places (D69) as the job's design holds them.
    pub llm_use: Vec<DesignRow>,
    /// `jobs.pins`: the template stack the job runs.
    pub pins: Vec<TemplatePin>,
    pub selection: Selection,
    /// B5 R8: the effective context spaces (bases at the design's pins + the design's `overlay` rows).
    pub spaces: SpaceCatalog,
    /// B5 R8: the template's versions, a name with a PUBLISHED tenant version shadowed by it.
    pub procedures: Vec<TemplateProcedure>,
    pub catalog: PinnedCatalog,
    /// D107/D115 (KH T7): the job's effective knowledge kinds of the same compile (each with its departments): what a
    /// support turn's knowledge step may search (crate::runtime::orchestrator::knowledge_scope).
    pub knowledge: CompiledKnowledge,
    /// D115 (4): the department (`industry_dept/<i>/<d>`, one of the selection's) that declares each context space,
    /// by space key; a space no selected department declares (base, industry, job, an archetype) is absent.
    pub space_departments: BTreeMap<String, String>,
    /// Why the template did not compile (`code:key` per error; empty when it did). The catalogue then holds the
    /// static built-ins only, and there are no spaces or procedures.
    pub template_errors: Vec<String>,
    /// B5: what has no B4 counterpart — the gate set, the toolsets and role settings (the one toolset source the
    /// assistant's offer and the runner's `ToolGate` consult). Empty for a registry built without a compiler
    /// (tests): no gate is asked, no toolset grants anything, the assistant is off.
    pub job_design: Arc<crate::job_design::runtime::JobDesign>,
    /// A2: the job's declared tool inputs/outputs by op key (`object.verb[{variant}]`) — the release snapshot's
    /// `tool_policy` places for Live, the working rows for Draft.
    pub policies: BTreeMap<String, ToolPolicy>,
    /// A2/A3: who produces which state key — the outputs of [`Self::policies`] plus the steps of the job's attached
    /// IAM procedures (`iam_attachment` places; the steps are read from `iam_procedure` when the design is read).
    pub producers: ProducerIndex,
    /// A2: the output fields declared to write state, by op key then output field — the op's own policy plus the
    /// steps of the attached IAM procedures that name the op (what [`Self::producers`] keeps only as field -> key).
    pub outputs: BTreeMap<String, BTreeMap<String, OutputDecl>>,
    /// A3 (owner 2026-11-01): the attached procedure whose step writes the person's identity, by the op key of that
    /// step, with the strength the procedure declares. Empty when no attached procedure establishes an identity.
    pub identity: BTreeMap<String, IdentityProducer>,
    /// Every step op key of an attached procedure that establishes the subject: the chain that makes the person. A
    /// `resolve` sub-call on one of these is not asked for the `subject` it is in the middle of establishing.
    pub identity_steps: std::collections::BTreeSet<String>,
}

/// The procedure that establishes a conversation's identity through one of its steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentityProducer {
    pub procedure: Uuid,
    pub strength: crate::runtime::declared::IdentityStrength,
}

impl RuntimeDesign {
    /// The output fields of `op_key` declared to write state (same base-operation fallback as [`Self::policy`]).
    pub fn output_decls(&self, op_key: &str) -> Option<&BTreeMap<String, OutputDecl>> {
        self.outputs.get(op_key).or_else(|| {
            op_key
                .split_once('{')
                .and_then(|(base, _)| self.outputs.get(base))
        })
    }

    /// The declaration of operation `op_key` (the exact key, else its base operation without the `{variant}`).
    pub fn policy(&self, op_key: &str) -> Option<&ToolPolicy> {
        self.policies.get(op_key).or_else(|| {
            op_key
                .split_once('{')
                .and_then(|(base, _)| self.policies.get(base))
        })
    }

    /// Read the job's design `of` (`Live` or `Draft`; a `Release(..)` is a programming error) on the caller's
    /// tenant-scoped connection.
    pub async fn load(
        c: &mut PgConnection,
        reg: &Registry,
        job: &JobRef,
        of: &DesignOf,
    ) -> ApiResult<RuntimeDesign> {
        let env = match of {
            DesignOf::Live => design::Env::Production,
            DesignOf::Draft => design::Env::Test,
            DesignOf::Release(_) => {
                return Err(ApiError::Internal(anyhow::anyhow!(
                    "the runtime reads the Live or the Draft design, never a release by id"
                )));
            }
        };
        for _ in 0..LOAD_ATTEMPTS {
            let case = case_design(&mut *c, job, env).await?;
            let d = Self::read(&mut *c, reg, job, of, case.clone()).await?;
            if case_design(&mut *c, job, env).await? == case {
                return Ok(d);
            }
        }
        Err(ApiError::Conflict(
            "design_changing",
            "The design kept changing while it was read; try again.".into(),
        ))
    }

    async fn read(
        c: &mut PgConnection,
        reg: &Registry,
        job: &JobRef,
        of: &DesignOf,
        case: CaseDesign,
    ) -> ApiResult<RuntimeDesign> {
        let languages = values_of_kind(&mut *c, reg, job, of, "languages")
            .await?
            .into_iter()
            .find_map(|r| r.snap)
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        let rule_values = values_of_kind(&mut *c, reg, job, of, "rule_values").await?;
        let llm_use = values_of_kind(&mut *c, reg, job, of, "llm_use").await?;
        let refinements = refinements_of(&values_of_kind(&mut *c, reg, job, of, "overlay").await?);
        // No stored selection reads as the empty one. It never compiles as base only, with or without a `job/` pin
        // (a template projected from an `industry_dept` head has none): `compile_pinned` is the job-context compile,
        // which grants no base-only exemption (only the platform preview, `compile_pinned_preview`, does), so a
        // selection without the D106/D115 hierarchy — empty, NULL, or legacy `industry_depts` without a department —
        // is refused `hierarchy_incomplete` and lands in `template_errors` below. R15: a pre-KH release's legacy
        // selection is upgraded in memory first (`design::read::selection`), so a complete one stays live.
        let selection = design::read::selection(&mut *c, reg, job, of).await?;
        let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(|| ApiError::Missing("job_not_found", "That job does not exist.".into()))?;
        let pins: Vec<TemplatePin> = serde_json::from_value(pins)
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("jobs.pins is unreadable: {e}")))?;
        let job_key = crate::release::compile::job_key(&pins);
        let mut knowledge = CompiledKnowledge::default();
        let mut space_departments = BTreeMap::new();
        let mut rules = Vec::new();
        let (template_spaces, template_procedures, catalog, template_errors, compiled_bases) =
            match templates::compile_pinned(&mut *c, &pins, &selection, job_key).await {
                Ok(Ok(compiled)) => {
                    rules =
                        crate::release::compile::rule_slot_items(&compiled.catalogue.rule_slots)
                            .map_err(ApiError::from)?;
                    let content = &compiled.effective.job_content;
                    knowledge = compiled.knowledge.clone();
                    space_departments = space_departments_of(&compiled, &selection);
                    (
                        SpaceCatalog::from_items(&content.context_spaces),
                        TemplateProcedure::all_from_items(&content.procedures),
                        PinnedCatalog::from_rows(&compiled.catalogue.operations),
                        Vec::new(),
                        Some((
                            settings_of(&compiled),
                            compiled.catalogue.instructions.clone(),
                        )),
                    )
                }
                Ok(Err(errors)) => (
                    SpaceCatalog::default(),
                    Vec::new(),
                    PinnedCatalog::builtins_only(),
                    errors
                        .iter()
                        .map(|e| format!("{}:{}", e.code, e.key))
                        .collect(),
                    None,
                ),
                // a provisional (v0) template does not compile yet: the runtime still runs the built-ins
                Err(ApiError::Conflict(code @ "template_not_tagged", _)) => (
                    SpaceCatalog::default(),
                    Vec::new(),
                    PinnedCatalog::builtins_only(),
                    vec![code.to_owned()],
                    None,
                ),
                Err(e) => return Err(e),
            };
        // 0246: the job's own operations (a tenant's tools) join the catalogue; the template's op of a key wins
        let mut catalog = catalog;
        for (op, j) in crate::job_design::targets::job_ops::of_design(&mut *c, reg, job, of).await?
        {
            if super::catalog::OperationCatalog::get(&catalog, &op).is_none() {
                catalog.put_row(&j.row(&op));
            }
        }
        if let CaseDesign::Live {
            release_id: Some(id),
        } = &case
        {
            rules = crate::release::readiness_rules::release_slots(&mut *c, job, id).await?;
        }
        crate::release::rule_authoring::merge(
            &mut rules,
            crate::release::rule_authoring::in_design(&mut *c, reg, job, of).await?,
        )?;
        // the bases in force, then the job's overrides on top
        let (mut settings, instructions) =
            setting_bases(&mut *c, job, &case, compiled_bases).await?;
        for r in values_of_kind(&mut *c, reg, job, of, "setting").await? {
            let Some(key) = r
                .r#ref
                .get("key")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            // no override (no snap): the base stays. An override is stored as `{value, base_hash}` (B2 T8); a
            // null value hides the setting, base included.
            let Some(snap) = r.snap.filter(|s| !s.is_null()) else {
                continue;
            };
            let v = match snap {
                Value::Object(mut o) if o.contains_key("value") => {
                    o.remove("value").unwrap_or(Value::Null)
                }
                other => other,
            };
            if v.is_null() {
                settings.remove(&key);
            } else {
                settings.insert(key, v);
            }
        }
        if !template_errors.is_empty() {
            tracing::warn!(job = %job.key, errors = ?template_errors, "runtime design: the template did not compile");
        }
        // B5 T12 (R8, ruling 4): spaces and procedures from the job design, plus what has no B4 counterpart
        let (spaces, procedures, job_design) = match reg.compiler() {
            Some(compiler) => {
                let parts =
                    crate::job_design::runtime::read(&mut *c, reg, compiler, job, of).await?;
                if parts.bases.template_errors.is_empty() {
                    (parts.spaces, parts.procedures, Arc::new(parts.design))
                } else {
                    tracing::warn!(
                        job = %job.key, errors = ?parts.bases.template_errors,
                        "runtime design: the job design's bases did not compile; template spaces and procedures"
                    );
                    (template_spaces, template_procedures, Arc::new(parts.design))
                }
            }
            None => (template_spaces, template_procedures, Arc::default()),
        };
        let (policies, producers, outputs, identity, identity_steps) =
            declared_of(&mut *c, reg, job, of).await?;
        Ok(RuntimeDesign {
            job: job.clone(),
            of: of.clone(),
            case,
            languages,
            settings,
            instructions,
            refinements,
            rule_values,
            rules,
            llm_use,
            pins,
            selection,
            spaces,
            procedures,
            catalog,
            knowledge,
            space_departments,
            template_errors,
            job_design,
            policies,
            producers,
            outputs,
            identity,
            identity_steps,
        })
    }

    /// An empty design of `job` (no template content, the built-in catalogue, no job design): for tests and seams
    /// that need a design value without a database.
    pub fn blank(job: JobRef, of: DesignOf) -> RuntimeDesign {
        RuntimeDesign {
            job,
            case: match of {
                DesignOf::Live => CaseDesign::Live { release_id: None },
                _ => CaseDesign::Draft { draft_rev: 0 },
            },
            of,
            languages: Vec::new(),
            settings: BTreeMap::new(),
            instructions: Vec::new(),
            refinements: Vec::new(),
            rule_values: Vec::new(),
            rules: Vec::new(),
            llm_use: Vec::new(),
            pins: Vec::new(),
            selection: Selection::default(),
            spaces: SpaceCatalog::default(),
            procedures: Vec::new(),
            catalog: PinnedCatalog::builtins_only(),
            knowledge: CompiledKnowledge::default(),
            space_departments: BTreeMap::new(),
            template_errors: Vec::new(),
            job_design: Arc::default(),
            policies: BTreeMap::new(),
            producers: ProducerIndex::default(),
            outputs: BTreeMap::new(),
            identity: BTreeMap::new(),
            identity_steps: std::collections::BTreeSet::new(),
        }
    }
}

/// A2/A3: the job's declared tool policies (`tool_policy` places, key `tool-policy:<op>`) and the producer index over
/// them and the job's attached IAM procedures (`iam_attachment` places, key `iam-attachment:<id>`; the steps are the
/// tenant's `iam_procedure` rows as they are now, unversioned; an attachment whose procedure is gone is skipped). A
/// place that does not decode is skipped with a WARN naming its key, never its value.
async fn declared_of(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<(
    BTreeMap<String, ToolPolicy>,
    ProducerIndex,
    BTreeMap<String, BTreeMap<String, OutputDecl>>,
    BTreeMap<String, IdentityProducer>,
    std::collections::BTreeSet<String>,
)> {
    let mut policies = BTreeMap::new();
    for r in values_of_kind(&mut *c, reg, job, of, "tool_policy").await? {
        let Some(op) = r.key.strip_prefix("tool-policy:") else {
            continue;
        };
        let Some(v) = r.snap.filter(|v| !v.is_null()) else {
            continue;
        };
        match serde_json::from_value::<ToolPolicy>(v) {
            Ok(p) => {
                policies.insert(op.to_owned(), p);
            }
            Err(_) => {
                tracing::warn!(job = %job.key, key = %r.key, "runtime design: a tool policy does not decode")
            }
        }
    }
    let ids: Vec<Uuid> = values_of_kind(&mut *c, reg, job, of, "iam_attachment")
        .await?
        .into_iter()
        .filter(|r| r.snap.as_ref().is_some_and(|v| !v.is_null()))
        .filter_map(|r| {
            r.key
                .strip_prefix("iam-attachment:")
                .and_then(|id| Uuid::parse_str(id).ok())
        })
        .collect();
    let mut procedures = Vec::new();
    if !ids.is_empty() {
        let rows: Vec<crate::iam::ProcedureRow> = sqlx::query_as(
            "SELECT id, name, description, strength, steps, created_at, updated_at, updated_by \
               FROM iam_procedure WHERE id = ANY($1) ORDER BY name",
        )
        .bind(&ids)
        .fetch_all(&mut *c)
        .await?;
        for r in rows {
            match r.procedure() {
                Ok(p) => procedures.push(p),
                Err(_) => {
                    tracing::warn!(job = %job.key, procedure = %r.id, "runtime design: an IAM procedure does not decode")
                }
            }
        }
    }
    let list: Vec<(String, ToolPolicy)> = policies
        .iter()
        .map(|(k, p)| (k.clone(), p.clone()))
        .collect();
    let producers = ProducerIndex::build(&list, &procedures);
    let mut outputs: BTreeMap<String, BTreeMap<String, OutputDecl>> = BTreeMap::new();
    let steps = procedures
        .iter()
        .flat_map(|p| p.steps.iter().map(|s| (&s.op, &s.outputs)));
    for (op, decls) in policies.iter().map(|(o, p)| (o, &p.outputs)).chain(steps) {
        if !decls.is_empty() {
            let e = outputs.entry(op.clone()).or_default();
            for (field, d) in decls {
                e.entry(field.clone()).or_insert_with(|| d.clone());
            }
        }
    }
    // which attached procedure's step establishes the person's identity (attach validation keeps it to one per job)
    let mut identity: BTreeMap<String, IdentityProducer> = BTreeMap::new();
    let mut identity_steps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for p in &procedures {
        let establishes = p
            .steps
            .iter()
            .any(|s| s.outputs.values().any(|o| o.key == "subject"));
        if establishes {
            identity_steps.extend(p.steps.iter().map(|s| s.op.clone()));
        }
        for s in &p.steps {
            if s.outputs
                .values()
                .any(|o| crate::runtime::declared::IDENTITY_KEYS.contains(&o.key.as_str()))
            {
                identity.entry(s.op.clone()).or_insert(IdentityProducer {
                    procedure: p.id,
                    strength: p.strength,
                });
            }
        }
    }
    Ok((policies, producers, outputs, identity, identity_steps))
}

/// D115 (4): the department that declares each context space of `compiled` — the first layer of the space's
/// provenance that is one of the selection's departments.
fn space_departments_of(compiled: &Compiled, selection: &Selection) -> BTreeMap<String, String> {
    compiled
        .effective
        .job_content
        .context_spaces
        .keys()
        .filter_map(|key| {
            let origins = compiled
                .effective
                .provenance
                .get(&format!("context_space:{key}"))?;
            origins
                .iter()
                .map(|o| o.layer.label())
                .find(|l| selection.departments.contains(l))
                .map(|d| (key.clone(), d))
        })
        .collect()
}

/// The base value in force of each job setting ([`BASE_SETTING_KEYS`]), before the job's overrides, and the folded
/// instructions (D108): the release bundle's `settings` and `objects.instruction` when the design is a release
/// (what the release froze), else what the bundle of this design would carry — `compiled`, the
/// `release::compile::settings_of` bases and `catalogue.instructions` of the same B6a compile of the same pins and
/// selection the LayerCompiler runs (`None` when the template does not compile). A key without a base is absent.
async fn setting_bases(
    c: &mut PgConnection,
    job: &JobRef,
    case: &CaseDesign,
    compiled: Option<(BundleSettings, Vec<FoldedInstruction>)>,
) -> ApiResult<(BTreeMap<String, Value>, Vec<FoldedInstruction>)> {
    if let CaseDesign::Live {
        release_id: Some(id),
    } = case
    {
        let stored: Option<(Option<Value>, Option<Value>)> = sqlx::query_as(
            "SELECT bundle -> 'settings', bundle -> 'objects' -> 'instruction' \
               FROM job_release WHERE job_id = $1 AND release_id = $2",
        )
        .bind(job.id)
        .bind(id.clone())
        .fetch_optional(&mut *c)
        .await?;
        let (settings, instructions) = stored.unwrap_or_default();
        let items: Vec<Map<String, Value>> = instructions
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        return Ok((
            bases_of(settings.as_ref().and_then(Value::as_object)),
            instructions_of(&items),
        ));
    }
    let Some((settings, instructions)) = compiled else {
        return Ok((BTreeMap::new(), Vec::new()));
    };
    let fields = match serde_json::to_value(&settings) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    Ok((bases_of(Some(&fields)), instructions))
}

/// The string [`BASE_SETTING_KEYS`] of `fields`.
fn bases_of(fields: Option<&Map<String, Value>>) -> BTreeMap<String, Value> {
    let Some(fields) = fields else {
        return BTreeMap::new();
    };
    BASE_SETTING_KEYS
        .iter()
        .filter_map(|k| {
            fields
                .get(*k)
                .filter(|v| v.is_string())
                .map(|v| ((*k).to_owned(), v.clone()))
        })
        .collect()
}

// ---- the per-scope cache ----------------------------------------------------------------------------------------

struct Cached {
    at: Instant,
    design: Arc<RuntimeDesign>,
}

#[derive(Default)]
struct CacheInner {
    entries: HashMap<Scope, Cached>,
    /// Bumped by every invalidation: a load that started before a nudge never caches its (stale) result.
    generation: HashMap<Scope, u64>,
}

/// One [`RuntimeDesign`] per scope, dropped on a nudge or after the refresh interval. The lock is never held
/// across an `.await`.
pub struct DesignCache {
    ttl: Duration,
    inner: Mutex<CacheInner>,
}

impl Default for DesignCache {
    fn default() -> Self {
        Self::new(REFRESH)
    }
}

impl DesignCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(CacheInner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The scope's cached design while it is fresh.
    pub fn cached(&self, s: &Scope) -> Option<Arc<RuntimeDesign>> {
        let g = self.lock();
        g.entries
            .get(s)
            .filter(|e| e.at.elapsed() < self.ttl)
            .map(|e| e.design.clone())
    }

    /// Drop the scope's design (a nudge): the next read loads it again.
    pub fn invalidate(&self, s: &Scope) {
        let mut g = self.lock();
        g.entries.remove(s);
        *g.generation.entry(s.clone()).or_insert(0) += 1;
    }

    fn generation(&self, s: &Scope) -> u64 {
        self.lock().generation.get(s).copied().unwrap_or(0)
    }

    /// Cache `d` for `s` unless the scope was invalidated since generation `seen` was read.
    fn put(&self, s: &Scope, seen: u64, d: Arc<RuntimeDesign>) {
        let mut g = self.lock();
        if g.generation.get(s).copied().unwrap_or(0) == seen {
            g.entries.insert(
                s.clone(),
                Cached {
                    at: Instant::now(),
                    design: d,
                },
            );
        }
    }

    /// Cache `d` for `s` as loaded now, whatever the generation: a caller that already holds the scope's design (a
    /// warm-up, a test that pins one).
    pub fn prime(&self, s: &Scope, d: Arc<RuntimeDesign>) {
        self.lock().entries.insert(
            s.clone(),
            Cached {
                at: Instant::now(),
                design: d,
            },
        );
    }

    /// The scope's design: cached, else loaded (`s.env` picks Live or Draft) on the caller's tenant-scoped
    /// connection. `job` must be the scope's job.
    pub async fn get_or_load(
        &self,
        c: &mut PgConnection,
        reg: &Registry,
        s: &Scope,
        job: &JobRef,
    ) -> ApiResult<Arc<RuntimeDesign>> {
        if job.key != s.job {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "design cache: job {} is not the scope's job {}",
                job.key,
                s.job
            )));
        }
        if let Some(d) = self.cached(s) {
            return Ok(d);
        }
        let seen = self.generation(s);
        let d = Arc::new(RuntimeDesign::load(c, reg, job, &design_of(s.env)).await?);
        self.put(s, seen, d.clone());
        Ok(d)
    }

    /// Follow the config nudges on `root` and drop each nudged scope, until the task is aborted. A lost
    /// subscription is re-made after a short backoff (the refresh interval covers the gap).
    pub fn follow(self: &Arc<Self>, root: Bus) -> JoinHandle<()> {
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let c = Arc::clone(&cache);
                if let Err(e) = config_nudge::follow(&root, move |s| c.invalidate(&s)).await {
                    tracing::warn!("design cache: config nudge subscription failed: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    }
}

// ---- nudges after commit ----------------------------------------------------------------------------------------

/// The Postgres channel a design writer queues its nudge on; the payload is the scope text `<tenant>/<job>/<env>`.
pub const PG_NUDGE_CHANNEL: &str = "agent_config_changed";

/// Queue the config nudge of `jobs` in `env` on the caller's transaction: Postgres delivers it to
/// [`spawn_nudge_relay`] when (and only when) the transaction commits. Takes no lock.
pub async fn nudge_on_commit(c: &mut PgConnection, jobs: &[Uuid], env: Env) -> ApiResult<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "SELECT pg_notify($1, tenant_id::text || '/' || key || '/' || $3) FROM jobs WHERE id = ANY($2)",
    )
    .bind(PG_NUDGE_CHANNEL)
    .bind(jobs)
    .bind(env.as_str())
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn listen(db: &PgPool) -> Result<PgListener, sqlx::Error> {
    let mut l = PgListener::connect_with(db).await?;
    l.listen(PG_NUDGE_CHANNEL).await?;
    Ok(l)
}

/// Publish one queued nudge on Redis (never fails: crate::runtime::config_nudge::publish).
async fn forward(redis: &Bus, payload: &str) {
    match config_nudge::parse(payload) {
        Some(s) => config_nudge::publish(Some(redis), s.tenant, &s.job, s.env).await,
        None => tracing::warn!("ignoring a malformed design nudge"),
    }
}

/// Relay until the pool closes. `PgListener` reconnects (and re-listens) by itself after a lost connection.
async fn relay(db: PgPool, redis: Bus, mut l: PgListener) {
    loop {
        match l.recv().await {
            Ok(n) => forward(&redis, n.payload()).await,
            Err(e) => {
                if db.is_closed() {
                    return;
                }
                metrics::counter!("agent_config_nudge_failures_total").increment(1);
                tracing::warn!("design nudge relay: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Start relaying the design writers' nudges from Postgres to Redis; returns once `LISTEN` is active (so a write
/// committed after it is relayed). Tests use it directly; agent-api boots through [`spawn_nudge_relay`].
pub async fn start_nudge_relay(db: PgPool, redis: Bus) -> anyhow::Result<JoinHandle<()>> {
    let l = listen(&db).await?;
    Ok(tokio::spawn(relay(db, redis, l)))
}

/// agent-api's relay (when `REDIS_URL` is set): retries the first `LISTEN` with a backoff, never fails the boot.
/// A lost nudge only delays a change by the runtime's 30 s refresh.
pub fn spawn_nudge_relay(db: PgPool, redis: Bus) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match listen(&db).await {
                Ok(l) => {
                    relay(db, redis, l).await;
                    return;
                }
                Err(e) => tracing::warn!("design nudge relay: LISTEN failed: {e}"),
            }
            if db.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidation_beats_a_racing_load() {
        let cache = DesignCache::default();
        let s = Scope::new(Uuid::from_u128(7), "care", Env::Test).unwrap();
        let seen = cache.generation(&s);
        cache.invalidate(&s);
        let mut d = RuntimeDesign::blank(
            JobRef {
                id: Uuid::from_u128(8),
                key: "care".into(),
            },
            DesignOf::Draft,
        );
        d.case = CaseDesign::Draft { draft_rev: 1 };
        cache.put(&s, seen, Arc::new(d.clone()));
        assert!(
            cache.cached(&s).is_none(),
            "a load older than the nudge is not cached"
        );
        cache.put(&s, cache.generation(&s), Arc::new(d));
        assert!(cache.cached(&s).is_some());
        let stale = DesignCache::new(Duration::ZERO);
        assert!(stale.cached(&s).is_none());
    }
}
