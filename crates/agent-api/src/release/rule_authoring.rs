//! Tenant rule definitions use the existing overlay design place. Only this validated authoring API writes them;
//! the generic refinement API continues to refuse `rule_slot`, so template rules cannot be rewritten by tenants.
use agent_model::kinds::rule_slot::RuleSlotFields;
use agent_model::kinds::{Kind, validate_fields, validate_key};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{bundle::RuleSlot, compile::TemplateCompiler, hash, rules};
use crate::design::{self, DesignOf, DraftWrite, JobRef, Label, Registry};
use crate::error::{ApiError, ApiResult};
use crate::tenancy::JobCtx;

pub const OBJECT: &str = "rule_slot";

#[derive(Debug, Deserialize)]
pub struct CreateRule {
    pub id: String,
    #[serde(flatten)]
    pub definition: RuleSlotFields,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_rule", message.into())
}

fn decode(item: Value) -> ApiResult<RuleSlot> {
    serde_json::from_value(item).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!("stored rule definition is unreadable: {e}"))
    })
}

pub async fn working(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<RuleSlot>> {
    let rows: Vec<Value> = sqlx::query_scalar("SELECT item FROM job_overlay_row WHERE job_id = $1 AND object = 'rule_slot' AND kind = 'extension' ORDER BY key")
        .bind(job.id).fetch_all(c).await?;
    rows.into_iter().map(decode).collect()
}

pub async fn in_design(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<RuleSlot>> {
    let rows = design::values_of_kind(c, reg, job, of, "overlay").await?;
    rows.into_iter()
        .filter(|r| r.r#ref.get("object").and_then(Value::as_str) == Some(OBJECT))
        .filter_map(|r| {
            r.snap
                .and_then(|v| v.get("item").filter(|v| !v.is_null()).cloned())
        })
        .map(decode)
        .collect()
}

pub fn merge(slots: &mut Vec<RuleSlot>, authored: Vec<RuleSlot>) -> ApiResult<()> {
    for slot in authored {
        if let Some(base) = slots.iter().find(|r| r.id == slot.id) {
            if base != &slot {
                return Err(ApiError::Conflict(
                    "rule_definition_conflict",
                    format!(
                        "Rule {} is defined by both the template and the tenant; rename the tenant rule before publishing.",
                        slot.id
                    ),
                ));
            }
        } else {
            slots.push(slot);
        }
    }
    Ok(())
}

pub async fn save(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    id: &str,
    mut definition: RuleSlotFields,
    create: bool,
) -> ApiResult<Value> {
    validate_key(Kind::RuleSlot, id).map_err(|_| invalid("A rule id starts with an upper-case letter and uses up to 32 upper-case letters, digits or hyphens."))?;
    if definition.evaluation.is_none() {
        return Err(invalid(
            "An authored rule needs an evaluation condition and outcome reasons.",
        ));
    }
    definition.slot_layer = Some("tenant".into());
    let fields = serde_json::to_value(&definition).map_err(|e| invalid(e.to_string()))?;
    validate_fields(Kind::RuleSlot, &fields)
        .map_err(|e| invalid(format!("Invalid rule definition: {e:?}")))?;
    if definition
        .applies_to
        .iter()
        .any(|op| agent_model::kinds::split_operation_key(op).is_none())
    {
        return Err(invalid("Each guarded operation must be an operation key."));
    }
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job.id)
        .execute(&mut *c)
        .await?;
    let platform = rules::template_draft_slots(&mut *c, compiler, &job).await?;
    if platform.iter().any(|r| r.id == id)
        || ["RET-01", "WAR-01", "PAY-04", "DIS-02", "ARR-01"].contains(&id)
    {
        return Err(ApiError::Conflict(
            "template_rule",
            "Template rule definitions are read-only; create a rule with a new id.".into(),
        ));
    }
    let own = working(&mut *c, &job).await?;
    let exists = own.iter().any(|r| r.id == id);
    if create && exists {
        return Err(ApiError::Conflict(
            "rule_exists",
            format!("Rule {id} already exists."),
        ));
    }
    if !create && !exists {
        return Err(ApiError::Missing(
            "rule_not_found",
            format!("No tenant-authored rule {id}."),
        ));
    }
    if create && own.len() >= 64 {
        return Err(invalid("A job may author up to 64 business rules."));
    }
    let mut item = fields;
    item["id"] = json!(id);
    item.as_object_mut().unwrap().remove("summary");
    let slot = decode(item.clone())?;
    let content_hash = hash::row_hash(&item, &[]).map_err(|e| invalid(e.to_string()))?;
    let r = serde_json::from_value(json!({"object": OBJECT, "key": id})).expect("string reference");
    let write = DraftWrite {
        area: "rules",
        page: "J9",
        kind: "overlay",
        r#ref: r,
        label: Label::Now(format!(
            "Rule {} {}",
            definition.label,
            if create { "created" } else { "updated" }
        )),
    };
    let (job_id, key, by) = (job.id, id.to_owned(), j.t.caller.sub.clone());
    let change = if create {
        "Rule definition created"
    } else {
        "Rule definition updated"
    };
    design::track::<(), _>(&mut *c, reg, j, write, move |c| Box::pin(async move {
        sqlx::query("INSERT INTO job_overlay_row (job_id, object, key, kind, item, content_hash, updated_by) VALUES ($1, 'rule_slot', $2, 'extension', $3, $4, $5) ON CONFLICT (tenant_id, job_id, object, key) DO UPDATE SET item = EXCLUDED.item, content_hash = EXCLUDED.content_hash, updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()")
            .bind(job_id).bind(&key).bind(item).bind(content_hash).bind(&by).execute(&mut *c).await?;
        sqlx::query("INSERT INTO business_rule_history (job_id, rule_id, by_sub, change) VALUES ($1, $2, $3, $4)").bind(job_id).bind(key).bind(by).bind(change).execute(c).await?;
        Ok(())
    })).await?;
    rules::view(&mut *c, reg, &job, &slot, None).await
}

pub async fn remove(c: &mut PgConnection, reg: &Registry, j: &JobCtx, id: &str) -> ApiResult<()> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job.id)
        .execute(&mut *c)
        .await?;
    if !working(&mut *c, &job).await?.iter().any(|r| r.id == id) {
        return Err(ApiError::Missing(
            "rule_not_found",
            format!("No tenant-authored rule {id}."),
        ));
    }
    let r = serde_json::from_value(json!({"object": OBJECT, "key": id})).expect("string reference");
    let write = DraftWrite {
        area: "rules",
        page: "J9",
        kind: "overlay",
        r#ref: r,
        label: Label::Now(format!("Rule {id} removed")),
    };
    let key = id.to_owned();
    design::track::<(), _>(c, reg, j, write, move |c| Box::pin(async move {
        sqlx::query("DELETE FROM job_overlay_row WHERE job_id = $1 AND object = 'rule_slot' AND key = $2").bind(job.id).bind(key).execute(c).await?;
        Ok(())
    })).await?;
    Ok(())
}
