//! B8 T10 / T10a: proposals on a PLATFORM run. A drafted item may name an object the catalogue does not have
//! (`new_object`, `proposeGeneratorObject`) or an operation (`operation_expectation` + `proposeGeneratorOperation`,
//! F-23, validated by B6b's F-22 validator in [`super::operation`]). A super_admin resolves a proposal
//! (`resolveGeneratorProposal`): approve writes the object / operation as an UNTAGGED revision of the run's
//! department layer (author `generator:<run id>`) and the proposal turns `approved`; reject turns it `rejected` and
//! writes nothing. Nothing is live either way: a job sees it only through tag, pin and release.
use agent_model::kinds::catalog::{FieldType, ObjectField, ObjectFields, ObjectTier};
use agent_model::kinds::{Kind, Op};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::run::RunRow;
use super::store::{self, ItemFull};
use super::{accept, drive, operation};
use crate::audit::Actor;
use crate::error::{ApiError, ApiResult};
use crate::platform::db::authored::{Expect, current_revision, write_authored};
use crate::platform::db::layers;

fn conflict(code: &'static str, msg: &str) -> ApiError {
    ApiError::Conflict(code, msg.into())
}

/// `{run}:{item}` → (run, item). The run id is the part before the FIRST `:`; a segment without one, or whose run is
/// not a number, is 404 `proposal_not_found`.
pub fn split(proposal: &str) -> ApiResult<(i64, &str)> {
    proposal
        .split_once(':')
        .and_then(|(r, i)| Some((r.parse::<i64>().ok()?, i)).filter(|(_, i)| !i.is_empty()))
        .ok_or_else(|| ApiError::Missing("proposal_not_found", format!("No proposal {proposal}.")))
}

/// `proposeGeneratorObject`: a `new_object` item becomes a proposal. 404 `item_not_found`; 422 `not_a_new_object`;
/// 409 `already_proposed`.
pub async fn propose_object(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    item: &str,
) -> ApiResult<Value> {
    drive::assert_open(run)?;
    let it = store::item_for_update(&mut *c, run.id, item).await?;
    let Some(object) = it.new_object.as_deref() else {
        return Err(ApiError::Unprocessable(
            "not_a_new_object",
            "Only an item that names a new object can be proposed.".into(),
        ));
    };
    if it.proposal != "none" {
        return Err(conflict(
            "already_proposed",
            "This item is already proposed.",
        ));
    }
    sqlx::query(
        "UPDATE generator_item SET proposal = 'proposed' WHERE run_id = $1 AND item_id = $2",
    )
    .bind(run.id)
    .bind(item)
    .execute(&mut *c)
    .await?;
    store::touch(&mut *c, run.id).await?;
    drive::event(
        &mut *c,
        run,
        actor,
        "generator.object_propose",
        json!({"item": item, "object": object}),
    )
    .await?;
    drive::detail(c, run).await
}

/// `proposeGeneratorOperation` (F-23). 404 `item_not_found`; 422 `not_an_operations_item`, `operation_invalid`
/// (the validator's `reason` and `fields[]`); 409 `already_proposed`, `operation_exists`.
pub async fn propose_operation(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    item: &str,
    body: operation::ProposeBody,
) -> ApiResult<Value> {
    drive::assert_open(run)?;
    let it = store::item_for_update(&mut *c, run.id, item).await?;
    if it.step != "operations" || it.kind != "operation_expectation" {
        return Err(ApiError::Unprocessable(
            "not_an_operations_item",
            "Only an item of the Operations step can propose an operation.".into(),
        ));
    }
    if it.proposal != "none" {
        return Err(conflict(
            "already_proposed",
            "This item is already proposed.",
        ));
    }
    let proposed = operation::validate(&mut *c, &body.operation).await?;
    sqlx::query(
        "UPDATE generator_item SET proposal = 'proposed', new_operation = $3 WHERE run_id = $1 AND item_id = $2",
    )
    .bind(run.id)
    .bind(item)
    .bind(proposed.stored())
    .execute(&mut *c)
    .await?;
    store::touch(&mut *c, run.id).await?;
    drive::event(
        &mut *c,
        run,
        actor,
        "generator.operation_propose",
        json!({"item": item, "operation": proposed.key}),
    )
    .await?;
    drive::detail(c, run).await
}

/// The object definition of a `new_object` item: its example row names the fields (a number is `decimal` /
/// `int`, a boolean `bool`, anything else `string`); an object with no usable example has the one `id` field.
fn object_def(it: &ItemFull) -> ObjectFields {
    let mut fields: Vec<ObjectField> = Vec::new();
    if let Some(row) = it.example.as_ref().and_then(Value::as_object) {
        for (name, v) in row {
            let valid = !name.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !valid {
                continue;
            }
            let data_type = match v {
                Value::Bool(_) => FieldType::Bool,
                Value::Number(n) if n.is_i64() || n.is_u64() => FieldType::Int,
                Value::Number(_) => FieldType::Decimal,
                _ if name == "id" => FieldType::Id,
                _ => FieldType::String,
            };
            fields.push(ObjectField {
                field: name.clone(),
                data_type,
                pii: false,
                enum_values: Vec::new(),
                ref_to: None,
                required: false,
                description: String::new(),
                retired: false,
            });
        }
    }
    if fields.is_empty() {
        fields.push(ObjectField {
            field: "id".into(),
            data_type: FieldType::Id,
            pii: false,
            enum_values: Vec::new(),
            ref_to: None,
            required: true,
            description: String::new(),
            retired: false,
        });
    }
    let label = it
        .new_object
        .as_deref()
        .map(|o| o.replace('_', " "))
        .unwrap_or_default();
    ObjectFields {
        label,
        tier: ObjectTier::Optional,
        storage_shape_of: None,
        origin: "generator".into(),
        description: it.detail.clone(),
        in_retail_job: false,
        fields,
        relations: Vec::new(),
        states: Vec::new(),
        transitions: Vec::new(),
        summary: None,
    }
}

/// `resolveGeneratorProposal`. 404 `item_not_found`; 409 `not_proposed`; an approved row the layer store refuses is
/// that refusal (422), and nothing changes.
pub async fn resolve(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    item: &str,
    approve: bool,
) -> ApiResult<Value> {
    drive::assert_open(run)?;
    let it = store::item_for_update(&mut *c, run.id, item).await?;
    if it.proposal != "proposed" {
        return Err(conflict(
            "not_proposed",
            "This item has no pending proposal.",
        ));
    }
    let mut what = json!({"item": item, "approve": approve});
    if approve {
        let layer_key = accept::dept_layer(run);
        let by = accept::author(run.id);
        let layer = layers::layer_by_key(&mut *c, &layer_key).await?;
        crate::platform::db::lock_family(&mut *c, &layer.family).await?;
        let (kind, key, fields, listed) = if let Some(op) = it.new_operation.as_ref() {
            let key = op["key"].as_str().unwrap_or_default().to_owned();
            (
                Kind::Operation,
                key.clone(),
                op["fields"].clone(),
                format!("operation:{key}"),
            )
        } else if let Some(object) = it.new_object.clone() {
            let def =
                serde_json::to_value(object_def(&it)).map_err(|e| ApiError::Internal(e.into()))?;
            (Kind::Object, object.clone(), def, object)
        } else {
            return Err(conflict(
                "not_proposed",
                "This item has no pending proposal.",
            ));
        };
        let (op, expect) = match current_revision(&mut *c, &layer, kind, &key).await? {
            Some(cur) if !cur.tombstone => (Op::Patch, Expect::Rev(cur.rev)),
            _ => (Op::Add, Expect::New),
        };
        write_authored(
            &mut *c, actor, &layer_key, kind, &key, op, fields, &by, expect,
        )
        .await?;
        what["written"] = json!(format!("{kind} {key}"));
        if kind == Kind::Object {
            sqlx::query("UPDATE generator_run SET objects = array_append(objects, $2) WHERE id = $1 AND NOT ($2 = ANY(objects))")
                .bind(run.id)
                .bind(&listed)
                .execute(&mut *c)
                .await?;
        } else {
            sqlx::query("UPDATE generator_run SET output = array_append(output, $2) WHERE id = $1 AND NOT ($2 = ANY(output))")
                .bind(run.id)
                .bind(&listed)
                .execute(&mut *c)
                .await?;
        }
    }
    sqlx::query("UPDATE generator_item SET proposal = $3 WHERE run_id = $1 AND item_id = $2")
        .bind(run.id)
        .bind(item)
        .bind(if approve { "approved" } else { "rejected" })
        .execute(&mut *c)
        .await?;
    store::touch(&mut *c, run.id).await?;
    drive::event(&mut *c, run, actor, "generator.proposal_resolve", what).await?;
    drive::detail(c, run).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_ids_split_on_the_first_colon() {
        assert_eq!(split("12:12-objects-3").unwrap(), (12, "12-objects-3"));
        assert_eq!(split("12:a:b").unwrap(), (12, "a:b"));
        for bad in ["12", "x:1", ":1", "12:"] {
            assert_eq!(
                split(bad).unwrap_err().code(),
                "proposal_not_found",
                "{bad}"
            );
        }
    }
}
