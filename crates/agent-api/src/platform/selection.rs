//! The hierarchy shape of a stored template selection (D106/D111/D115): [`selection_v2`] rewrites a pre-KH
//! selection on read, so an immutable pre-KH release snapshot reads in the current shape.

use serde_json::Value;

/// A selection in the D106/D111/D115 shape. A pre-KH selection keeps the shape it was stored with
/// — bare department keys under `industry_depts`, `dimensions.region[]`, `industry: null` — so this
/// rewrites it on read, exactly as migration 0132 (`kh_selection_v2`) rewrote the rows already stored:
/// `departments` = `industry_dept/<industry>/<dept>` per entry (order kept), `region` = the first region (else a
/// `region` already there), `industry` null → `""`. A selection naming `region` under `dimensions` without
/// `industry_depts` is rewritten too (KH-F3: the 0132 CHECKs refuse it); a non-object `dimensions` is dropped. One
/// with neither is already in the new shape and is returned unchanged; one without an industry stays incomplete
/// (its readers refuse it `hierarchy_incomplete`). Ruling R15: every reader of a stored selection
/// (`design::read::selection_of_value`) applies it, so an immutable pre-KH release snapshot reads in this shape.
pub fn selection_v2(v: &Value) -> Value {
    let Some(o) = v.as_object() else {
        return v.clone();
    };
    let has_depts = o.contains_key("industry_depts");
    if !has_depts && !o.get("dimensions").is_some_and(names_region) {
        return v.clone();
    }
    let mut out = o.clone();
    out.remove("industry_depts");
    out.remove("dimensions");
    if has_depts {
        let industry = o
            .get("industry")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let departments: Vec<Value> = o
            .get("industry_depts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|d| Value::String(format!("industry_dept/{industry}/{d}")))
            .collect();
        out.insert("industry".to_owned(), Value::String(industry));
        out.insert("departments".to_owned(), Value::Array(departments));
    }
    if let Some(dims) = o.get("dimensions").and_then(Value::as_object) {
        let region = dims
            .get("region")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .or_else(|| o.get("region"))
            .cloned()
            .unwrap_or(Value::Null);
        let mut dims = dims.clone();
        dims.remove("region");
        out.insert("dimensions".to_owned(), Value::Object(dims));
        out.insert("region".to_owned(), region);
    }
    Value::Object(out)
}

/// jsonb `dimensions ? 'region'` (the 0132 CHECKs' predicate): a key of an object, a string element of an array, or
/// the string itself.
fn names_region(d: &Value) -> bool {
    match d {
        Value::Object(m) => m.contains_key("region"),
        Value::Array(a) => a.iter().any(|x| x.as_str() == Some("region")),
        Value::String(s) => s == "region",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_selections_are_read_in_the_hierarchy_shape() {
        let legacy = json!({
            "profile": "retail_ecommerce",
            "industry": "retail",
            "industry_depts": ["customer_service", "returns"],
            "areas": ["faq"],
            "dimensions": {"channel": ["chat"], "region": ["nl", "be"]},
        });
        assert_eq!(
            selection_v2(&legacy),
            json!({
                "profile": "retail_ecommerce",
                "industry": "retail",
                "departments": ["industry_dept/retail/customer_service", "industry_dept/retail/returns"],
                "areas": ["faq"],
                "dimensions": {"channel": ["chat"]},
                "region": "nl",
            })
        );
        let new = selection_v2(&legacy);
        assert_eq!(selection_v2(&new), new, "idempotent");
        assert_eq!(
            selection_v2(
                &json!({"industry": "it_services", "industry_depts": ["it_service_desk"], "profile": null})
            ),
            json!({"industry": "it_services", "departments": ["industry_dept/it_services/it_service_desk"], "profile": null})
        );
        let base_only = selection_v2(
            &json!({"industry": null, "industry_depts": [], "dimensions": {"channel": [], "region": []}}),
        );
        assert_eq!(
            base_only,
            json!({"industry": "", "departments": [], "dimensions": {"channel": []}, "region": null})
        );
    }
}
