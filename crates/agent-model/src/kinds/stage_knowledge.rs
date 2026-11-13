//! `stage_knowledge_kind` (D107, plan KH T3; spec 11 §4.3): a knowledge kind a stage asks for. Declared at any
//! template layer (base, industry, industry_dept, job; their region sub-layers once D111 adds them) and folded down
//! D112's order by [`crate::compile::fold_knowledge`]. Key `stage|kind`; `stage` may be `*` (every stage).
//!
//! Fields: `label`, and optionally `description`, `store_key`, `required` (a lower layer may set it false → true
//! only: `true → false` is the compile error `kind_required_downgrade`), `tenant_data` (D110: frozen packs alone do
//! not satisfy the kind) and the `summary` every row stores. Rows written before KH carry `{label, summary}` only;
//! their summary `store <key> · required|optional` still reads as the store key and the `required` flag.

use serde::{Deserialize, Serialize};

use super::{FieldError, done, require_text};

/// The wildcard stage of a `stage_knowledge_kind` key: the kind is asked for at every stage.
pub const ANY_STAGE: &str = "*";

/// Fields of a `stage_knowledge_kind` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageKnowledgeKindFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The knowledge store the kind is meant to be bound to (a store key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_key: Option<String>,
    /// `None` = inherit (an add without it is optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// D110: the kind is the tenant's own data (order status, delivery slots); `None` = inherit (false on an add).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_data: Option<bool>,
}

fn nonblank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

impl StageKnowledgeKindFields {
    /// The `·`-separated parts of the stored summary (the pre-KH carrier of store and `required`).
    fn summary_parts(&self) -> impl Iterator<Item = &str> {
        self.summary
            .as_deref()
            .unwrap_or("")
            .split('·')
            .map(str::trim)
            .filter(|p| !p.is_empty())
    }

    /// The row's `required` flag: the field, else a summary part `required` (true) or `optional` (false), else
    /// `None` (inherit).
    pub fn required_flag(&self) -> Option<bool> {
        self.required.or_else(|| {
            if self.summary_parts().any(|p| p == "required") {
                Some(true)
            } else if self.summary_parts().any(|p| p == "optional") {
                Some(false)
            } else {
                None
            }
        })
    }

    /// The store key: the field, else a summary part `store <key>`.
    pub fn store(&self) -> Option<String> {
        nonblank(self.store_key.as_deref())
            .map(str::to_owned)
            .or_else(|| {
                self.summary_parts()
                    .find_map(|p| nonblank(p.strip_prefix("store ")))
                    .map(str::to_owned)
            })
    }

    /// What the kind is for: the description, else the summary, else the label.
    pub fn description_text(&self) -> String {
        nonblank(self.description.as_deref())
            .or_else(|| nonblank(self.summary.as_deref()))
            .unwrap_or(self.label.trim())
            .to_owned()
    }
}

/// D107 `pack_kind_undeclared`: whether the declared kind `decl_stage|decl_kind` covers a pack reference's
/// `stage|kind`. `*` on either stage matches any stage; a pack kind `*` names no kind, so any declaration covers it.
pub fn declares(decl_stage: &str, decl_kind: &str, stage: &str, kind: &str) -> bool {
    let any = super::knowledge_pack::ANY;
    (decl_stage == ANY_STAGE || stage == any || decl_stage == stage)
        && (kind == any || decl_kind == kind)
}

/// D107: whether an archetype's `knowledge_kinds` entry covers kind `kind` — the same kind, or a family pattern
/// `<family>.*` whose prefix `kind` starts with (`policy.*` covers `policy.returns`). A pack kind `*` is covered.
pub fn archetype_covers(entry: &str, kind: &str) -> bool {
    kind == super::knowledge_pack::ANY
        || entry == kind
        || entry
            .strip_suffix('*')
            .is_some_and(|prefix| prefix.ends_with('.') && kind.starts_with(prefix))
}

pub(crate) fn check(f: &StageKnowledgeKindFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    if let Some(store) = &f.store_key {
        require_text(store, "store_key", &mut errors);
    }
    done(errors)
}

/// The one-line summary of a row: the stored summary, else `store <key> · required|optional · tenant data`, else
/// the label.
pub(crate) fn summary(f: &StageKnowledgeKindFields) -> String {
    if let Some(s) = nonblank(f.summary.as_deref()) {
        return s.to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(store) = nonblank(f.store_key.as_deref()) {
        parts.push(format!("store {store}"));
    }
    match f.required {
        Some(true) => parts.push("required".to_owned()),
        Some(false) => parts.push("optional".to_owned()),
        None => {}
    }
    if f.tenant_data == Some(true) {
        parts.push("tenant data".to_owned());
    }
    if parts.is_empty() {
        f.label.trim().to_owned()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, Op, summary as kind_summary, validate_key, validate_row};
    use super::*;
    use serde_json::json;

    fn fields(v: serde_json::Value) -> StageKnowledgeKindFields {
        serde_json::from_value(v).expect("fields")
    }

    #[test]
    fn new_fields_and_legacy_summaries() {
        let k = Kind::StageKnowledgeKind;
        let row = json!({"label": "Order status", "store_key": "orders", "required": true, "tenant_data": true});
        assert_eq!(validate_row(k, Op::Add, &row), Ok(()));
        assert_eq!(
            kind_summary(k, &row),
            "store orders · required · tenant data"
        );
        assert!(validate_row(k, Op::Add, &json!({"label": "x", "store_key": " "})).is_err());
        assert!(validate_row(k, Op::Add, &json!({"label": "x", "weight": 1})).is_err());

        let legacy =
            fields(json!({"label": "policy.shipping", "summary": "store faq-policies · required"}));
        assert_eq!(legacy.required_flag(), Some(true));
        assert_eq!(legacy.store().as_deref(), Some("faq-policies"));
        assert_eq!(legacy.description_text(), "store faq-policies · required");
        let optional = fields(json!({"label": "p", "summary": "store faq-policies · optional"}));
        assert_eq!(optional.required_flag(), Some(false));
        let bare = fields(json!({"label": "p"}));
        assert_eq!(bare.required_flag(), None);
        assert_eq!(bare.description_text(), "p");
        // the field wins over the summary
        let explicit = fields(json!({"label": "p", "summary": "required", "required": false}));
        assert_eq!(explicit.required_flag(), Some(false));
    }

    #[test]
    fn declarations_cover_pack_references() {
        assert!(declares("support", "faq", "support", "faq"));
        assert!(declares("*", "faq", "support", "faq"));
        assert!(declares("support", "faq", "*", "faq"));
        assert!(declares("support", "faq", "support", "*"));
        assert!(!declares("support", "faq", "exit", "faq"));
        assert!(!declares("support", "faq", "support", "policy"));
        assert!(archetype_covers("policy.*", "policy.returns"));
        assert!(archetype_covers("faq.general", "faq.general"));
        assert!(archetype_covers("faq.general", "*"));
        assert!(!archetype_covers("policy.*", "policyx"));
        assert!(!archetype_covers("faq.general", "faq"));
    }

    #[test]
    fn the_stage_may_be_a_wildcard() {
        let k = Kind::StageKnowledgeKind;
        assert_eq!(validate_key(k, "*|policy.privacy"), Ok(()));
        assert_eq!(validate_key(k, "support|policy.privacy"), Ok(()));
        for bad in ["*|", "*", "**|x", "*|a|b", "*|x:y", "x*|y"] {
            assert!(validate_key(k, bad).is_err(), "{bad}");
        }
    }
}
