//! J11 ontology (B5 T6, plan "Route table"): the job's entities and relations, base (the template at the draft's
//! pins, never copied, plan R1) and the tenant's. Reads are the DRAFT design (what the designer edits); every write is
//! a D54 draft through `design::track` (one ledger row + one audit row, `draft_rev` bumped), page J11, area
//! `ontology`. A port of the fixture `areas/ontology.ts` J11 methods (codes, messages, labels).
//!
//! | Method | Route | Role | Ledger kind |
//! |---|---|---|---|
//! | ontologyEntities | `GET /api/jobs/{job}/ontology/entities?include_hidden=true` | member | — |
//! | ontologyRelations | `GET /api/jobs/{job}/ontology/relations` | member | — |
//! | createOntologyEntity | `POST /api/jobs/{job}/ontology/entities` | job_designer | `entity` |
//! | addOntologySynonym | `POST …/entities/{name}/synonyms` `{term}` | job_designer | `entity_synonyms` \| `entity` |
//! | addOntologyAttribute | `POST …/entities/{name}/attributes` | job_designer | `entity_attributes` \| `entity` (tenant entity); `overlay` (base entity) |
//! | setOntologyEntityHidden | `PUT …/entities/{name}/hidden` `{hidden}` | job_designer | `entity_hidden` \| `entity` |
//! | createOntologyRelation | `POST /api/jobs/{job}/ontology/relations` | job_designer | `relation` |
//! | deleteOntologyRelation | `DELETE /api/jobs/{job}/ontology/relations` (JSON body) | job_designer | `relation` |
//!
//! - Collapse rule (fixture `entityKind`): an edit of an entity the draft itself created is recorded under `entity`.
//! - A base entity is refined in `ontology_entity_refinement` (hidden, synonyms added; the row goes when both are back
//!   to the base); an attribute added to a base entity is B2's `ontology_attribute` extension (plan R2,
//!   [`overlay::save_attribute_extension`]), never a B5 row.
//! - Lists are not paged further (`next` is always null): an ontology is a few dozen entities.
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{body, job_ref};
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::job_design::base::{self, Bases};
use crate::job_design::ontology::{
    self, Rows, is_attribute_name, is_entity_name, is_predicate, relation_key,
};
use crate::job_design::targets::entity_write_kind;
use crate::job_design::types::{
    AttributeInput, EntityInput, OntologyEntity, OntologyRelation, OntologyScope, Paged,
    RelationInput,
};
use crate::release::overlay::{self, AttributeExtension};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/ontology/entities",
            get(ontology_entities).post(create_ontology_entity),
        )
        .route(
            "/api/jobs/{job}/ontology/entities/{name}/synonyms",
            post(add_ontology_synonym),
        )
        .route(
            "/api/jobs/{job}/ontology/entities/{name}/attributes",
            post(add_ontology_attribute),
        )
        .route(
            "/api/jobs/{job}/ontology/entities/{name}/hidden",
            put(set_ontology_entity_hidden),
        )
        .route(
            "/api/jobs/{job}/ontology/relations",
            get(ontology_relations)
                .post(create_ontology_relation)
                .delete(delete_ontology_relation),
        )
}

const AREA: &str = "ontology";
const PAGE: &str = "J11";
/// `ontology_entity.label` CHECK (1..120 characters).
const MAX_LABEL_CHARS: usize = 120;

#[derive(Debug, Default, Deserialize)]
struct EntitiesQuery {
    #[serde(default)]
    include_hidden: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SynonymInput {
    term: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HiddenInput {
    hidden: bool,
}

fn place(pairs: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), Value::String((*v).to_owned()));
    }
    m
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn entity_not_found(name: &str) -> ApiError {
    ApiError::Missing("entity_not_found", format!("No entity {name}."))
}

/// The DRAFT bases and the job's working ontology rows.
async fn draft(c: &mut PgConnection, st: &AppState, jr: &JobRef) -> ApiResult<(Arc<Bases>, Rows)> {
    let bases = base::at(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        jr,
        &DesignOf::Draft,
    )
    .await?;
    let rows = ontology::draft_rows(&mut *c, jr.id).await?;
    Ok((bases, rows))
}

/// Entity `name` as the draft has it now (after a write).
async fn entity_now(
    c: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    name: &str,
) -> ApiResult<OntologyEntity> {
    let (bases, rows) = draft(&mut *c, st, &job_ref(j)).await?;
    ontology::entity(&bases, &rows, j.t.tenant, &j.job_key, name)
        .ok_or_else(|| internal(format!("entity {name} is gone after its own write")))
}

/// The `base_hash` of base entity `name` for a new refinement row.
fn entity_base_hash(bases: &Bases, name: &str) -> ApiResult<String> {
    let item = bases
        .entity(name)
        .ok_or_else(|| internal(format!("{name} is not a base entity")))?;
    base::entity_hash(item)
}

// ---- reads --------------------------------------------------------------------------------------------------------

async fn ontology_entities(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<EntitiesQuery>,
) -> ApiResult<Json<Paged<OntologyEntity>>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    tx.commit().await?;
    let items = ontology::entities(&bases, &rows, j.t.tenant, &j.job_key)
        .into_iter()
        .filter(|e| q.include_hidden || !e.hidden)
        .collect();
    Ok(Json(Paged { items, next: None }))
}

async fn ontology_relations(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Paged<OntologyRelation>>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    tx.commit().await?;
    let items = ontology::relations(&bases, &rows, j.t.tenant, &j.job_key);
    Ok(Json(Paged { items, next: None }))
}

// ---- entity writes ----------------------------------------------------------------------------------------------

async fn create_ontology_entity(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<OntologyEntity>)> {
    let i: EntityInput = body(v)?;
    if !is_entity_name(&i.name) {
        return Err(ApiError::Unprocessable(
            "invalid_name",
            "An entity name is PascalCase letters and digits, e.g. GiftCard.".into(),
        ));
    }
    let label = i.label.trim().to_owned();
    if label.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_label",
            "Label is required.".into(),
        ));
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_label",
            format!("Label is at most {MAX_LABEL_CHARS} characters."),
        ));
    }
    let mut attributes = Vec::with_capacity(i.attributes.len());
    for a in &i.attributes {
        if !is_attribute_name(&a.name) {
            return Err(ApiError::Unprocessable(
                "invalid_attribute",
                "An attribute name is snake_case, e.g. gift_message.".into(),
            ));
        }
        if attributes
            .iter()
            .any(|x: &Value| x["name"] == json!(a.name))
        {
            return Err(ApiError::Unprocessable(
                "invalid_attribute",
                format!("{} is listed twice.", a.name),
            ));
        }
        attributes.push(json!({
            "name": a.name,
            "data_type": a.data_type,
            "pii": a.pii,
            "synonyms": a.synonyms,
        }));
    }
    let synonyms: Vec<String> = i
        .synonyms
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let scope = i.scope;
    let industry = match scope {
        OntologyScope::Generic => None,
        _ => i.industry.clone(),
    };
    let department = match scope {
        OntologyScope::IndustryDept => i.department.clone(),
        _ => None,
    };
    let scope_text = serde_json::to_value(scope)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| internal("an ontology scope does not serialise".into()))?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    if bases.entity(&i.name).is_some() || rows.tenant_entity(&i.name).is_some() {
        return Err(ApiError::Conflict(
            "entity_exists",
            format!("{} already exists in this job.", i.name),
        ));
    }
    let name = i.name.clone();
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let object_key = i.object_key.clone();
    let attributes = Value::Array(attributes);
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "entity",
        r#ref: place(&[("name", name.as_str())]),
        label: Label::Now(format!("Entity {name} added")),
    };
    let row_name = name.clone();
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO ontology_entity \
                        (job_id, name, label, scope, industry, department, synonyms, attributes, object_key, \
                         created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(job)
            .bind(&row_name)
            .bind(&label)
            .bind(&scope_text)
            .bind(&industry)
            .bind(&department)
            .bind(&synonyms)
            .bind(&attributes)
            .bind(&object_key)
            .bind(&by)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let out = entity_now(&mut tx, &st, &j, &name).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn add_ontology_synonym(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, name)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<OntologyEntity>> {
    let i: SynonymInput = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let e = ontology::entity(&bases, &rows, j.t.tenant, &j.job_key, &name)
        .ok_or_else(|| entity_not_found(&name))?;
    let term = i.term.trim().to_owned();
    if term.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_synonym",
            "A synonym cannot be empty.".into(),
        ));
    }
    let lower = term.to_lowercase();
    if e.synonyms.iter().any(|s| s.to_lowercase() == lower) {
        return Err(ApiError::Conflict(
            "synonym_exists",
            format!("“{term}” is already a synonym of {}.", e.label),
        ));
    }
    let kind = entity_write_kind(&mut tx, &jr, &name, "entity_synonyms").await?;
    let tenant_entity = rows.tenant_entity(&name).is_some();
    let hash = if tenant_entity {
        None
    } else {
        Some(entity_base_hash(&bases, &name)?)
    };
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let row_name = name.clone();
    let row_term = term.clone();
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind,
        r#ref: place(&[("name", name.as_str())]),
        label: Label::Now(format!("{name}: synonym “{term}” added")),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            match hash {
                None => {
                    sqlx::query(
                        "UPDATE ontology_entity \
                            SET synonyms = array_append(synonyms, $3), updated_at = clock_timestamp() \
                          WHERE job_id = $1 AND name = $2",
                    )
                    .bind(job)
                    .bind(&row_name)
                    .bind(&row_term)
                    .execute(&mut *c)
                    .await?;
                }
                Some(base_hash) => {
                    sqlx::query(
                        "INSERT INTO ontology_entity_refinement \
                                (job_id, name, hidden, synonyms_added, base_hash, updated_by) \
                         VALUES ($1, $2, false, ARRAY[$3::text], $4, $5) \
                         ON CONFLICT (tenant_id, job_id, name) DO UPDATE \
                            SET synonyms_added = array_append(ontology_entity_refinement.synonyms_added, $3), \
                                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
                    )
                    .bind(job)
                    .bind(&row_name)
                    .bind(&row_term)
                    .bind(&base_hash)
                    .bind(&by)
                    .execute(&mut *c)
                    .await?;
                }
            }
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let out = entity_now(&mut tx, &st, &j, &name).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn add_ontology_attribute(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, name)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<OntologyEntity>> {
    let a: AttributeInput = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let e = ontology::entity(&bases, &rows, j.t.tenant, &j.job_key, &name)
        .ok_or_else(|| entity_not_found(&name))?;
    if !is_attribute_name(&a.name) {
        return Err(ApiError::Unprocessable(
            "invalid_attribute",
            "An attribute name is snake_case, e.g. gift_message.".into(),
        ));
    }
    // a base attribute the tenant hid still owns its name
    let hidden_base = bases.attributes.contains_key(&format!("{name}.{}", a.name));
    if hidden_base || e.attributes.iter().any(|x| x.name == a.name) {
        return Err(ApiError::Conflict(
            "attribute_exists",
            format!("{} already has {}.", e.label, a.name),
        ));
    }
    if rows.tenant_entity(&name).is_none() {
        // a base entity: B2's overlay extension (plan R2), no B5 row
        overlay::save_attribute_extension(
            &mut tx,
            &st.design,
            &j,
            AttributeExtension {
                entity: &name,
                name: &a.name,
                data_type: &a.data_type,
                pii: a.pii,
                synonyms: &a.synonyms,
            },
        )
        .await?;
    } else {
        let kind = entity_write_kind(&mut tx, &jr, &name, "entity_attributes").await?;
        let job = j.job_id;
        let row_name = name.clone();
        let item = json!([{
            "name": a.name,
            "data_type": a.data_type,
            "pii": a.pii,
            "synonyms": a.synonyms,
        }]);
        let w = DraftWrite {
            area: AREA,
            page: PAGE,
            kind,
            r#ref: place(&[("name", name.as_str())]),
            label: Label::Now(format!("{name}: attribute {} added", a.name)),
        };
        design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
            Box::pin(async move {
                sqlx::query(
                    "UPDATE ontology_entity \
                        SET attributes = attributes || $3, updated_at = clock_timestamp() \
                      WHERE job_id = $1 AND name = $2",
                )
                .bind(job)
                .bind(&row_name)
                .bind(&item)
                .execute(&mut *c)
                .await?;
                Ok::<(), ApiError>(())
            })
        })
        .await?;
    }
    let out = entity_now(&mut tx, &st, &j, &name).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn set_ontology_entity_hidden(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, name)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<OntologyEntity>> {
    let HiddenInput { hidden } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    if ontology::entity(&bases, &rows, j.t.tenant, &j.job_key, &name).is_none() {
        return Err(entity_not_found(&name));
    }
    let kind = entity_write_kind(&mut tx, &jr, &name, "entity_hidden").await?;
    let hash = if rows.tenant_entity(&name).is_some() {
        None
    } else {
        Some(entity_base_hash(&bases, &name)?)
    };
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let row_name = name.clone();
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind,
        r#ref: place(&[("name", name.as_str())]),
        label: Label::Now(format!(
            "{name} {}",
            if hidden { "hidden" } else { "shown again" }
        )),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            match hash {
                None => {
                    sqlx::query(
                        "UPDATE ontology_entity SET hidden = $3, updated_at = clock_timestamp() \
                          WHERE job_id = $1 AND name = $2",
                    )
                    .bind(job)
                    .bind(&row_name)
                    .bind(hidden)
                    .execute(&mut *c)
                    .await?;
                }
                Some(base_hash) => {
                    sqlx::query(
                        "INSERT INTO ontology_entity_refinement \
                                (job_id, name, hidden, synonyms_added, base_hash, updated_by) \
                         VALUES ($1, $2, $3, '{}', $4, $5) \
                         ON CONFLICT (tenant_id, job_id, name) DO UPDATE \
                            SET hidden = EXCLUDED.hidden, updated_by = EXCLUDED.updated_by, \
                                updated_at = clock_timestamp()",
                    )
                    .bind(job)
                    .bind(&row_name)
                    .bind(hidden)
                    .bind(&base_hash)
                    .bind(&by)
                    .execute(&mut *c)
                    .await?;
                    // back to the base on both fields: no refinement row (plan "Data model")
                    sqlx::query(
                        "DELETE FROM ontology_entity_refinement \
                          WHERE job_id = $1 AND name = $2 AND NOT hidden AND synonyms_added = '{}'",
                    )
                    .bind(job)
                    .bind(&row_name)
                    .execute(&mut *c)
                    .await?;
                }
            }
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let out = entity_now(&mut tx, &st, &j, &name).await?;
    tx.commit().await?;
    Ok(Json(out))
}

// ---- relation writes --------------------------------------------------------------------------------------------

fn relation_label(r: &RelationInput, done: &str) -> String {
    format!("Relation {} {} {} {done}", r.subject, r.predicate, r.object)
}

fn relation_place(r: &RelationInput) -> DraftRef {
    place(&[
        ("subject", r.subject.as_str()),
        ("predicate", r.predicate.as_str()),
        ("object", r.object.as_str()),
    ])
}

async fn create_ontology_relation(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<OntologyRelation>)> {
    let i: RelationInput = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let known = |n: &str| bases.entity(n).is_some() || rows.tenant_entity(n).is_some();
    if !known(&i.subject) {
        return Err(ApiError::Unprocessable(
            "unknown_subject",
            format!("{} is not an entity.", i.subject),
        ));
    }
    if !known(&i.object) {
        return Err(ApiError::Unprocessable(
            "unknown_object",
            format!("{} is not an entity.", i.object),
        ));
    }
    if !is_predicate(&i.predicate) {
        return Err(ApiError::Unprocessable(
            "invalid_predicate",
            "Predicate must be lowerCamel letters and digits.".into(),
        ));
    }
    let base_has = bases
        .relations
        .contains_key(&relation_key(&i.subject, &i.predicate, &i.object));
    if base_has
        || rows
            .tenant_relation(&i.subject, &i.predicate, &i.object)
            .is_some()
    {
        return Err(ApiError::Conflict(
            "relation_exists",
            format!("{} {} {} already exists.", i.subject, i.predicate, i.object),
        ));
    }
    let cardinality = serde_json::to_value(i.cardinality)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| internal("a cardinality does not serialise".into()))?;
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let (s, p, o) = (i.subject.clone(), i.predicate.clone(), i.object.clone());
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "relation",
        r#ref: relation_place(&i),
        label: Label::Now(relation_label(&i, "added")),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO ontology_relation (job_id, subject, predicate, object, cardinality, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(job)
            .bind(&s)
            .bind(&p)
            .bind(&o)
            .bind(&cardinality)
            .bind(&by)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let rows = ontology::draft_rows(&mut tx, j.job_id).await?;
    let out = rows
        .tenant_relation(&i.subject, &i.predicate, &i.object)
        .map(|r| ontology::tenant_relation(r, j.t.tenant, &j.job_key))
        .ok_or_else(|| internal("a relation is gone after its own write".into()))?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn delete_ontology_relation(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<StatusCode> {
    let i: RelationInput = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    if rows
        .tenant_relation(&i.subject, &i.predicate, &i.object)
        .is_none()
    {
        let base_has =
            bases
                .relations
                .contains_key(&relation_key(&i.subject, &i.predicate, &i.object));
        return Err(if base_has {
            ApiError::Conflict(
                "base_row",
                "Base relations come from the template; hide one of its entities instead.".into(),
            )
        } else {
            ApiError::Missing("relation_not_found", "No such relation.".into())
        });
    }
    let job = j.job_id;
    let (s, p, o) = (i.subject.clone(), i.predicate.clone(), i.object.clone());
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "relation",
        r#ref: relation_place(&i),
        label: Label::Now(relation_label(&i, "removed")),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "DELETE FROM ontology_relation \
                  WHERE job_id = $1 AND subject = $2 AND predicate = $3 AND object = $4",
            )
            .bind(job)
            .bind(&s)
            .bind(&p)
            .bind(&o)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
