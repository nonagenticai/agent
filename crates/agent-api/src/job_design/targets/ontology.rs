//! J11/J12 targets: `entity` and its three projections, `relation`, and `space_required` (a projection of B2's
//! `overlay` place `{object: "context_space", key}`, plan R4).
//!
//! An `entity` place holds one of two values:
//! - a **tenant** entity (`ontology_entity`): `{name, label, scope, industry, department, synonyms, attributes,
//!   hidden, object_key}`; `proj` = `[label, synonyms, attributes, hidden]`;
//! - a **base** entity's refinement (`ontology_entity_refinement`): `{hidden, synonyms_added, base_hash}`; `proj` =
//!   `[hidden, synonyms_added]`. Attributes the tenant adds to a base entity are B2 `overlay` places
//!   (`ontology_attribute` extensions, key `Entity.attr`), never here.
//!
//! A value with a `label` is a tenant entity. An unrefined base entity has no place (`None`): the projections read
//! `None` there too, and writing one back to `None`/`false`/`[]` drops the refinement row (plan: "row deleted when
//! both fields are back to the base").
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{SET_BY, decode, field, internal, job_of, js_string, list, place_ref, present};
use crate::design::{self, DesignTarget, DraftRef, JobRef, Place, Scope, Snap};
use crate::error::ApiResult;
use crate::release::overlay::{CONTEXT_SPACE, item_hash};

/// `base_hash` of a refinement row that `set` has to create while no stored design names the base it refines (a
/// restore with no snapshot of the entity). The T4 detector treats it as "no hash recorded".
pub const UNKNOWN_BASE_HASH: &str = "unknown";

/// The ledger kind an entity-field edit is recorded under (plan "collapse rule", fixture `entityKind`): `entity`
/// when the draft itself created the entity (the job's ledger has a row at `entity:<name>`), else `field_kind`
/// (`entity_hidden`, `entity_synonyms` or `entity_attributes`).
pub async fn entity_write_kind(
    c: &mut PgConnection,
    job: &JobRef,
    name: &str,
    field_kind: &'static str,
) -> ApiResult<&'static str> {
    let key = format!("entity:{name}");
    Ok(match design::ledger::find(c, job, &key).await? {
        Some(_) => "entity",
        None => field_kind,
    })
}

// ---- entity ---------------------------------------------------------------------------------------------------------

/// A tenant entity as its place stores it.
#[derive(Debug, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct TenantEntity {
    name: String,
    label: String,
    scope: String,
    #[serde(default)]
    industry: Option<String>,
    #[serde(default)]
    department: Option<String>,
    #[serde(default)]
    synonyms: Vec<String>,
    #[serde(default = "empty_array")]
    attributes: Value,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    object_key: Option<String>,
}

fn empty_array() -> Value {
    json!([])
}

impl TenantEntity {
    fn value(self) -> Value {
        json!({
            "name": self.name,
            "label": self.label,
            "scope": self.scope,
            "industry": self.industry,
            "department": self.department,
            "synonyms": self.synonyms,
            "attributes": self.attributes,
            "hidden": self.hidden,
            "object_key": self.object_key,
        })
    }
}

/// A base entity's refinement as its place stores it.
#[derive(Debug, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct BaseRefinement {
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    synonyms_added: Vec<String>,
    base_hash: String,
}

impl BaseRefinement {
    fn value(self) -> Value {
        json!({
            "hidden": self.hidden,
            "synonyms_added": self.synonyms_added,
            "base_hash": self.base_hash,
        })
    }
}

fn is_tenant(v: &Value) -> bool {
    v.get("label").is_some()
}

async fn tenant_entity(
    c: &mut PgConnection,
    job: Uuid,
    name: &str,
) -> ApiResult<Option<TenantEntity>> {
    Ok(sqlx::query_as::<_, TenantEntity>(
        "SELECT name, label, scope, industry, department, synonyms, attributes, hidden, object_key \
           FROM ontology_entity WHERE job_id = $1 AND name = $2",
    )
    .bind(job)
    .bind(name)
    .fetch_optional(&mut *c)
    .await?)
}

async fn refinement(
    c: &mut PgConnection,
    job: Uuid,
    name: &str,
) -> ApiResult<Option<BaseRefinement>> {
    Ok(sqlx::query_as::<_, BaseRefinement>(
        "SELECT hidden, synonyms_added, base_hash FROM ontology_entity_refinement \
          WHERE job_id = $1 AND name = $2",
    )
    .bind(job)
    .bind(name)
    .fetch_optional(&mut *c)
    .await?)
}

/// The draft value of the `entity` place `name`.
async fn entity_value(c: &mut PgConnection, job: Uuid, name: &str) -> ApiResult<Snap> {
    if let Some(t) = tenant_entity(&mut *c, job, name).await? {
        return Ok(Some(t.value()));
    }
    Ok(refinement(&mut *c, job, name)
        .await?
        .map(BaseRefinement::value))
}

/// The `base_hash` a stored design last recorded for the base entity `name` (the current release first, then any
/// release, then the baseline); [`UNKNOWN_BASE_HASH`] when none did.
async fn stored_base_hash(c: &mut PgConnection, job: Uuid, name: &str) -> ApiResult<String> {
    let key = format!("entity:{name}");
    let released: Option<Option<String>> = sqlx::query_scalar(
        "SELECT s.snap -> 'v' ->> 'base_hash' FROM release_snapshot s JOIN jobs j ON j.id = s.job_id \
          WHERE s.job_id = $1 AND s.key = $2 AND s.snap -> 'v' ->> 'base_hash' IS NOT NULL \
          ORDER BY (s.release_id = j.current_release_id) DESC NULLS LAST, s.release_id DESC LIMIT 1",
    )
    .bind(job)
    .bind(&key)
    .fetch_optional(&mut *c)
    .await?;
    if let Some(Some(h)) = released {
        return Ok(h);
    }
    let baseline: Option<Option<String>> = sqlx::query_scalar(
        "SELECT snap -> 'v' ->> 'base_hash' FROM release_baseline WHERE job_id = $1 AND key = $2",
    )
    .bind(job)
    .bind(&key)
    .fetch_optional(&mut *c)
    .await?;
    Ok(baseline
        .flatten()
        .unwrap_or_else(|| UNKNOWN_BASE_HASH.to_owned()))
}

/// Set one or both fields of a base entity's refinement (a projection's restore). The row is created when needed
/// and deleted when both fields are back to the base (`hidden = false`, no synonyms added).
async fn refine_base(
    c: &mut PgConnection,
    job: Uuid,
    name: &str,
    hidden: Option<bool>,
    synonyms_added: Option<Vec<String>>,
) -> ApiResult<()> {
    let cur = refinement(&mut *c, job, name).await?;
    let (was_hidden, was_synonyms) = cur
        .as_ref()
        .map(|r| (r.hidden, r.synonyms_added.clone()))
        .unwrap_or((false, Vec::new()));
    let hidden = hidden.unwrap_or(was_hidden);
    let synonyms_added = synonyms_added.unwrap_or(was_synonyms);
    if !hidden && synonyms_added.is_empty() {
        sqlx::query("DELETE FROM ontology_entity_refinement WHERE job_id = $1 AND name = $2")
            .bind(job)
            .bind(name)
            .execute(&mut *c)
            .await?;
        return Ok(());
    }
    if cur.is_some() {
        sqlx::query(
            "UPDATE ontology_entity_refinement \
                SET hidden = $3, synonyms_added = $4, updated_at = clock_timestamp() \
              WHERE job_id = $1 AND name = $2",
        )
        .bind(job)
        .bind(name)
        .bind(hidden)
        .bind(&synonyms_added)
        .execute(&mut *c)
        .await?;
        return Ok(());
    }
    let base_hash = stored_base_hash(&mut *c, job, name).await?;
    sqlx::query(
        "INSERT INTO ontology_entity_refinement (job_id, name, hidden, synonyms_added, base_hash, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(job)
    .bind(name)
    .bind(hidden)
    .bind(&synonyms_added)
    .bind(base_hash)
    .bind(SET_BY)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Update one column of a tenant entity; `false` when the job has no tenant entity `name`.
async fn update_tenant(
    c: &mut PgConnection,
    job: Uuid,
    name: &str,
    column: TenantColumn,
    v: &Value,
) -> ApiResult<bool> {
    let n = match column {
        TenantColumn::Hidden => {
            let h = v.as_bool().unwrap_or(false);
            sqlx::query(
                "UPDATE ontology_entity SET hidden = $3, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND name = $2",
            )
            .bind(job)
            .bind(name)
            .bind(h)
            .execute(&mut *c)
            .await?
        }
        TenantColumn::Synonyms => {
            let xs: Vec<String> = decode("entity_synonyms", v)?;
            sqlx::query(
                "UPDATE ontology_entity SET synonyms = $3, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND name = $2",
            )
            .bind(job)
            .bind(name)
            .bind(xs)
            .execute(&mut *c)
            .await?
        }
        TenantColumn::Attributes => {
            if !v.is_array() {
                return Err(internal(format!(
                    "entity_attributes value is not an array: {v}"
                )));
            }
            sqlx::query(
                "UPDATE ontology_entity SET attributes = $3, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND name = $2",
            )
            .bind(job)
            .bind(name)
            .bind(v)
            .execute(&mut *c)
            .await?
        }
    };
    Ok(n.rows_affected() > 0)
}

#[derive(Clone, Copy)]
enum TenantColumn {
    Hidden,
    Synonyms,
    Attributes,
}

/// One entity of the job's ontology: a tenant entity, or the refinement of a base entity.
pub struct Entity;

impl DesignTarget for Entity {
    fn kind(&self) -> &'static str {
        "entity"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("entity:{}", field(r, "name"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move { entity_value(c, job_of(at)?, field(r, "name")).await })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let name = field(r, "name");
            let Some(v) = present(v) else {
                sqlx::query("DELETE FROM ontology_entity WHERE job_id = $1 AND name = $2")
                    .bind(job)
                    .bind(name)
                    .execute(&mut *c)
                    .await?;
                sqlx::query(
                    "DELETE FROM ontology_entity_refinement WHERE job_id = $1 AND name = $2",
                )
                .bind(job)
                .bind(name)
                .execute(&mut *c)
                .await?;
                return Ok(());
            };
            if is_tenant(v) {
                let e: TenantEntity = decode("entity", v)?;
                sqlx::query(
                    "DELETE FROM ontology_entity_refinement WHERE job_id = $1 AND name = $2",
                )
                .bind(job)
                .bind(name)
                .execute(&mut *c)
                .await?;
                sqlx::query(
                    "INSERT INTO ontology_entity \
                            (job_id, name, label, scope, industry, department, synonyms, attributes, hidden, \
                             object_key, created_by) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
                     ON CONFLICT (tenant_id, job_id, name) DO UPDATE \
                        SET label = EXCLUDED.label, scope = EXCLUDED.scope, industry = EXCLUDED.industry, \
                            department = EXCLUDED.department, synonyms = EXCLUDED.synonyms, \
                            attributes = EXCLUDED.attributes, hidden = EXCLUDED.hidden, \
                            object_key = EXCLUDED.object_key, updated_at = clock_timestamp()",
                )
                .bind(job)
                .bind(name)
                .bind(&e.label)
                .bind(&e.scope)
                .bind(&e.industry)
                .bind(&e.department)
                .bind(&e.synonyms)
                .bind(&e.attributes)
                .bind(e.hidden)
                .bind(&e.object_key)
                .bind(SET_BY)
                .execute(&mut *c)
                .await?;
            } else {
                let b: BaseRefinement = decode("entity", v)?;
                sqlx::query("DELETE FROM ontology_entity WHERE job_id = $1 AND name = $2")
                    .bind(job)
                    .bind(name)
                    .execute(&mut *c)
                    .await?;
                sqlx::query(
                    "INSERT INTO ontology_entity_refinement \
                            (job_id, name, hidden, synonyms_added, base_hash, updated_by) \
                     VALUES ($1, $2, $3, $4, $5, $6) \
                     ON CONFLICT (tenant_id, job_id, name) DO UPDATE \
                        SET hidden = EXCLUDED.hidden, synonyms_added = EXCLUDED.synonyms_added, \
                            base_hash = EXCLUDED.base_hash, updated_at = clock_timestamp()",
                )
                .bind(job)
                .bind(name)
                .bind(b.hidden)
                .bind(&b.synonyms_added)
                .bind(&b.base_hash)
                .bind(SET_BY)
                .execute(&mut *c)
                .await?;
            }
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM ontology_entity WHERE job_id = $1 \
                 UNION SELECT name FROM ontology_entity_refinement WHERE job_id = $1 \
                 ORDER BY 1",
            )
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                names
                    .iter()
                    .map(|n| place_ref(&[("name", n.as_str())]))
                    .collect(),
            ))
        })
    }
    /// Tenant entity `[label, synonyms, attributes, hidden]` (fixture); base refinement `[hidden, synonyms_added]`.
    fn proj(&self, v: &Snap) -> Value {
        let Some(e) = present(v) else {
            return Value::Null;
        };
        let pick = |f: &str| e.get(f).cloned().unwrap_or(Value::Null);
        if is_tenant(e) {
            json!([
                pick("label"),
                pick("synonyms"),
                pick("attributes"),
                pick("hidden")
            ])
        } else {
            json!([
                e.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                e.get("synonyms_added")
                    .cloned()
                    .unwrap_or_else(|| json!([]))
            ])
        }
    }
    fn text(&self, v: &Snap, r: &DraftRef) -> Option<String> {
        let Some(e) = present(v) else {
            return Some("not in the ontology".to_string());
        };
        if is_tenant(e) {
            let label = e.get("label").map(js_string).unwrap_or_default();
            let names: Vec<String> = e
                .get("attributes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| a.get("name").map(js_string))
                .collect();
            let attrs = if names.is_empty() {
                "no attributes".to_string()
            } else {
                names.join(", ")
            };
            return Some(format!("{label} ({attrs})"));
        }
        let mut parts = Vec::new();
        if e.get("hidden").and_then(Value::as_bool).unwrap_or(false) {
            parts.push("hidden".to_string());
        }
        let added = e.get("synonyms_added");
        if added
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            parts.push(format!("also called {}", list(added)));
        }
        Some(if parts.is_empty() {
            format!("{} as the template has it", field(r, "name"))
        } else {
            parts.join(" · ")
        })
    }
}

/// The `entity` place of an entity-field ref.
fn entity_parent(r: &DraftRef) -> Option<(&'static str, DraftRef)> {
    Some(("entity", place_ref(&[("name", field(r, "name"))])))
}

/// The field of an entity value a projection reads: `tenant` for a tenant entity, `base` for a base refinement
/// (`None` = the projection has nothing there).
fn entity_field(parent: &Snap, tenant: &str, base: Option<&str>) -> Snap {
    let e = present(parent)?;
    if is_tenant(e) {
        return e.get(tenant).cloned();
    }
    base.and_then(|b| e.get(b).cloned())
}

/// Projection `entity_hidden` (`hide:<name>`): whether the entity is hidden.
pub struct EntityHidden;

impl DesignTarget for EntityHidden {
    fn kind(&self) -> &'static str {
        "entity_hidden"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("hide:{}", field(r, "name"))
    }
    fn is_projection(&self) -> bool {
        true
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let parent = entity_value(c, job_of(at)?, field(r, "name")).await?;
            Ok(self.project(&parent, r))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let name = field(r, "name");
            let hidden = present(v).and_then(Value::as_bool).unwrap_or(false);
            if tenant_entity(&mut *c, job, name).await?.is_some() {
                // fixture `field.set`: a tenant entity's row is never removed through its field
                if present(v).is_some() {
                    update_tenant(c, job, name, TenantColumn::Hidden, &json!(hidden)).await?;
                }
                return Ok(());
            }
            refine_base(c, job, name, Some(hidden), None).await
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(None) })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(
            if present(v).and_then(Value::as_bool).unwrap_or(false) {
                "hidden"
            } else {
                "shown"
            }
            .to_string(),
        )
    }
    fn parent(&self, r: &DraftRef) -> Option<(&'static str, DraftRef)> {
        entity_parent(r)
    }
    fn project(&self, parent: &Snap, _r: &DraftRef) -> Snap {
        entity_field(parent, "hidden", Some("hidden"))
    }
}

/// Projection `entity_synonyms` (`synonyms:<name>`): a tenant entity's synonyms, or the synonyms a base entity's
/// refinement adds.
pub struct EntitySynonyms;

impl DesignTarget for EntitySynonyms {
    fn kind(&self) -> &'static str {
        "entity_synonyms"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("synonyms:{}", field(r, "name"))
    }
    fn is_projection(&self) -> bool {
        true
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let parent = entity_value(c, job_of(at)?, field(r, "name")).await?;
            Ok(self.project(&parent, r))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let name = field(r, "name");
            let xs: Vec<String> = match present(v) {
                Some(x) => decode("entity_synonyms", x)?,
                None => Vec::new(),
            };
            if tenant_entity(&mut *c, job, name).await?.is_some() {
                if present(v).is_some() {
                    update_tenant(c, job, name, TenantColumn::Synonyms, &json!(xs)).await?;
                }
                return Ok(());
            }
            refine_base(c, job, name, None, Some(xs)).await
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(None) })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(list(present(v)))
    }
    fn parent(&self, r: &DraftRef) -> Option<(&'static str, DraftRef)> {
        entity_parent(r)
    }
    fn project(&self, parent: &Snap, _r: &DraftRef) -> Snap {
        entity_field(parent, "synonyms", Some("synonyms_added"))
    }
}

/// Projection `entity_attributes` (`attributes:<name>`): a TENANT entity's attributes. A base entity has none here
/// (attributes added to it are B2 `overlay` places).
pub struct EntityAttributes;

impl DesignTarget for EntityAttributes {
    fn kind(&self) -> &'static str {
        "entity_attributes"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("attributes:{}", field(r, "name"))
    }
    fn is_projection(&self) -> bool {
        true
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let parent = entity_value(c, job_of(at)?, field(r, "name")).await?;
            Ok(self.project(&parent, r))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            // fixture `field.set`: only an existing tenant entity's attributes change; nothing else is written
            if let Some(x) = present(v) {
                update_tenant(
                    c,
                    job_of(at)?,
                    field(r, "name"),
                    TenantColumn::Attributes,
                    x,
                )
                .await?;
            }
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(None) })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let names: Vec<Value> = present(v)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| a.get("name").cloned().unwrap_or(Value::Null))
            .collect();
        Some(list(Some(&Value::Array(names))))
    }
    fn parent(&self, r: &DraftRef) -> Option<(&'static str, DraftRef)> {
        entity_parent(r)
    }
    fn project(&self, parent: &Snap, _r: &DraftRef) -> Snap {
        entity_field(parent, "attributes", None)
    }
}

// ---- relation -------------------------------------------------------------------------------------------------------

/// A tenant relation `subject predicate object` (`ontology_relation`); the value is `{cardinality}`.
pub struct Relation;

impl DesignTarget for Relation {
    fn kind(&self) -> &'static str {
        "relation"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!(
            "relation:{}:{}:{}",
            field(r, "subject"),
            field(r, "predicate"),
            field(r, "object")
        )
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let cardinality: Option<String> = sqlx::query_scalar(
                "SELECT cardinality FROM ontology_relation \
                  WHERE job_id = $1 AND subject = $2 AND predicate = $3 AND object = $4",
            )
            .bind(job_of(at)?)
            .bind(field(r, "subject"))
            .bind(field(r, "predicate"))
            .bind(field(r, "object"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(cardinality.map(|k| json!({"cardinality": k})))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let (s, p, o) = (
                field(r, "subject"),
                field(r, "predicate"),
                field(r, "object"),
            );
            let Some(v) = present(v) else {
                sqlx::query(
                    "DELETE FROM ontology_relation \
                      WHERE job_id = $1 AND subject = $2 AND predicate = $3 AND object = $4",
                )
                .bind(job)
                .bind(s)
                .bind(p)
                .bind(o)
                .execute(&mut *c)
                .await?;
                return Ok(());
            };
            let cardinality = v
                .get("cardinality")
                .and_then(Value::as_str)
                .ok_or_else(|| internal(format!("relation value has no cardinality: {v}")))?;
            sqlx::query(
                "INSERT INTO ontology_relation (job_id, subject, predicate, object, cardinality, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (tenant_id, job_id, subject, predicate, object) DO UPDATE \
                    SET cardinality = EXCLUDED.cardinality",
            )
            .bind(job)
            .bind(s)
            .bind(p)
            .bind(o)
            .bind(cardinality)
            .bind(SET_BY)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT subject, predicate, object FROM ontology_relation WHERE job_id = $1 \
                  ORDER BY subject, predicate, object",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                rows.iter()
                    .map(|(s, p, o)| {
                        place_ref(&[
                            ("subject", s.as_str()),
                            ("predicate", p.as_str()),
                            ("object", o.as_str()),
                        ])
                    })
                    .collect(),
            ))
        })
    }
    fn text(&self, v: &Snap, r: &DraftRef) -> Option<String> {
        Some(match present(v) {
            Some(_) => format!(
                "{} {} {}",
                field(r, "subject"),
                field(r, "predicate"),
                field(r, "object")
            ),
            None => "no relation".to_string(),
        })
    }
}

// ---- space_required -------------------------------------------------------------------------------------------------

/// Projection `space_required` (`space:<key>`, plan R4): the `required` slots of B2's `overlay` place
/// `{object: "context_space", key}`. `None` = the template's (no tenant row, or the space is hidden).
pub struct SpaceRequired;

/// `{required}` of an overlay value (`{kind, item, …}`); `None` for a tombstone or an item without `required`.
fn required_of(overlay: &Value) -> Snap {
    if overlay.get("kind").and_then(Value::as_str) == Some("tombstone") {
        return None;
    }
    let required = overlay.get("item")?.get("required")?;
    Some(json!({"required": required}))
}

impl DesignTarget for SpaceRequired {
    fn kind(&self) -> &'static str {
        "space_required"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("space:{}", field(r, "key"))
    }
    fn is_projection(&self) -> bool {
        true
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<(String, Option<Value>)> = sqlx::query_as(
                "SELECT kind, item FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(job_of(at)?)
            .bind(CONTEXT_SPACE)
            .bind(field(r, "key"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.and_then(|(kind, item)| required_of(&json!({"kind": kind, "item": item}))))
        })
    }
    /// Restore: `Some({required})` sets the slots of the tenant's (non-hidden) row, keeping its kind and
    /// `base_hash`; `None` (the live design has no tenant row there) removes the tenant's row. Without a row to
    /// update nothing is written (fixture `field.set`: the row is never created through its field).
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let key = field(r, "key");
            let row: Option<(String, Option<Value>)> = sqlx::query_as(
                "SELECT kind, item FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(job)
            .bind(CONTEXT_SPACE)
            .bind(key)
            .fetch_optional(&mut *c)
            .await?;
            let Some((kind, item)) = row else {
                return Ok(());
            };
            if kind == "tombstone" {
                return Ok(());
            }
            let Some(v) = present(v) else {
                sqlx::query(
                    "DELETE FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
                )
                .bind(job)
                .bind(CONTEXT_SPACE)
                .bind(key)
                .execute(&mut *c)
                .await?;
                return Ok(());
            };
            let required = v
                .get("required")
                .cloned()
                .ok_or_else(|| internal(format!("space_required value has no required: {v}")))?;
            let mut item = item
                .and_then(|i| i.as_object().cloned())
                .unwrap_or_default();
            item.insert("key".to_owned(), Value::String(key.to_owned()));
            item.insert("required".to_owned(), required);
            let content_hash = item_hash(&item)?;
            sqlx::query(
                "UPDATE job_overlay_row SET item = $4, content_hash = $5, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(job)
            .bind(CONTEXT_SPACE)
            .bind(key)
            .bind(Value::Object(item))
            .bind(content_hash)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(None) })
    }
    /// Fixture: `v?.required ?? null`.
    fn proj(&self, v: &Snap) -> Value {
        present(v)
            .and_then(|x| x.get("required"))
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(list(present(v).and_then(|x| x.get("required"))))
    }
    fn parent(&self, r: &DraftRef) -> Option<(&'static str, DraftRef)> {
        Some((
            "overlay",
            place_ref(&[("object", CONTEXT_SPACE), ("key", field(r, "key"))]),
        ))
    }
    fn project(&self, parent: &Snap, _r: &DraftRef) -> Snap {
        required_of(present(parent)?)
    }
}
