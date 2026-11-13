//! F-23 (T10a): a new `operation` proposed from the Operations step. The body is checked by B6b's F-22 validator
//! (`platform::routes::objects::edit::apply_operation_edit`, P6) against the head catalogue, with a synthetic
//! "current" row for the operation that does not exist yet; the stored proposal is the validated `operation` row
//! (`{key, object, verb, variant, fields}`) in `generator_item.new_operation`. Approving it writes that row as an
//! untagged revision in the run's department layer (crate::generator::drive::resolve).
use std::collections::BTreeSet;

use agent_model::catalog::{OperationRow, STAR, op_key};
use agent_model::kinds::catalog::OperationFields;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};
use crate::platform::routes::objects::catalogue;
use crate::platform::routes::objects::edit::{
    OperationContext, OperationEdit, apply_operation_edit, snake,
};

/// `proposeGeneratorOperation` body: `{operation: OperationDraft}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposeBody {
    pub operation: OperationDraft,
}

/// The F-22 `OperationEdit` plus the operation's identity (`object.verb{variant}`) and its modality.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationDraft {
    pub object: String,
    pub verb: String,
    #[serde(default)]
    pub variant: Option<String>,
    /// `do` | `request` | `observe`; default: `observe` for a read, else `do`.
    #[serde(default)]
    pub modality: Option<String>,
    pub identity_level: String,
    #[serde(default)]
    pub raises_identity_level: Option<String>,
    pub side_effect: String,
    pub confirm_back: bool,
    #[serde(default)]
    pub verify_read: Option<String>,
    pub idempotency: String,
    #[serde(default)]
    pub input_fields: Vec<String>,
    #[serde(default)]
    pub output_fields: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub extra_errors: Vec<String>,
}

/// A validated proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposed {
    /// `object.verb` / `object.verb{variant}`.
    pub key: String,
    /// The `operation` row's fields (the F-22 columns, and for a `*` row the columns F-22 leaves alone).
    pub fields: Value,
}

impl Proposed {
    /// The `generator_item.new_operation` value.
    pub fn stored(&self) -> Value {
        json!({"key": self.key, "fields": self.fields})
    }
}

fn invalid(reason: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("operation_invalid", msg.into()).with("reason", reason)
}

/// The validator's refusal as `operation_invalid` carrying the validator's own code (and its `fields[]`).
fn from_validator(e: ApiError) -> ApiError {
    match e {
        ApiError::Unprocessable(code, msg) => invalid(code, msg),
        ApiError::Detailed {
            code,
            message,
            extra,
            ..
        } => {
            let mut out = invalid(code, message);
            for (k, v) in extra {
                out = out.with(&k, v);
            }
            out
        }
        other => other,
    }
}

/// Check `d` against the head catalogue and build the `operation` row. 422 `operation_invalid` (`reason` = the
/// validator's code: `invalid_identity_level`, `invalid_operation`, `unknown_field`, `unknown_verify_read`, or
/// `invalid_key` / `unknown_object` / `star_required` / `invalid_modality`); 409 `operation_exists`.
pub async fn validate(conn: &mut PgConnection, d: &OperationDraft) -> ApiResult<Proposed> {
    let object = d.object.trim();
    let verb = d.verb.trim();
    let variant = d
        .variant
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != STAR)
        .unwrap_or(STAR);
    if !snake(object) || !snake(verb) || (variant != STAR && !snake(variant)) {
        return Err(invalid(
            "invalid_key",
            "An operation is object.verb (and an optional variant) in lower_snake_case.",
        ));
    }
    let key = op_key(object, verb, variant);
    let cat = catalogue::load(&mut *conn).await?;
    let Some((_, def)) = cat.object(object) else {
        return Err(invalid(
            "unknown_object",
            format!("The object {object} is not in the catalogue; propose and approve it first."),
        ));
    };
    if cat.op_row(&key).is_some() {
        return Err(ApiError::Conflict(
            "operation_exists",
            format!("{key} is already an operation of the catalogue."),
        ));
    }
    let star_key = op_key(object, verb, STAR);
    let star = if variant == STAR {
        None
    } else {
        match cat.op_row(&star_key) {
            Some((_, s)) => Some((&s.raw_attrs, s.extra_errors.as_slice())),
            None => {
                return Err(invalid(
                    "star_required",
                    format!("Propose {star_key} before its variant {variant}."),
                ));
            }
        }
    };
    let read_ops: BTreeSet<String> = cat
        .resolved
        .iter()
        .filter(|r| r.attrs.get("side_effect").and_then(Value::as_str) == Some("R"))
        .map(|r| r.key.clone())
        .collect();
    let synthetic = OperationRow {
        attrs: Map::new(),
        key: key.clone(),
        object: object.to_owned(),
        verb: verb.to_owned(),
        variant: variant.to_owned(),
        discriminator: None,
        overridden: Vec::new(),
        inherited: Vec::new(),
        description: String::new(),
        retail: None,
        stages: Vec::new(),
        error_states: Vec::new(),
    };
    let empty = json!({});
    let cx = OperationContext {
        row_fields: &empty,
        star,
        verb,
        current: &synthetic,
        fields: &def.def.fields,
        read_ops: &read_ops,
    };
    let edit = OperationEdit {
        identity_level: d.identity_level.clone(),
        raises_identity_level: d.raises_identity_level.clone(),
        side_effect: d.side_effect.clone(),
        confirm_back: d.confirm_back,
        verify_read: d.verify_read.clone(),
        idempotency: d.idempotency.clone(),
        input_fields: d.input_fields.clone(),
        output_fields: d.output_fields.clone(),
        description: d.description.clone(),
        extra_errors: d.extra_errors.clone(),
    };
    let mut fields = apply_operation_edit(&cx, edit).map_err(from_validator)?;
    if variant == STAR {
        let modality = match d.modality.as_deref().map(str::trim) {
            Some(m @ ("do" | "request" | "observe")) => m.to_owned(),
            Some(m) if !m.is_empty() => {
                return Err(invalid(
                    "invalid_modality",
                    format!("“{m}” is not a modality (do, request, observe)."),
                ));
            }
            _ => {
                if d.side_effect.trim() == "R" {
                    "observe".to_owned()
                } else {
                    "do".to_owned()
                }
            }
        };
        if let Some(attrs) = fields.get_mut("attrs").and_then(Value::as_object_mut) {
            attrs.insert("modality".into(), json!(modality));
            attrs.entry("is_derive").or_insert(json!(false));
            attrs.entry("builtin").or_insert(json!(false));
            attrs.entry("op_kind").or_insert(json!("standard"));
            attrs.entry("guard_rules").or_insert(json!([]));
        }
    }
    if serde_json::from_value::<OperationFields>(fields.clone()).is_err() {
        return Err(invalid(
            "invalid_operation",
            "The operation is not a valid operation row.",
        ));
    }
    Ok(Proposed { key, fields })
}
