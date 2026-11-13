//! The eight overlay kinds (configurator spec section 4.2): key shapes and field schemas, ported from
//! `configurator-core/src/overlay.rs`.
//!
//! Adapted to the agent fixtures, which the seed is validated against: the context-space key is D61's
//! `industry:area:slug`; a procedure version may be a plain integer (`return_request@2`); entity, attribute and
//! job rows carry the extra fields the fixtures author (`object`, `variants`, `axis`, `values`, `note`, `of`,
//! `north_star`, data type `list`); the job-content kinds are checked field by field instead of as care-bus
//! bundle v1 items (their bundle shape is B2's). Every kind may carry `summary`.

use serde_json::{Map, Value};

use super::{FieldError, Kind, Op, is_entity_name, is_snake_name};

/// `^[a-z][A-Za-z0-9]{0,63}$`
fn is_predicate(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// `^[a-z][a-z0-9-]{0,62}$`
fn is_kebab(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `^[a-z][a-z0-9_]{0,62}$` (a procedure name)
fn is_snake63(s: &str) -> bool {
    is_snake_name(s) && s.len() <= 63
}

/// `^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$`, each numeric part at most 9 digits (as care-bus), or a plain revision
/// number `^\d{1,9}$` (the agent fixtures' `return_request@2`).
fn is_version(s: &str) -> bool {
    let digits = |p: &str| !p.is_empty() && p.len() <= 9 && p.chars().all(|c| c.is_ascii_digit());
    if digits(s) {
        return true;
    }
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (s, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| digits(p))
        && pre.is_none_or(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        })
}

/// `[a-z][a-z0-9]*` segments joined by single `sep` characters.
fn is_joined(s: &str, sep: char) -> bool {
    let lower_alnum = |p: &str| {
        p.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    };
    let mut parts = s.split(sep);
    let first_ok = parts
        .next()
        .is_some_and(|p| p.starts_with(|c: char| c.is_ascii_lowercase()) && lower_alnum(p));
    first_ok && parts.all(|p| !p.is_empty() && lower_alnum(p))
}

/// D61 context-space key `<industry>:<area>:<slug>` (web `parseContextSpaceKey`): industry and slug
/// lower_snake_case, area lower-case-with-dashes. Returns `(industry, area, slug)`.
pub fn parse_context_space_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.split(':');
    let (Some(industry), Some(area), Some(slug), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (is_joined(industry, '_') && is_joined(area, '-') && is_joined(slug, '_'))
        .then_some((industry, area, slug))
}

/// The area a context-space key belongs to (`retail:logistics:track_parcel` → `logistics`), as the web
/// `areaOfContextSpace`; `None` when malformed.
pub fn area_of_context_space(key: &str) -> Option<&str> {
    parse_context_space_key(key).map(|(_, area, _)| area)
}

pub(crate) fn key_matches(kind: Kind, key: &str) -> bool {
    match kind {
        Kind::Entity => is_entity_name(key),
        Kind::Attribute => key
            .split_once('.')
            .is_some_and(|(entity, name)| is_entity_name(entity) && is_snake_name(name)),
        Kind::Relation => {
            let parts: Vec<&str> = key.split('|').collect();
            matches!(parts.as_slice(), [s, p, o]
                if is_entity_name(s) && is_predicate(p) && is_entity_name(o))
        }
        Kind::Job => is_snake_name(key),
        Kind::ContextSpace => parse_context_space_key(key).is_some(),
        Kind::Procedure => key
            .split_once('@')
            .is_some_and(|(name, version)| is_snake63(name) && is_version(version)),
        Kind::ConnectorSlot | Kind::KnowledgeStore => is_kebab(key),
        _ => false,
    }
}

// --- field schemas ------------------------------------------------------

#[derive(Clone, Copy)]
enum Ty {
    Str,
    StrList,
    EntityName,
    EntityList,
    Bool,
    /// A non-negative integer.
    Count,
    Object,
    ObjectList,
    /// A step count (the fixtures) or a list of step objects (care-bus `StepV1`).
    CountOrList,
    OneOf(&'static [&'static str]),
}

struct Spec {
    name: &'static str,
    ty: Ty,
    required_on_add: bool,
}

const fn spec(name: &'static str, ty: Ty, required_on_add: bool) -> Spec {
    Spec {
        name,
        ty,
        required_on_add,
    }
}

const DATA_TYPES: &[&str] = &[
    "string", "text", "enum", "money", "number", "boolean", "date", "datetime", "list",
];
/// The UI `ObjectRelation` cardinality union (`types/model.ts`).
const CARDINALITIES: &[&str] = &["1:1", "1:0..1", "1:n", "n:1", "n:m"];
const PROCEDURE_STATUSES: &[&str] = &["DRAFT", "CANARY", "PUBLISHED"];
const AUTH_TYPES: &[&str] = &[
    "oauth2_client_credentials",
    "api_key",
    "mtls_api_key",
    "none",
];
const STORE_KINDS: &[&str] = &["group", "leaf"];

const ENTITY_FIELDS: &[Spec] = &[
    spec("label", Ty::Str, true),
    spec("synonyms", Ty::StrList, false),
    spec("hidden", Ty::Bool, false),
    spec("object", Ty::Str, false),
    spec("variants", Ty::StrList, false),
    spec("summary", Ty::Str, false),
];
const ATTRIBUTE_FIELDS: &[Spec] = &[
    spec("data_type", Ty::OneOf(DATA_TYPES), true),
    spec("pii", Ty::Bool, false),
    spec("synonyms", Ty::StrList, false),
    spec("axis", Ty::Str, false),
    spec("values", Ty::StrList, false),
    spec("note", Ty::Str, false),
    spec("of", Ty::EntityName, false),
    spec("summary", Ty::Str, false),
];
const RELATION_FIELDS: &[Spec] = &[
    spec("cardinality", Ty::OneOf(CARDINALITIES), true),
    spec("summary", Ty::Str, false),
];
const JOB_FIELDS: &[Spec] = &[
    spec("label", Ty::Str, true),
    spec("actor", Ty::Str, false),
    spec("stance", Ty::Str, false),
    spec("preamble", Ty::Str, false),
    spec("actor_label", Ty::Str, false),
    spec("north_star", Ty::Str, false),
    spec("ontology_slice", Ty::EntityList, false),
    spec("channels", Ty::StrList, false),
    spec("escalation", Ty::Object, false),
    spec("use_cases", Ty::StrList, false),
    spec("sops", Ty::StrList, false),
    spec("tools", Ty::StrList, false),
    spec("stores", Ty::StrList, false),
    spec("summary", Ty::Str, false),
];
const CONTEXT_SPACE_FIELDS: &[Spec] = &[
    spec("label", Ty::Str, true),
    // the qualifier's disambiguating sentence for this space (shown in `[task qualify]` instead of the label)
    spec("description", Ty::Str, false),
    spec("area", Ty::Str, true),
    spec("request_type", Ty::Str, false),
    spec("required", Ty::StrList, false),
    spec("entities", Ty::EntityList, false),
    spec("procedure", Ty::Str, false),
    // the activity this space serves (its tools come from the space's procedure); none for a cross-cutting space
    spec("activity", Ty::Str, false),
    spec("knowledge_kinds", Ty::StrList, false),
    spec("slots", Ty::StrList, false),
    spec("summary", Ty::Str, false),
];
const PROCEDURE_FIELDS: &[Spec] = &[
    spec("label", Ty::Str, true),
    spec("uses_slots", Ty::StrList, false),
    spec("status", Ty::OneOf(PROCEDURE_STATUSES), false),
    spec("context_space", Ty::Str, false),
    spec("domain", Ty::Str, false),
    spec("description", Ty::Str, false),
    spec("pddl_domain", Ty::Str, false),
    spec("tool_requirements", Ty::StrList, false),
    spec("success_criteria", Ty::StrList, false),
    spec("reviewers", Ty::StrList, false),
    spec("plan", Ty::Object, false),
    spec("steps", Ty::CountOrList, false),
    spec("summary", Ty::Str, false),
];
const CONNECTOR_SLOT_FIELDS: &[Spec] = &[
    spec("label", Ty::Str, true),
    spec("implements", Ty::StrList, false),
    spec("name", Ty::Str, false),
    spec("system", Ty::Str, false),
    spec("auth_type", Ty::OneOf(AUTH_TYPES), false),
    spec("base_url", Ty::Str, false),
    spec("timeout_ms", Ty::Count, false),
    spec("rate_limit", Ty::Str, false),
    spec("field_mappings", Ty::ObjectList, false),
    spec("summary", Ty::Str, false),
];
const KNOWLEDGE_STORE_FIELDS: &[Spec] = &[
    spec("label", Ty::Str, true),
    spec("kinds", Ty::StrList, false),
    spec("kinds_added", Ty::StrList, false),
    spec("parent", Ty::Str, false),
    spec("kind", Ty::OneOf(STORE_KINDS), false),
    spec("areas", Ty::StrList, false),
    spec("description", Ty::Str, false),
    spec("summary", Ty::Str, false),
];

fn specs(kind: Kind) -> &'static [Spec] {
    match kind {
        Kind::Entity => ENTITY_FIELDS,
        Kind::Attribute => ATTRIBUTE_FIELDS,
        Kind::Relation => RELATION_FIELDS,
        Kind::Job => JOB_FIELDS,
        Kind::ContextSpace => CONTEXT_SPACE_FIELDS,
        Kind::Procedure => PROCEDURE_FIELDS,
        Kind::ConnectorSlot => CONNECTOR_SLOT_FIELDS,
        Kind::KnowledgeStore => KNOWLEDGE_STORE_FIELDS,
        _ => &[],
    }
}

/// The fields a job-content row's key supplies (never stored in `fields`).
fn key_fields(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::ContextSpace | Kind::KnowledgeStore => &["key"],
        Kind::Procedure => &["name", "version"],
        Kind::ConnectorSlot => &["id"],
        _ => &[],
    }
}

fn str_list(v: &Value) -> Option<Vec<&str>> {
    v.as_array()?.iter().map(Value::as_str).collect()
}

/// `None` when the value fits the type, otherwise a message. `null` never fits: an unset field is omitted.
fn check_value(ty: Ty, v: &Value) -> Option<String> {
    match ty {
        Ty::Str => match v.as_str() {
            Some(s) if !s.trim().is_empty() => None,
            _ => Some("must be a non-empty string".into()),
        },
        Ty::StrList => match str_list(v) {
            Some(_) => None,
            None => Some("must be an array of strings".into()),
        },
        Ty::EntityName => match v.as_str() {
            Some(s) if is_entity_name(s) => None,
            _ => Some("must be an entity name (e.g. Offering)".into()),
        },
        Ty::EntityList => match str_list(v) {
            Some(items) => items
                .iter()
                .find(|s| !is_entity_name(s))
                .map(|s| format!("{s:?} is not a valid entity name")),
            None => Some("must be an array of entity names".into()),
        },
        Ty::Bool => (!v.is_boolean()).then(|| "must be a boolean".to_owned()),
        Ty::Count => v
            .as_u64()
            .is_none()
            .then(|| "must be a non-negative integer".to_owned()),
        Ty::Object => (!v.is_object()).then(|| "must be an object".to_owned()),
        Ty::ObjectList => match v.as_array() {
            Some(items) if items.iter().all(Value::is_object) => None,
            _ => Some("must be an array of objects".into()),
        },
        Ty::CountOrList => match v {
            Value::Number(n) if n.as_u64().is_some() => None,
            Value::Array(items) if items.iter().all(Value::is_object) => None,
            _ => Some("must be a step count or an array of steps".into()),
        },
        Ty::OneOf(allowed) => match v.as_str() {
            Some(s) if allowed.contains(&s) => None,
            _ => Some(format!("must be one of {}", allowed.join(" | "))),
        },
    }
}

/// Validate the `fields` of an `add` (complete) or `patch` (partial, at least one field) overlay row.
pub(crate) fn validate(
    kind: Kind,
    op: Op,
    fields: &Map<String, Value>,
) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    if op == Op::Patch && fields.is_empty() {
        errors.push(FieldError::new(
            "empty_patch",
            None,
            "a patch must name at least one field",
        ));
    }
    let known = specs(kind);
    let from_key = key_fields(kind);
    for (name, value) in fields {
        if from_key.contains(&name.as_str()) {
            errors.push(FieldError::new(
                "unknown_field",
                Some(name.as_str()),
                format!("field {name:?} of {kind} comes from the row key"),
            ));
            continue;
        }
        match known.iter().find(|s| s.name == name.as_str()) {
            None => errors.push(FieldError::new(
                "unknown_field",
                Some(name.as_str()),
                format!("unknown field {name:?} for {kind}"),
            )),
            Some(s) => {
                if let Some(msg) = check_value(s.ty, value) {
                    errors.push(FieldError::new("invalid_value", Some(name.as_str()), msg));
                }
            }
        }
    }
    if op == Op::Add {
        for s in known.iter().filter(|s| s.required_on_add) {
            if !fields.contains_key(s.name) {
                errors.push(FieldError::new(
                    "missing_field",
                    Some(s.name),
                    format!("field {:?} is required on add", s.name),
                ));
            }
        }
    }
    super::done(errors)
}

#[cfg(test)]
mod tests {
    use super::super::{validate_fields, validate_key, validate_row};
    use super::*;
    use serde_json::json;

    fn codes(r: Result<(), Vec<FieldError>>) -> Vec<&'static str> {
        r.err().unwrap_or_default().iter().map(|e| e.code).collect()
    }

    #[test]
    fn key_shapes() {
        let ok = [
            (Kind::Entity, "Order"),
            (Kind::Attribute, "Order.order_id"),
            (Kind::Relation, "Order|paidBy|Payment"),
            (Kind::Job, "customer_care_agent"),
        ];
        for (kind, key) in ok {
            assert_eq!(validate_key(kind, key), Ok(()), "{key}");
        }
        let bad = [
            (Kind::Entity, "order"),
            (Kind::Attribute, "Order.Status"),
            (Kind::Relation, "Order|paid by|Payment"),
            (Kind::Attribute, "Order."),
            (Kind::Entity, ""),
            (Kind::Job, ""),
            (Kind::Relation, "Order|paidBy"),
            (Kind::Entity, "Order.id"),
        ];
        for (kind, key) in bad {
            let err = validate_key(kind, key).err();
            assert_eq!(err.map(|e| e.code), Some("invalid_key"), "{key:?}");
        }
        let long = format!("A{}", "b".repeat(64));
        assert!(validate_key(Kind::Entity, &long).is_err());
        assert!(validate_key(Kind::Entity, &long[..64]).is_ok());
    }

    #[test]
    fn add_entity_needs_label() {
        assert_eq!(
            codes(validate_fields(Kind::Entity, &json!({}))),
            vec!["missing_field"]
        );
        let ok = json!({"label": "Order", "synonyms": ["purchase"], "hidden": false});
        assert!(validate_fields(Kind::Entity, &ok).is_ok());
    }

    #[test]
    fn patch_unknown_field_rejected() {
        assert_eq!(
            codes(validate_row(
                Kind::Entity,
                Op::Patch,
                &json!({"colour": "red"})
            )),
            vec!["unknown_field"]
        );
        assert!(validate_row(Kind::Entity, Op::Patch, &json!({"hidden": true})).is_ok());
    }

    #[test]
    fn empty_patch_rejected() {
        assert_eq!(
            codes(validate_row(Kind::Job, Op::Patch, &json!({}))),
            vec!["empty_patch"]
        );
    }

    #[test]
    fn attribute_data_type_enum() {
        for t in DATA_TYPES {
            let f = json!({ "data_type": t });
            assert!(validate_fields(Kind::Attribute, &f).is_ok(), "{t}");
        }
        assert_eq!(
            codes(validate_fields(
                Kind::Attribute,
                &json!({"data_type": "integer"})
            )),
            vec!["invalid_value"]
        );
        assert_eq!(
            codes(validate_fields(Kind::Attribute, &json!({}))),
            vec!["missing_field"]
        );
    }

    #[test]
    fn relation_cardinality_is_the_ui_union() {
        for c in ["1:1", "1:0..1", "1:n", "n:1", "n:m"] {
            assert!(
                validate_fields(Kind::Relation, &json!({ "cardinality": c })).is_ok(),
                "{c}"
            );
        }
        assert_eq!(
            codes(validate_fields(
                Kind::Relation,
                &json!({"cardinality": "0..1:1"})
            )),
            vec!["invalid_value"]
        );
    }

    #[test]
    fn relation_and_job_fields() {
        assert!(validate_fields(Kind::Relation, &json!({"cardinality": "n:m"})).is_ok());
        assert!(validate_fields(Kind::Relation, &json!({"cardinality": "2:3"})).is_err());
        // `n:n` is not a cardinality (the fixture's `Location|affectedBy|ServiceEvent` must say `n:m`).
        assert!(validate_fields(Kind::Relation, &json!({"cardinality": "n:n"})).is_err());

        let job = json!({
            "label": "Care", "actor": "agent", "ontology_slice": ["Order"],
            "escalation": {"to": "human"}, "channels": ["voice"]
        });
        assert!(validate_fields(Kind::Job, &job).is_ok());
        assert_eq!(
            codes(validate_row(
                Kind::Job,
                Op::Patch,
                &json!({"ontology_slice": ["order"]})
            )),
            vec!["invalid_value"]
        );
        assert_eq!(
            codes(validate_row(
                Kind::Job,
                Op::Patch,
                &json!({"preamble": " "})
            )),
            vec!["invalid_value"]
        );
    }

    #[test]
    fn context_space_key_validation() {
        for key in [
            "retail:order-support:returns",
            "retail:logistics:track_parcel",
            "it_services:access:password_reset",
        ] {
            assert_eq!(validate_key(Kind::ContextSpace, key), Ok(()), "{key}");
        }
        for key in [
            "Retail:x:y",
            "retail",
            "retail:faq",
            "a:b:c:d",
            "a::b",
            "retail:order support:x",
            "retail:order_support:x",
            "retail:faq:gift-cards",
            "retail_:faq:x",
        ] {
            assert!(validate_key(Kind::ContextSpace, key).is_err(), "{key}");
        }
        assert_eq!(
            area_of_context_space("retail:logistics:track_parcel"),
            Some("logistics")
        );
        assert_eq!(area_of_context_space("retail:faq"), None);
    }

    #[test]
    fn procedure_and_slot_keys() {
        for key in [
            "return@1.3.0",
            "return@1.4.0-beta",
            "return_request@2",
            "password_reset@0",
        ] {
            assert!(validate_key(Kind::Procedure, key).is_ok(), "{key}");
        }
        for key in [
            "return",
            "return@1.3",
            "Return@1.0.0",
            "return@1.0.0-",
            "return@",
        ] {
            assert!(validate_key(Kind::Procedure, key).is_err(), "{key}");
        }
        assert!(validate_key(Kind::ConnectorSlot, "crm-core").is_ok());
        assert!(validate_key(Kind::ConnectorSlot, "CRM").is_err());
        assert!(validate_key(Kind::KnowledgeStore, "faq-policies").is_ok());
        assert!(validate_key(Kind::KnowledgeStore, "faq_policies").is_err());
    }

    #[test]
    fn job_content_fields() {
        // Key fields come from the key; unknown fields and nulls are refused.
        assert_eq!(
            codes(validate_row(
                Kind::Procedure,
                Op::Patch,
                &json!({"name": "return"})
            )),
            vec!["unknown_field"]
        );
        assert_eq!(
            codes(validate_row(
                Kind::ConnectorSlot,
                Op::Patch,
                &json!({"secret_ref": "k/v"})
            )),
            vec!["unknown_field"]
        );
        assert_eq!(
            codes(validate_row(
                Kind::Procedure,
                Op::Patch,
                &json!({"description": null})
            )),
            vec!["invalid_value"]
        );
        assert!(validate_row(Kind::Procedure, Op::Patch, &json!({"status": "CANARY"})).is_ok());
        let steps = json!({"label": "Return", "steps": [{"step_number": 1}]});
        assert!(validate_fields(Kind::Procedure, &steps).is_ok());
        assert_eq!(
            codes(validate_fields(
                Kind::Procedure,
                &json!({"label": "Return", "steps": -1})
            )),
            vec!["invalid_value"]
        );
        assert_eq!(
            codes(validate_fields(
                Kind::ContextSpace,
                &json!({"label": "Returns"})
            )),
            vec!["missing_field"]
        );
    }

    /// Row shapes exactly as `web/src/lib/fixtures/platform.ts` authors them (the seed is validated on boot).
    #[test]
    fn fixture_row_shapes_are_accepted() {
        let rows = [
            (
                Kind::Entity,
                Op::Add,
                json!({"label": "Customer", "synonyms": ["customer", "klant", "account holder"], "object": "party"}),
            ),
            (
                Kind::Entity,
                Op::Add,
                json!({"label": "Case", "synonyms": ["ticket"], "variants": ["return", "warranty_claim"]}),
            ),
            (
                Kind::Entity,
                Op::Patch,
                json!({"synonyms": ["purchase", "ord_no", "bestelling"]}),
            ),
            (
                Kind::Entity,
                Op::Patch,
                json!({"variants": ["incident", "service_request"]}),
            ),
            (
                Kind::Entity,
                Op::Tombstone,
                json!({"reason": "Gift cards fold into Offering (price_tier) + Payment; see D24"}),
            ),
            (
                Kind::Attribute,
                Op::Add,
                json!({"data_type": "string", "pii": true}),
            ),
            (
                Kind::Attribute,
                Op::Add,
                json!({"data_type": "enum", "axis": "customer_type"}),
            ),
            (
                Kind::Attribute,
                Op::Add,
                json!({"data_type": "enum", "values": ["free", "paid"], "note": "freemium = price_tier free (D24)"}),
            ),
            (
                Kind::Attribute,
                Op::Add,
                json!({"data_type": "list", "of": "Offering"}),
            ),
            (Kind::Relation, Op::Add, json!({"cardinality": "1:n"})),
            (
                Kind::Job,
                Op::Add,
                json!({"label": "Customer care agent", "actor": "customer", "stance": "aftersales is presales", "north_star": "Containment rate"}),
            ),
            (
                Kind::Job,
                Op::Patch,
                json!({"preamble": "You help customers with orders, deliveries, returns and warranty."}),
            ),
            (
                Kind::ContextSpace,
                Op::Add,
                json!({"label": "Returns", "area": "order-support", "knowledge_kinds": ["policy.returns"], "slots": ["order_id", "reason"]}),
            ),
            (
                Kind::ContextSpace,
                Op::Add,
                json!({"label": "Returns", "area": "order-support", "slots": ["order_id"]}),
            ),
            (
                Kind::ContextSpace,
                Op::Tombstone,
                json!({"reason": "Gift cards removed from retail@v2"}),
            ),
            (
                Kind::KnowledgeStore,
                Op::Add,
                json!({"label": "FAQ & policies", "kinds": ["policy.returns", "faq.general"]}),
            ),
            (
                Kind::KnowledgeStore,
                Op::Patch,
                json!({"kinds_added": ["catalog", "guide.product_usage"]}),
            ),
            (
                Kind::Procedure,
                Op::Add,
                json!({"label": "Return request", "uses_slots": ["shop", "helpdesk"], "steps": 6}),
            ),
            (
                Kind::ConnectorSlot,
                Op::Add,
                json!({"label": "Webshop", "implements": ["order.list", "order.read", "payment.refund"]}),
            ),
        ];
        for (kind, op, fields) in rows {
            assert_eq!(validate_row(kind, op, &fields), Ok(()), "{kind} {fields}");
        }
    }
}
