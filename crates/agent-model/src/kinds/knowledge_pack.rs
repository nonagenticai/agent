//! `knowledge_pack` (plan PK-1): a template layer's reference to a strata platform pack `pack_key@version` for the
//! knowledge kinds of a stage. Key `<stage>|<kind>|<pack_key>`, fields `{version}` (plus an optional `replaces`,
//! D112) and nothing else; a `tombstone` (`{}` or `{reason}`) removes an inherited reference. `stage` and `kind`
//! may be `*`, because base and industry layers own no stages: `*|*` (every kind of every stage), `<stage>|*` (every
//! kind of one stage), `*|<kind>` (one kind at every stage: a base pack for a base `*|<kind>` kind, D107, KH T3)
//! or `<stage>|<kind>` (one kind at one stage). Precedence per pack key (PK-1, widened by KH T3): exact beats
//! `*|<kind>` beats `<stage>|*` beats `*|*`.
//!
//! The parts follow agent-api's CHECKs: stage and kind as `job_knowledge_kind` (0054: no `:`, `|` or whitespace,
//! at most 64 / 128 characters), the pack key as `platform_pack_key_check` (one to six `/`-separated
//! `[a-z0-9_-]+` segments), the version as `knowledge_pack_pins_valid` (1..=999999999). The whole key is at most
//! 512 characters, so it also fits a `job_overlay_row` key (PK-2's job-level tombstone uses the same key).

use serde::{Deserialize, Serialize};

use super::{FieldError, done};

/// The highest pack version (0054 `knowledge_pack_pins_valid`: `[1-9][0-9]{0,8}`).
pub const MAX_VERSION: i64 = 999_999_999;

/// The longest key: a `job_overlay_row` key (0047 `job_overlay_row_key_check`).
pub const MAX_KEY_LEN: usize = 512;

/// The wildcard stage or kind.
pub const ANY: &str = "*";

/// Fields of a `knowledge_pack` row. `replaces` (D112, KH T3): the pack key this pack was renamed from by an
/// upgrade; a job's tombstone of the old key follows it (absent when unset, so older rows hash as before).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgePackFields {
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

/// A pack key: one to six `/`-separated segments of `[a-z0-9_-]+` (`base`, `industry/retail`,
/// `industry_dept/retail/hr`).
pub fn is_pack_key(s: &str) -> bool {
    s.split('/').count() <= 6
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        })
}

/// A stage or kind part as the 0054 CHECKs take it: 1..=`max` characters, no `:`, `|` or whitespace.
fn is_part(s: &str, max: usize) -> bool {
    let n = s.chars().count();
    (1..=max).contains(&n) && !s.chars().any(|c| c == ':' || c == '|' || c.is_whitespace())
}

/// `(stage, kind, pack_key)` of a `knowledge_pack` key, or `None` when it is not one.
pub fn split_key(key: &str) -> Option<(&str, &str, &str)> {
    if key.len() > MAX_KEY_LEN {
        return None;
    }
    let mut parts = key.split('|');
    let (stage, kind, pack) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let stage_ok = stage == ANY || is_part(stage, 64);
    let kind_ok = kind == ANY || is_part(kind, 128);
    (stage_ok && kind_ok && is_pack_key(pack)).then_some((stage, kind, pack))
}

pub(crate) fn check(f: &KnowledgePackFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    if !(1..=MAX_VERSION).contains(&f.version) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("version"),
            format!("must be an integer from 1 to {MAX_VERSION}"),
        ));
    }
    if let Some(old) = &f.replaces
        && !is_pack_key(old)
    {
        errors.push(FieldError::new(
            "invalid_value",
            Some("replaces"),
            "must be a pack key (one to six `/`-separated segments of a-z, 0-9, `_`, `-`)",
        ));
    }
    done(errors)
}

/// `<pack_key>@v<version> → <stage>/<kind>`; without a version (a tombstone) `<pack_key> → <stage>/<kind>`; the
/// key itself when it is not a `knowledge_pack` key.
pub(crate) fn label(key: &str, version: Option<i64>) -> String {
    match split_key(key) {
        Some((stage, kind, pack)) => match version {
            Some(v) => format!("{pack}@v{v} → {stage}/{kind}"),
            None => format!("{pack} → {stage}/{kind}"),
        },
        None => key.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, Op, label as kind_label, summary, validate_key, validate_row};
    use super::*;
    use crate::layer::Scope;
    use serde_json::json;

    fn codes(r: Result<(), Vec<FieldError>>) -> Vec<&'static str> {
        r.err().unwrap_or_default().iter().map(|e| e.code).collect()
    }

    #[test]
    fn kind_basics() {
        let k = Kind::KnowledgePack;
        assert_eq!(k.as_str(), "knowledge_pack");
        assert_eq!(Kind::parse("knowledge_pack"), Some(k));
        assert!(!k.is_overlay());
        assert!(!k.is_job_content());
        assert!(!k.is_journey());
        assert_eq!(
            k.scopes(),
            [
                Scope::Base,
                Scope::Industry,
                Scope::BaseDept,
                Scope::IndustryDept,
                Scope::Job
            ]
        );
        assert!(!k.allowed_in(Scope::Profile));
    }

    #[test]
    fn keys() {
        let ok = [
            "*|*|base",
            "support|*|industry/retail",
            "support|tariff.sheet|industry_dept/retail/hr",
            "support|faq|a/b/c/d/e/f",
            "s-1|k_2|job/store_manager",
            // KH T3 (D107): one kind at every stage
            "*|faq|base",
        ];
        for key in ok {
            assert_eq!(validate_key(Kind::KnowledgePack, key), Ok(()), "{key}");
        }
        let long_stage = format!("{}|faq|base", "s".repeat(65));
        let long_kind = format!("support|{}|base", "k".repeat(129));
        let long_key = format!("support|faq|{}", "p".repeat(MAX_KEY_LEN));
        let bad = [
            "",
            "base",
            "support|base",
            "support|faq|base|x",
            // 0054 stage/kind CHECKs
            "sup port|faq|base",
            "support|fa q|base",
            "sup:port|faq|base",
            "support|faq:x|base",
            "|faq|base",
            "support||base",
            long_stage.as_str(),
            long_kind.as_str(),
            // pack key shape
            "support|faq|Base",
            "support|faq|industry/",
            "support|faq|/base",
            "support|faq|a/b/c/d/e/f/g",
            "support|faq|base@1",
            "support|faq|",
            long_key.as_str(),
        ];
        for key in bad {
            assert_eq!(
                validate_key(Kind::KnowledgePack, key).err().map(|e| e.code),
                Some("invalid_key"),
                "{key:?}"
            );
        }
        // the limits themselves are allowed
        let max_stage = format!("{}|{}|base", "s".repeat(64), "k".repeat(128));
        assert_eq!(validate_key(Kind::KnowledgePack, &max_stage), Ok(()));
        assert_eq!(
            split_key("support|*|industry/retail"),
            Some(("support", "*", "industry/retail"))
        );
    }

    #[test]
    fn fields_are_only_a_version() {
        let k = Kind::KnowledgePack;
        assert_eq!(validate_row(k, Op::Add, &json!({"version": 1})), Ok(()));
        assert_eq!(
            validate_row(k, Op::Patch, &json!({"version": MAX_VERSION})),
            Ok(())
        );
        for bad in [
            json!({"version": 0}),
            json!({"version": MAX_VERSION + 1}),
            json!({"version": -3}),
        ] {
            assert_eq!(
                codes(validate_row(k, Op::Add, &bad)),
                vec!["invalid_value"],
                "{bad}"
            );
        }
        assert_eq!(
            codes(validate_row(k, Op::Add, &json!({"version": 1.5}))),
            vec!["invalid_value"]
        );
        assert_eq!(
            codes(validate_row(k, Op::Add, &json!({"version": "1"}))),
            vec!["invalid_value"]
        );
        assert_eq!(
            codes(validate_row(k, Op::Add, &json!({}))),
            vec!["missing_field"]
        );
        assert_eq!(
            codes(validate_row(
                k,
                Op::Add,
                &json!({"version": 1, "summary": "x"})
            )),
            vec!["unknown_field"]
        );
        assert_eq!(
            codes(validate_row(
                k,
                Op::Add,
                &json!({"version": 1, "root_id": "x"})
            )),
            vec!["unknown_field"]
        );
        // D112: `replaces` names the pack key an upgrade renamed this pack from
        assert_eq!(
            validate_row(
                k,
                Op::Add,
                &json!({"version": 2, "replaces": "industry/retail"})
            ),
            Ok(())
        );
        assert_eq!(
            codes(validate_row(
                k,
                Op::Add,
                &json!({"version": 2, "replaces": "Bad Key"})
            )),
            vec!["invalid_value"]
        );
        // a tombstone takes `{}` (or the generic optional reason) and nothing else
        assert_eq!(validate_row(k, Op::Tombstone, &json!({})), Ok(()));
        assert_eq!(
            codes(validate_row(k, Op::Tombstone, &json!({"version": 1}))),
            vec!["fields_on_tombstone"]
        );
    }

    #[test]
    fn label_and_summary() {
        let k = Kind::KnowledgePack;
        assert_eq!(
            kind_label(k, "support|faq|industry/retail", &json!({"version": 2})),
            "industry/retail@v2 → support/faq"
        );
        assert_eq!(
            kind_label(k, "*|*|base", &json!({"version": 1})),
            "base@v1 → */*"
        );
        assert_eq!(
            kind_label(k, "support|*|base", &json!({})),
            "base → support/*"
        );
        // no summary: the stored fields stay `{version}` (write_revision drops an empty summary)
        assert_eq!(summary(k, &json!({"version": 2})), "");
    }
}
