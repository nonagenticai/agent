//! The tag → pin contract (plan B6 §4, B6-3, B6-4, B6-6; Ruling 17): what B1, B2, B5, B7 and B8 read of the
//! platform model. Every function takes the caller's connection and runs inside the caller's transaction; the
//! readers work as `agent_app` or `agent_platform`, the projection needs `agent_platform`.
//!
//! - [`job_templates`]: the job templates (B1's `job_template_tag`, filled by the projection below).
//! - [`resolve_template`]: one template's pins, default selection and label (404 `template_not_found`, 409
//!   `template_not_tagged`, B1's codes).
//! - [`latest_tags`]: latest non-provisional version per family (re-exported from the store).
//! - [`compile_pinned`]: compile a selection over the members of pinned tags only (B6-6).
//! - [`compile_head`] / [`head_model`]: compile over the current rows (`layer_head`), the platform compile page's
//!   preview (B6b T9b, R4); the head model also feeds `departmentArchetypes`' knowledge fold.
//! - [`tag_diff`]: what changed between two tags of a family, by (kind, key), with `fields_hash` before/after.
//! - [`project_job_template_tags`]: the `job_template_tag` projection of one family; `tag_family` and the boot
//!   seeder both call it, under the family lock.
//!
//! # Projection rules
//! A family projects when its head is a `job`, or an `industry_dept` that no job head specialises (no job head's
//! `template_selection` names its industry and the department's layer key among its `departments` — that job is
//! the template, e.g.
//! `job/customer_care_agent` over `industry_dept/retail/customer_service`). Projected versions: the latest
//! non-provisional tag, the one before it, and a provisional v0; rows already in the table for older versions are
//! kept and re-labelled. Per row: `key` = the head's job / department, `industry` = the head's template industry,
//! `pins` = the tag's recorded stack ∪ the tag itself (base, industry, industry_dept, job), `status` = `draft` for
//! the provisional v0 else `tagged`. `label`: a new row takes the head label; an existing row keeps its label (B1's
//! curated 0033 labels); every non-latest tagged version carries the suffix ` (previous tag)`. The upsert changes
//! nothing when a row is already equal and never touches `created_by`.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::compile::{
    self, AxisRuleDef, CompileError, CompileInput, Compiled, DepartmentRef, LayerMeta, ProfileDef,
    Selection, SelectionIssue,
};
use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use agent_model::layer::Scope;
use agent_model::tags::{self as model_tags, TagDiffRow};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};
use crate::platform::db::layers::{self, LAYER_COLUMNS, Layer};
use crate::platform::db::rows::KeyedRow;
use crate::platform::db::tags::{self as db_tags, StackEntry, TemplateTag};
use crate::platform::db::{FAMILY_LOCK_SPACE, corrupt};

pub use crate::platform::db::tags::latest_tags;

/// One pin: a family (its head's key) at a tag version. The same shape as a tag's stack entry.
pub type TemplatePin = StackEntry;

/// One diff row of [`tag_diff`] (plan §4 `ItemChange`: kind, key, label, change, before/after detail and
/// `fields_hash`).
pub type ItemChange = TagDiffRow;

/// The label suffix of a non-latest tagged template (B1 0033: `Customer care agent (previous tag)`).
pub const PREVIOUS_TAG_SUFFIX: &str = " (previous tag)";

/// `job_template_tag.label` is at most this many characters (the table's CHECK).
const MAX_LABEL_CHARS: usize = 120;

/// One job template (UI `JobTemplateTag` + B1's `key`): a `job_template_tag` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobTemplateTag {
    pub layer: String,
    pub version: i32,
    pub key: String,
    pub label: String,
    pub industry: Option<String>,
    pub pins: Vec<TemplatePin>,
    /// `tagged` | `draft` (the provisional v0).
    pub status: String,
}

/// What a job created from a template starts from (plan §4 `TemplateResolution`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TemplateResolution {
    pub pins: Vec<TemplatePin>,
    /// The head's `template_selection` (D65), else its industry and department.
    pub default_selection: Selection,
    pub label: String,
}

type TemplateRow = (String, i32, String, String, Option<String>, Value, String);

const TEMPLATE_COLUMNS: &str = "layer, version, key, label, industry, pins, status";

fn template(row: TemplateRow) -> ApiResult<JobTemplateTag> {
    let (layer, version, key, label, industry, pins, status) = row;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins).map_err(|e| {
        corrupt(format!(
            "job_template_tag {layer}@v{version} has unreadable pins: {e}"
        ))
    })?;
    Ok(JobTemplateTag {
        layer,
        version,
        key,
        label,
        industry,
        pins,
        status,
    })
}

/// Every job template (`job_template_tag`), sorted by (layer, version) bytewise.
pub async fn job_templates(conn: &mut PgConnection) -> ApiResult<Vec<JobTemplateTag>> {
    let rows: Vec<TemplateRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TEMPLATE_COLUMNS} FROM job_template_tag"
    )))
    .fetch_all(conn)
    .await?;
    let mut out = rows
        .into_iter()
        .map(template)
        .collect::<ApiResult<Vec<_>>>()?;
    out.sort_by(|a, b| (a.layer.as_str(), a.version).cmp(&(b.layer.as_str(), b.version)));
    Ok(out)
}

/// The template `layer@v{version}`: 404 `template_not_found`, 409 `template_not_tagged` for a draft (the codes
/// and wording of B1's `createJobFromTemplate`).
pub async fn resolve_template(
    conn: &mut PgConnection,
    layer: &str,
    version: i32,
) -> ApiResult<TemplateResolution> {
    let not_found = || {
        ApiError::Missing(
            "template_not_found",
            format!("no job template {layer}@v{version}"),
        )
    };
    let row: Option<TemplateRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TEMPLATE_COLUMNS} FROM job_template_tag WHERE layer = $1 AND version = $2"
    )))
    .bind(layer)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?;
    let t = template(row.ok_or_else(not_found)?)?;
    if t.status != "tagged" {
        return Err(ApiError::Conflict(
            "template_not_tagged",
            format!("{layer}@v{version} is a draft; only a tagged template creates jobs"),
        ));
    }
    let head = layers::find_layer_by_key(&mut *conn, layer)
        .await?
        .ok_or_else(not_found)?;
    let default_selection = head_selection(&head)?;
    Ok(TemplateResolution {
        pins: t.pins,
        default_selection,
        label: t.label,
    })
}

/// The selection a template head starts its jobs with (D65): its `template_selection`, else its industry and — for
/// an `industry_dept` head — itself as the one department. Not validated here: a head without the D106/D115
/// hierarchy yields an incomplete selection, which [`template_refusal`] refuses.
pub fn head_selection(head: &Layer) -> ApiResult<Selection> {
    match &head.template_selection {
        Some(v) if !v.is_null() => serde_json::from_value::<Selection>(v.clone()).map_err(|e| {
            corrupt(format!(
                "layer {} has an unreadable template_selection: {e}",
                head.key
            ))
        }),
        _ => Ok(Selection {
            industry: head.template_industry().unwrap_or_default().to_owned(),
            departments: if head.scope == "industry_dept" {
                vec![head.key.clone()]
            } else {
                Vec::new()
            },
            ..Selection::default()
        }),
    }
}

/// 422 for `issues` through the one mapper ([`compile::issues_refusal`]), with every issue in
/// `errors[{code, path, message}]`; `Ok` for none.
pub fn issues_refusal(issues: &[SelectionIssue]) -> ApiResult<()> {
    let Some(r) =
        compile::issues_refusal(issues.iter().map(|i| (i.code.as_str(), i.message.as_str())))
    else {
        return Ok(());
    };
    let (code, message) = (r.code, r.message);
    let errors = issues
        .iter()
        .map(|i| json!({"code": i.code, "path": i.path, "message": i.message}))
        .collect::<Vec<_>>();
    Err(
        ApiError::detailed(StatusCode::UNPROCESSABLE_ENTITY, code, message)
            .with("errors", Value::Array(errors)),
    )
}

/// The 422 of a pinned compile refused for its selection ([`compile::refusal`] → a D106/D115 code), with
/// `errors[{code, path, message}]` (a compile error's `key` is the selection path). A 404 `profile_not_found`
/// refusal keeps its own status and code.
pub fn compile_refusal(errors: &[CompileError]) -> ApiError {
    let Some(r) = compile::refusal(errors) else {
        return ApiError::detailed(
            StatusCode::UNPROCESSABLE_ENTITY,
            "compile_failed",
            "The template does not compile.",
        );
    };
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
    let e = ApiError::detailed(status, r.code, r.message);
    if r.code == compile::code::PROFILE_NOT_FOUND {
        return e;
    }
    let list = errors
        .iter()
        .map(|x| json!({"code": x.code, "path": x.key, "message": x.message}))
        .collect::<Vec<_>>();
    e.with("errors", Value::Array(list))
}

/// D106/D115 at every entry point (template registration, job creation, `PUT /target`, cut): `Ok` when `sel`
/// holds the hierarchy ([`Selection::hierarchy_issues`]), else 422 under the first issue's code
/// (`hierarchy_incomplete`, `department_industry_mismatch`, `duplicate_department`, `invalid_selection`) with every
/// issue in `errors[{code, path, message}]`.
pub fn hierarchy_refusal(sel: &Selection) -> ApiResult<()> {
    issues_refusal(&sel.hierarchy_issues())
}

/// Where the pins break the selection's hierarchy (D115 "`jobs.pins` carries one `industry_dept` pin per
/// department"): no pin of the selection's `industry/<i>` family or of a selected department
/// (`hierarchy_incomplete`, path `pins`), or a pin of another industry's layer (`department_industry_mismatch`).
pub fn pin_issues(sel: &Selection, pins: &[TemplatePin]) -> Vec<SelectionIssue> {
    let mut out = Vec::new();
    let issue = |code: &str, message: String| SelectionIssue {
        code: code.to_owned(),
        path: "pins".to_owned(),
        message,
    };
    let industry = format!("industry/{}", sel.industry);
    if !pins.iter().any(|p| p.layer == industry) {
        out.push(issue(
            compile::code::HIERARCHY_INCOMPLETE,
            format!("The template pins no {industry}."),
        ));
    }
    for dept in &sel.departments {
        if !pins.iter().any(|p| p.layer == *dept) {
            out.push(issue(
                compile::code::HIERARCHY_INCOMPLETE,
                format!("The template pins no {dept}."),
            ));
        }
    }
    // D116: a base_dept pin carries no industry, so it is never another industry's (like base)
    for p in pins {
        let other = match LayerKey::parse_label(&p.layer) {
            Ok(k) => k.industry.filter(|i| {
                matches!(k.scope, Scope::Industry | Scope::IndustryDept) && *i != sel.industry
            }),
            Err(_) => None,
        };
        if let Some(other) = other {
            out.push(issue(
                compile::code::DEPARTMENT_INDUSTRY_MISMATCH,
                format!(
                    "The template pins {} of {other}, not of {}.",
                    p.layer, sel.industry
                ),
            ));
        }
    }
    out
}

/// The pins a hierarchy change keeps (D115; `PUT /target` of a job with no release): the `base`, profile, `job/*`
/// and dimension families stay as they are; an `industry/<i>` pin only for the selection's industry and an
/// `industry_dept/<i>/<d>` pin only for a selected department; no `base_dept/*` — D116: [`complete_pins`] re-adds
/// the archetype the selected department composes on. A pin the layer grammar cannot read is kept: never silently
/// drop what cannot be classified.
pub fn reconcile_pins(pins: &[TemplatePin], sel: &Selection) -> Vec<TemplatePin> {
    let industry = format!("industry/{}", sel.industry);
    pins.iter()
        .filter(|p| match LayerKey::parse_label(&p.layer) {
            Ok(k) => match k.scope {
                Scope::Industry => p.layer == industry,
                Scope::IndustryDept => sel.departments.contains(&p.layer),
                Scope::BaseDept => false,
                _ => true,
            },
            Err(_) => true,
        })
        .cloned()
        .collect()
}

/// The platform's region vocabulary (D111): the values of the `region` dimension (0042 `dimension_value`, curated by
/// the platform in its seed — the same list the P13 dimensions page shows), by key.
pub async fn region_vocabulary(conn: &mut PgConnection) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT key FROM dimension_value WHERE dimension = 'region' ORDER BY key",
    )
    .fetch_all(conn)
    .await?)
}

/// D111 at every entry point that takes a selection (template registration, job creation, `PUT /target`, cut, a
/// restored selection): `Ok` without a region or for a region of the platform vocabulary ([`region_vocabulary`]),
/// else 422 `unknown_region` with `errors[{code, path: "selection.region", message}]`. Without a region no region
/// sub-layer applies; nothing here needs the region's sub-layers to exist (no region content is required).
pub async fn region_refusal(conn: &mut PgConnection, sel: &Selection) -> ApiResult<()> {
    let Some(region) = sel.region.as_deref() else {
        return Ok(());
    };
    let known = region_vocabulary(&mut *conn).await?;
    if known.iter().any(|k| k == region) {
        return Ok(());
    }
    let message = if known.is_empty() {
        format!("Unknown region “{region}”: the platform defines no regions.")
    } else {
        format!(
            "Unknown region “{region}”; the platform's regions are {}.",
            known.join(", ")
        )
    };
    Err(ApiError::detailed(
        StatusCode::UNPROCESSABLE_ENTITY,
        compile::code::UNKNOWN_REGION,
        message.clone(),
    )
    .with(
        "errors",
        json!([{"code": compile::code::UNKNOWN_REGION, "path": "selection.region", "message": message}]),
    ))
}

/// [`hierarchy_refusal`] of a selection, then its [`pin_issues`] against `pins`: what creating a job from a template
/// and `PUT …/target` (against `jobs.pins`) refuse.
pub fn template_refusal(sel: &Selection, pins: &[TemplatePin]) -> ApiResult<()> {
    hierarchy_refusal(sel)?;
    issues_refusal(&pin_issues(sel, pins))
}

// --- projection -----------------------------------------------------------------------------------------------------

/// The `job_template_tag` projection of `family` (see the module doc), in the caller's `agent_platform`
/// transaction. Takes the family lock (re-entrant: `tag_family` already holds it). A family that does not project
/// (base, industry, a specialised industry_dept, no tag yet) writes nothing.
///
/// D106/D115 template registration: when the family projects a tagged template, its selection ([`head_selection`])
/// must hold the hierarchy ([`hierarchy_refusal`]): 422 `hierarchy_incomplete` / `department_industry_mismatch` /
/// `duplicate_department` otherwise, and the caller's transaction (the tag) rolls back. The pins are checked when
/// a job is created from the template ([`template_refusal`]): a tag's stack is what is tagged at that moment. The
/// boot seeder calls [`project`] directly: the shipped seed v1 is checked by its tests, and a job from a template
/// without the hierarchy is still refused at creation.
pub async fn project_job_template_tags(
    conn: &mut PgConnection,
    family: &str,
    created_by: &str,
) -> ApiResult<()> {
    project(&mut *conn, family, created_by)
        .await
        .map_err(ApiError::from)?;
    let registered: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM job_template_tag WHERE layer = $1 AND status = 'tagged')",
    )
    .bind(family)
    .fetch_one(&mut *conn)
    .await?;
    if !registered {
        return Ok(());
    }
    let head = layers::layer_by_key(&mut *conn, family).await?;
    let sel = head_selection(&head)?;
    hierarchy_refusal(&sel)?;
    region_refusal(&mut *conn, &sel).await
}

/// [`project_job_template_tags`] with the database error unwrapped (the boot seeder classifies sqlx errors).
pub(crate) async fn project(
    conn: &mut PgConnection,
    family: &str,
    created_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(FAMILY_LOCK_SPACE)
        .bind(family)
        .execute(&mut *conn)
        .await?;
    let head: Option<Layer> = sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAYER_COLUMNS} FROM layer WHERE key = $1 AND family = key"
    )))
    .bind(family)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(head) = head else {
        return Ok(());
    };
    // D116: a base_dept head is no job template (it carries no template_selection); like base and industry it
    // projects nothing
    let key = match head.scope.as_str() {
        "job" => head.job.clone(),
        "industry_dept" => head.department.clone(),
        _ => None,
    };
    let Some(key) = key else {
        return Ok(());
    };
    if head.scope == "industry_dept" {
        let specialised: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM layer \
                             WHERE scope = 'job' AND family = key \
                               AND template_selection ->> 'industry' = $1 \
                               AND template_selection -> 'departments' @> jsonb_build_array($2::text))",
        )
        .bind(head.industry.as_deref())
        .bind(head.key.as_str())
        .fetch_one(&mut *conn)
        .await?;
        if specialised {
            return Ok(());
        }
    }

    let tags: Vec<(i32, bool, Value)> = sqlx::query_as(
        "SELECT version, provisional, stack FROM layer_tag WHERE family = $1 ORDER BY version",
    )
    .bind(family)
    .fetch_all(&mut *conn)
    .await?;
    let existing: BTreeMap<i32, String> = sqlx::query_as::<_, (i32, String)>(
        "SELECT version, label FROM job_template_tag WHERE layer = $1",
    )
    .bind(family)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    let released: Vec<i32> = tags.iter().filter(|t| !t.1).map(|t| t.0).collect();
    let latest = released.last().copied();
    let previous = released.iter().rev().nth(1).copied();
    let head_label = clip(
        head.label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(head.key.as_str()),
        MAX_LABEL_CHARS,
    );
    let industry = head.template_industry().map(str::to_owned);

    let mut items: Vec<Value> = Vec::new();
    for (version, provisional, stack) in &tags {
        let version = *version;
        let wanted = *provisional || Some(version) == latest || Some(version) == previous;
        let stored = existing.get(&version);
        if !wanted && stored.is_none() {
            continue;
        }
        // A new row takes the curated label of the nearest earlier released row (suffix stripped), so billing@2
        // reads as billing@1 did; else the head label.
        let base = match stored {
            Some(l) => l.clone(),
            None => existing
                .range(..version)
                .rev()
                .find(|(v, _)| released.contains(*v))
                .map(|(_, l)| l.strip_suffix(PREVIOUS_TAG_SUFFIX).unwrap_or(l).to_owned())
                .unwrap_or_else(|| head_label.clone()),
        };
        let label = if !*provisional && Some(version) != latest {
            with_previous_suffix(&base)
        } else {
            base
        };
        let status = if *provisional { "draft" } else { "tagged" };
        let mut pins: Vec<Value> = stack.as_array().cloned().unwrap_or_default();
        pins.push(json!({"layer": family, "version": version}));
        items.push(json!({
            "layer": family,
            "version": version,
            "key": key,
            "label": label,
            "industry": industry,
            "pins": pins,
            "status": status,
        }));
    }
    if items.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO job_template_tag (layer, version, key, label, industry, pins, status, created_by) \
         SELECT x.layer, x.version, x.key, x.label, x.industry, x.pins, x.status, $2 \
           FROM jsonb_to_recordset($1::jsonb) AS x(layer text, version integer, key text, label text, \
                industry text, pins jsonb, status text) \
         ON CONFLICT (layer, version) DO UPDATE \
            SET key = EXCLUDED.key, label = EXCLUDED.label, industry = EXCLUDED.industry, \
                pins = EXCLUDED.pins, status = EXCLUDED.status \
          WHERE (job_template_tag.key, job_template_tag.label, job_template_tag.industry, job_template_tag.pins, \
                 job_template_tag.status) \
                IS DISTINCT FROM (EXCLUDED.key, EXCLUDED.label, EXCLUDED.industry, EXCLUDED.pins, \
                 EXCLUDED.status)",
    )
    .bind(Value::Array(items))
    .bind(created_by)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// At most `max` characters of `s`.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `label` with [`PREVIOUS_TAG_SUFFIX`] (once), within [`MAX_LABEL_CHARS`].
fn with_previous_suffix(label: &str) -> String {
    if label.ends_with(PREVIOUS_TAG_SUFFIX) {
        return label.to_owned();
    }
    let room = MAX_LABEL_CHARS - PREVIOUS_TAG_SUFFIX.chars().count();
    format!("{}{PREVIOUS_TAG_SUFFIX}", clip(label, room).trim_end())
}

// --- pinned compile + diff ------------------------------------------------------------------------------------------

/// A store row as the compile reads it.
fn compile_row(r: &KeyedRow) -> ApiResult<compile::Row> {
    let kind = Kind::parse(&r.kind)
        .ok_or_else(|| corrupt(format!("row {} has kind {:?}", r.id, r.kind)))?;
    let op = match r.op.as_str() {
        "add" => Op::Add,
        "patch" => Op::Patch,
        "tombstone" => Op::Tombstone,
        other => return Err(corrupt(format!("row {} has op {other:?}", r.id))),
    };
    Ok(compile::Row {
        id: r.id.to_string(),
        layer_id: r.layer_id.to_string(),
        kind,
        key: r.key.clone(),
        op,
        fields: r.fields.clone(),
        rev: r.rev,
    })
}

/// A layer as the compile reads it; `tag` (display only) is `family@vN` of the tag it is read at.
fn layer_meta(l: &Layer, tag: Option<&TemplateTag>) -> ApiResult<LayerMeta> {
    let key = LayerKey::parse_label(&l.key)
        .map_err(|e| corrupt(format!("layer {} has an unreadable key: {e}", l.key)))?;
    Ok(LayerMeta {
        id: l.id.to_string(),
        key,
        extends: l.extends.clone(),
        tag: tag.map(|t| format!("{}@v{}", t.family, t.version)),
        tagged_at: tag.map(|t| t.tagged_at.to_rfc3339()),
    })
}

/// Take the `Ok` of a definition parse, or move its errors into `errors` (every error is reported).
fn collect<T>(parsed: Result<Vec<T>, Vec<CompileError>>, errors: &mut Vec<CompileError>) -> Vec<T> {
    match parsed {
        Ok(defs) => defs,
        Err(e) => {
            errors.extend(e);
            Vec::new()
        }
    }
}

/// Compile `selection` (+ `job`) over the members of the pinned tags only (B6-6, Ruling 17). The outer result is
/// the database side: 404 `tag_not_found` for a pin naming no tag (every pin is resolved first). The inner result
/// is the compile: `Err(errors)` lists every problem at once (route: 422 `compile_failed`, or 404
/// `profile_not_found` when [`compile::refusal`] says so) —
///
/// - 409 `template_not_tagged` (outer, B1's code) for a pin of a provisional v0: its members can still change;
/// - `invalid_selection` for a family pinned at two versions;
/// - `invalid_row` for unreadable `axis_rule` / `profile` member rows (both lists, merged);
/// - `unpinned_layer` (key = the family) for each layer the selection needs (base, fired profile layers, industry,
///   departments, job, job × dimension layers) whose family no pin names, and for the family of a profile only an
///   unpinned family defines. The needed layers are found over every layer, with the rules and profiles of the
///   pinned members plus the current `axis_rule` / `profile` rows of every unpinned family, so a rule an unpinned
///   family owns still names that family when it fires. A needed layer of a pinned family that its tag does not
///   contain (created after the tag) is `unpinned_layer` too, keyed by that layer — except a region sub-layer
///   (D111): the pinned tag predates it, so the pinned version compiles without it; likewise an unpinned base
///   department (D116) created after every pinned tag. The compile itself is then not
///   run: over an incomplete stack it would only add follow-on errors;
/// - `profile_not_found` when no family defines the selection's profile, or (from the compile) when only an
///   untagged row of a pinned family does;
/// - the compile's own errors.
///
/// The compile reads pinned content only: layers of the pinned families that existed when their tag was cut (a
/// layer created later is an untagged edit); rows = the tag members, ordered bytewise by (layer key, kind, key) as
/// the seed and the compile goldens are.
pub async fn compile_pinned(
    conn: &mut PgConnection,
    pins: &[TemplatePin],
    selection: &Selection,
    job: Option<&str>,
) -> ApiResult<Result<Compiled, Vec<CompileError>>> {
    compile_pinned_as(conn, pins, selection, job, compile::Mode::Job).await
}

/// `pins` with every family `selection` needs and no pin names, each at its newest tag (D116: a department's
/// `industry_dept/<i>/<d>` composes on `base_dept/<d>`, which a job naming only base, industry and departments does
/// not pin). The compile names those families `unpinned_layer`; a family pinned here may need another, so this
/// repeats until the compile names none it can pin. Any other compile error is left for the caller's compile.
pub async fn complete_pins(
    conn: &mut PgConnection,
    mut pins: Vec<TemplatePin>,
    selection: &Selection,
) -> ApiResult<Vec<TemplatePin>> {
    let latest = latest_tags(&mut *conn).await?;
    for _ in 0..4 {
        let Err(errors) = compile_pinned(&mut *conn, &pins, selection, None).await? else {
            break;
        };
        let missing: BTreeSet<&str> = errors
            .iter()
            .filter(|e| e.code == compile::code::UNPINNED_LAYER)
            .map(|e| e.key.as_str())
            .filter(|f| latest.contains_key(*f) && !pins.iter().any(|p| p.layer == *f))
            .collect();
        if missing.is_empty() {
            break;
        }
        pins.extend(missing.into_iter().map(|f| TemplatePin {
            layer: f.to_owned(),
            version: latest[f],
        }));
    }
    Ok(pins)
}

/// [`compile_pinned`] as the platform compile page previews it ([`compile::Mode::Preview`]): the base-only
/// selection compiles, labelled `base_only_preview`. No job-context caller uses it.
pub async fn compile_pinned_preview(
    conn: &mut PgConnection,
    pins: &[TemplatePin],
    selection: &Selection,
    job: Option<&str>,
) -> ApiResult<Result<Compiled, Vec<CompileError>>> {
    compile_pinned_as(conn, pins, selection, job, compile::Mode::Preview).await
}

async fn compile_pinned_as(
    conn: &mut PgConnection,
    pins: &[TemplatePin],
    selection: &Selection,
    job: Option<&str>,
    mode: compile::Mode,
) -> ApiResult<Result<Compiled, Vec<CompileError>>> {
    let mut errors: Vec<CompileError> = Vec::new();
    let mut pinned: BTreeMap<String, TemplateTag> = BTreeMap::new();
    for pin in pins {
        let tag = db_tags::tag(&mut *conn, &pin.layer, pin.version).await?;
        // a provisional v0's members can still be replaced (B8): a compile of it is not reproducible
        if tag.provisional {
            return Err(ApiError::Conflict(
                "template_not_tagged",
                format!(
                    "{}@v{} is a draft; only a tagged template compiles",
                    pin.layer, pin.version
                ),
            ));
        }
        if let Some(other) = pinned.get(&pin.layer) {
            if other.version != tag.version {
                errors.push(CompileError {
                    code: compile::code::INVALID_SELECTION.to_owned(),
                    key: pin.layer.clone(),
                    row_id: None,
                    message: format!(
                        "{} is pinned twice (v{} and v{}).",
                        pin.layer, other.version, tag.version
                    ),
                    layers: Vec::new(),
                });
            }
            continue;
        }
        pinned.insert(pin.layer.clone(), tag);
    }

    let all = layers::layers(&mut *conn).await?;
    let mut all_meta: Vec<LayerMeta> = Vec::with_capacity(all.len());
    let mut pinned_meta: Vec<LayerMeta> = Vec::new();
    let mut family_of: BTreeMap<String, String> = BTreeMap::new();
    // D116: a base department created after every pinned tag was cut is not part of the template — it compiles
    // without it, as it did before the layer existed (the next re-cut pins it, `stack_rule`).
    // `tagged_at`, not `created_at`: a seeded tag's `created_at` is when the seed ran (now() on a fresh database),
    // while its `tagged_at` and the seeded layers' `created_at` are the seed's own timestamps — the same clock
    let cut_by = pinned.values().map(|t| t.tagged_at).max();
    let later_base_depts: BTreeSet<String> = all
        .iter()
        .filter(|l| l.scope == "base_dept" && cut_by.is_some_and(|at| l.created_at > at))
        .map(|l| l.key.clone())
        .collect();
    let mut ordered: Vec<&Layer> = all.iter().collect();
    ordered.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    for l in ordered {
        family_of.insert(l.key.clone(), l.family.clone());
        let tag = pinned.get(&l.family);
        all_meta.push(layer_meta(l, None)?);
        if let Some(t) = tag
            && l.created_at <= t.created_at
        {
            pinned_meta.push(layer_meta(l, Some(t))?);
        }
    }

    let mut members: Vec<KeyedRow> = Vec::new();
    for t in pinned.values() {
        members.extend(db_tags::tag_members(&mut *conn, &t.family, t.version).await?);
    }
    // The compile applies rows in this order, and SQL returns them in the database collation: re-sort bytewise
    // (the seed's and the goldens' order) so a collation never changes a compile.
    members.sort_by(|a, b| {
        (a.layer_key.as_str(), a.kind.as_str(), a.key.as_str()).cmp(&(
            b.layer_key.as_str(),
            b.kind.as_str(),
            b.key.as_str(),
        ))
    });
    let rows = members
        .iter()
        .map(compile_row)
        .collect::<ApiResult<Vec<_>>>()?;
    let rules = collect(AxisRuleDef::from_rows(&rows), &mut errors);
    let profiles = collect(ProfileDef::from_rows(&rows), &mut errors);
    let departments = DepartmentRef::from_layers(&all_meta);

    // Ruling 17: the probe that finds the needed layers also knows the rule and profile definitions of every
    // UNPINNED family (its current rows), so a rule that fires from an unpinned family, or a profile only an
    // unpinned family defines, names that family `unpinned_layer` instead of silently dropping its layer. The
    // compile below still reads pinned content only.
    let pinned_families: Vec<String> = pinned.keys().cloned().collect();
    let mut unpinned_defs: Vec<KeyedRow> = sqlx::query_as::<_, KeyedRow>(
        "SELECT id, layer_id, layer_key, family, scope, kind, key, op, fields, fields_hash, rev, author, \
                created_at \
           FROM layer_head WHERE kind IN ('axis_rule', 'profile') AND NOT (family = ANY($1))",
    )
    .bind(&pinned_families)
    .fetch_all(&mut *conn)
    .await?;
    unpinned_defs.sort_by(|a, b| {
        (a.layer_key.as_str(), a.kind.as_str(), a.key.as_str()).cmp(&(
            b.layer_key.as_str(),
            b.kind.as_str(),
            b.key.as_str(),
        ))
    });
    let unpinned_rows = unpinned_defs
        .iter()
        .map(compile_row)
        .collect::<ApiResult<Vec<_>>>()?;
    // unpinned first, pinned last: a profile key both define resolves to the pinned one (the last wins)
    let mut probe_rules = collect(AxisRuleDef::from_rows(&unpinned_rows), &mut errors);
    probe_rules.extend(rules.iter().cloned());
    let mut probe_profiles = collect(ProfileDef::from_rows(&unpinned_rows), &mut errors);
    probe_profiles.extend(profiles.iter().cloned());
    let probe = CompileInput {
        layers: &all_meta,
        rows: &[],
        rules: &probe_rules,
        profiles: &probe_profiles,
        departments: &departments,
    };
    let needed = match compile::selected_layers(&probe, selection, job, mode) {
        Ok(needed) => needed,
        // no family, pinned or not, defines the profile
        Err(refused) => {
            errors.extend(refused);
            return Ok(Err(errors));
        }
    };
    // family → what needs it
    let mut unpinned: BTreeMap<String, String> = BTreeMap::new();
    // a needed layer of a pinned family that its tag does not contain (created after the tag was cut)
    let in_tags: BTreeSet<String> = pinned_meta.iter().map(LayerMeta::label).collect();
    let mut not_in_tag: Vec<CompileError> = Vec::new();
    for layer in &needed {
        let Some(family) = family_of.get(layer) else {
            continue;
        };
        match pinned.get(family) {
            None if later_base_depts.contains(layer) => {}
            None => {
                unpinned
                    .entry(family.clone())
                    .or_insert_with(|| layer.clone());
            }
            // D111: a region sub-layer created after its parent's pinned tag is not part of that tag — the pinned
            // version compiles without it (one tag pins parent and sub-layer together; the next tag adds it)
            Some(_)
                if !in_tags.contains(layer)
                    && LayerKey::parse_label(layer).is_ok_and(|k| k.is_region()) => {}
            Some(t) if !in_tags.contains(layer) => not_in_tag.push(CompileError {
                code: compile::code::UNPINNED_LAYER.to_owned(),
                key: layer.clone(),
                row_id: None,
                message: format!(
                    "The selection needs {layer}, which {family}@v{} does not contain (the layer was created \
                     after that tag); tag {family} again to include it.",
                    t.version
                ),
                layers: Vec::new(),
            }),
            Some(_) => {}
        }
    }
    if let Some(key) = selection.profile.as_deref().filter(|k| !k.is_empty())
        && !profiles.iter().any(|p| p.key == key)
        && let Some(def) = unpinned_defs
            .iter()
            .rev()
            .find(|r| r.kind == "profile" && r.key == key && r.op != "tombstone")
    {
        unpinned
            .entry(def.family.clone())
            .or_insert_with(|| format!("profile {key} ({})", def.layer_key));
    }
    for (family, what) in &unpinned {
        errors.push(CompileError {
            code: compile::code::UNPINNED_LAYER.to_owned(),
            key: family.clone(),
            row_id: None,
            message: format!("The selection needs {what}, but no pin names its family {family}."),
            layers: Vec::new(),
        });
    }
    let blocked = !unpinned.is_empty() || !not_in_tag.is_empty();
    errors.extend(not_in_tag);
    if blocked {
        return Ok(Err(errors));
    }

    let input = CompileInput {
        layers: &pinned_meta,
        rows: &rows,
        rules: &rules,
        profiles: &profiles,
        departments: &departments,
    };
    Ok(match compile::compile_as(&input, selection, job, mode) {
        Ok(compiled) if errors.is_empty() => Ok(compiled),
        Ok(_) => Err(errors),
        Err(e) => {
            errors.extend(e);
            Err(errors)
        }
    })
}

// --- head compile (B6b T9b, Revision 2026-10-22 R4) ----------------------------------------------------------------

/// The current platform model as the compile reads it (`layer_head`, not tag members): every layer bytewise by key,
/// every current row bytewise by (layer key, kind, key) — the order [`compile_pinned`] applies its members in — and
/// the rule / profile definitions parsed from those rows. `errors` holds the unreadable `axis_rule` / `profile` rows
/// (`invalid_row`); a compile over the model reports them with its own.
#[derive(Debug, Clone)]
pub struct HeadModel {
    pub layers: Vec<LayerMeta>,
    pub rows: Vec<compile::Row>,
    pub rules: Vec<AxisRuleDef>,
    pub profiles: Vec<ProfileDef>,
    pub departments: Vec<DepartmentRef>,
    pub errors: Vec<CompileError>,
}

impl HeadModel {
    /// The model as a compile input.
    pub fn input(&self) -> CompileInput<'_> {
        CompileInput {
            layers: &self.layers,
            rows: &self.rows,
            rules: &self.rules,
            profiles: &self.profiles,
            departments: &self.departments,
        }
    }
}

/// Read the [`HeadModel`] in the caller's transaction (`agent_platform` or `agent_app`: both read `layer_head`).
pub async fn head_model(conn: &mut PgConnection) -> ApiResult<HeadModel> {
    let mut all = layers::layers(&mut *conn).await?;
    all.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    let metas = all
        .iter()
        .map(|l| layer_meta(l, None))
        .collect::<ApiResult<Vec<_>>>()?;
    let mut current: Vec<KeyedRow> = sqlx::query_as::<_, KeyedRow>(
        "SELECT id, layer_id, layer_key, family, scope, kind, key, op, fields, fields_hash, rev, author, \
                created_at \
           FROM layer_head",
    )
    .fetch_all(&mut *conn)
    .await?;
    // bytewise, never the database collation (as compile_pinned)
    current.sort_by(|a, b| {
        (a.layer_key.as_str(), a.kind.as_str(), a.key.as_str()).cmp(&(
            b.layer_key.as_str(),
            b.kind.as_str(),
            b.key.as_str(),
        ))
    });
    let rows = current
        .iter()
        .map(compile_row)
        .collect::<ApiResult<Vec<_>>>()?;
    let mut errors: Vec<CompileError> = Vec::new();
    let rules = collect(AxisRuleDef::from_rows(&rows), &mut errors);
    let profiles = collect(ProfileDef::from_rows(&rows), &mut errors);
    let departments = DepartmentRef::from_layers(&metas);
    Ok(HeadModel {
        layers: metas,
        rows,
        rules,
        profiles,
        departments,
        errors,
    })
}

/// Compile `selection` (+ `job`) over the current rows ([`head_model`]) — what the platform compile page previews
/// (`compilePreview` without `as_of`). Same result shape as [`compile_pinned`]: the inner `Err` lists every problem
/// (the unreadable definition rows first, then the compile's; `profile_not_found` alone when the profile is unknown,
/// [`compile::refusal`]). `mode` is the caller's: the compile page passes [`compile::Mode::Preview`] (base-only
/// allowed, labelled `base_only_preview`). Never writes.
pub async fn compile_head(
    conn: &mut PgConnection,
    selection: &Selection,
    job: Option<&str>,
    mode: compile::Mode,
) -> ApiResult<Result<Compiled, Vec<CompileError>>> {
    let head = head_model(conn).await?;
    let mut errors = head.errors.clone();
    Ok(
        match compile::compile_as(&head.input(), selection, job, mode) {
            Ok(compiled) if errors.is_empty() => Ok(compiled),
            Ok(_) => Err(errors),
            Err(e) => {
                errors.extend(e);
                Err(errors)
            }
        },
    )
}

/// The contents of `family@v{version}` as UI tag items: its members, the head layer first, then the other layers
/// by key (bytewise), each by (kind, key). Items are keyed by (kind, key) across the family (plan §4), so when a
/// job × dimension layer (or a profile layer) carries the same (kind, key) as the head, the sublayer's row is the
/// item and the head's row is not in the contents. For B2 `release_conflict` this means a tenant refinement of
/// that (kind, key) is compared with the sublayer's `fields_hash`: an edit of only the head's row of such a key is
/// not reported as `changed`. D111: a region sub-layer's rows are their own items (`TagItem::region`), never the
/// parent's — a job that does not select that region still runs the parent's row, so an `nl` override neither masks
/// the parent item nor a change of it.
async fn contents_at(
    conn: &mut PgConnection,
    family: &str,
    version: i32,
) -> ApiResult<Vec<model_tags::TagItem>> {
    let mut members = db_tags::tag_members(conn, family, version).await?;
    members.sort_by(|a, b| {
        (
            a.layer_key != family,
            a.layer_key.as_str(),
            a.kind.as_str(),
            a.key.as_str(),
        )
            .cmp(&(
                b.layer_key != family,
                b.layer_key.as_str(),
                b.kind.as_str(),
                b.key.as_str(),
            ))
    });
    let rows = members
        .iter()
        .map(compile_row)
        .collect::<ApiResult<Vec<_>>>()?;
    let region_of_layer: BTreeMap<String, String> = members
        .iter()
        .filter_map(|m| {
            let region = LayerKey::parse_label(&m.layer_key).ok()?.region?;
            Some((m.layer_id.to_string(), region))
        })
        .collect();
    Ok(model_tags::contents_by_region(&rows, |r| {
        region_of_layer.get(&r.layer_id).cloned()
    }))
}

/// What changed in `family` from tag `from` to tag `to`, by (kind, key) over every layer of the family
/// (`agent_model::tags::diff`: `changed` by `fields_hash`, Ruling 19). 404 `tag_not_found` for either tag.
pub async fn tag_diff(
    conn: &mut PgConnection,
    family: &str,
    from: i32,
    to: i32,
) -> ApiResult<Vec<ItemChange>> {
    db_tags::tag(&mut *conn, family, from).await?;
    db_tags::tag(&mut *conn, family, to).await?;
    let before = contents_at(&mut *conn, family, from).await?;
    let after = contents_at(&mut *conn, family, to).await?;
    Ok(model_tags::diff(&before, &after))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_suffix_is_added_once_and_fits() {
        assert_eq!(
            with_previous_suffix("Customer care agent"),
            "Customer care agent (previous tag)"
        );
        assert_eq!(
            with_previous_suffix("Customer care agent (previous tag)"),
            "Customer care agent (previous tag)"
        );
        let long = "x".repeat(200);
        let l = with_previous_suffix(&long);
        assert_eq!(l.chars().count(), MAX_LABEL_CHARS);
        assert!(l.ends_with(PREVIOUS_TAG_SUFFIX));
    }
}
