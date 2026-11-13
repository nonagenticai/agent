//! Per-language knowledge coverage (B3 T10; D68, plan §5.4 `knowledgeCoverage`, §0 R9/R10): a port of
//! `web/src/lib/tenant/coverage-D68.ts`. Coverage = the job's knowledge kinds × its served languages, counting the
//! documents of each kind's bound store in that language. A missing cell is a warning, never a block.
//!
//! R10: agent cannot list a platform pack's documents (strata's document routes are caller-owned only), so a kind
//! with no tenant document in a language but at least one listed, readable, not excluded pinned pack with cached
//! leaves (T5) has that cell `platform`:
//! not missing, not a gap, and the language is at best [`Level::Partial`]. Tenant documents count first.
//!
//! [`language_level`] is B2's hook (`design::coverage::language_level`, the languages view) and reads local state
//! only: documents are the files of the tenant's direct uploads (`knowledge_upload`, the language they were
//! uploaded in). The `knowledgeCoverage` route counts strata's documents instead (falling back to the same local
//! documents when strata_rag cannot be read).
use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::stores::{self, StoreRow};
use crate::design::JobRef;
use crate::design::coverage::Level;
use crate::error::ApiResult;

/// One knowledge kind as coverage sees it.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CoverageKind {
    pub stage: String,
    pub kind: String,
    pub required: bool,
    /// The bound store path (`store_path`).
    pub store: Option<String>,
    /// At least one pinned pack is listed, readable, not excluded and has leaves in the cache (R10, T5).
    pub packs: bool,
}

/// One document: its id, the agent store it is in, its language.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CoverageDoc {
    pub id: String,
    pub store_id: Uuid,
    pub language: String,
}

/// `CoverageRow` (web `types/tenant.ts`) plus `platform`: the served languages only pinned packs cover (R10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CoverageRow {
    pub stage: String,
    pub kind: String,
    pub required: bool,
    pub store: Option<String>,
    pub documents: BTreeMap<String, i64>,
    pub missing: Vec<String>,
    pub platform: Vec<String>,
}

/// `KnowledgeCoverage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub job: String,
    pub languages: Vec<String>,
    pub rows: Vec<CoverageRow>,
    pub gaps: usize,
    pub unserved: BTreeMap<String, i64>,
}

impl Coverage {
    /// The response body.
    pub fn json(&self) -> Value {
        json!({
            "job": self.job,
            "languages": self.languages,
            "rows": self.rows,
            "gaps": self.gaps,
            "unserved": self.unserved,
        })
    }
}

/// Fixture `coverageOf(job, languages, kinds, stores, documents)` (+ R10 `platform` cells).
pub fn coverage_of(
    job: &str,
    languages: &[String],
    kinds: &[CoverageKind],
    stores: &[StoreRow],
    docs: &[CoverageDoc],
) -> Coverage {
    let mut unserved: BTreeMap<String, i64> = BTreeMap::new();
    let mut counted: HashSet<&str> = HashSet::new();
    let mut rows = Vec::with_capacity(kinds.len());
    for k in kinds {
        let store = k.store.as_deref().and_then(|path| {
            stores
                .iter()
                .find(|s| stores::path_of(stores, s.id).join("/") == path)
        });
        let mut per: BTreeMap<String, i64> = languages.iter().map(|l| (l.clone(), 0)).collect();
        if let Some(s) = store {
            for d in docs.iter().filter(|d| d.store_id == s.id) {
                if let Some(n) = per.get_mut(&d.language) {
                    *n += 1;
                } else if !counted.contains(d.id.as_str()) {
                    *unserved.entry(d.language.clone()).or_insert(0) += 1;
                }
                counted.insert(d.id.as_str());
            }
        }
        let uncovered: Vec<&String> = languages
            .iter()
            .filter(|l| per.get(*l).copied().unwrap_or(0) == 0)
            .collect();
        let (platform, missing): (Vec<String>, Vec<String>) = if k.packs {
            (uncovered.into_iter().cloned().collect(), Vec::new())
        } else {
            (Vec::new(), uncovered.into_iter().cloned().collect())
        };
        rows.push(CoverageRow {
            stage: k.stage.clone(),
            kind: k.kind.clone(),
            required: k.required,
            store: store.and(k.store.clone()),
            documents: per,
            missing,
            platform,
        });
    }
    let gaps = rows
        .iter()
        .filter(|r| r.required)
        .map(|r| r.missing.len())
        .sum();
    Coverage {
        job: job.to_string(),
        languages: languages.to_vec(),
        rows,
        gaps,
        unserved,
    }
}

/// Fixture `gapsByLanguage`: per served language, how many required kinds miss it.
pub fn gaps_by_language(c: &Coverage) -> BTreeMap<String, usize> {
    c.languages
        .iter()
        .map(|l| {
            let n = c
                .rows
                .iter()
                .filter(|r| r.required && r.missing.contains(l))
                .count();
            (l.clone(), n)
        })
        .collect()
}

/// Fixture `languageLevel` (+ R10): `full` when every required kind has a tenant document in `lang` (or there is no
/// required kind), `none` when no required kind has a tenant document or a pack for it, else `partial` (a
/// pack-only cell is never `full`).
pub fn level(c: &Coverage, lang: &str) -> Level {
    let req: Vec<&CoverageRow> = c.rows.iter().filter(|r| r.required).collect();
    let missing = req
        .iter()
        .filter(|r| r.missing.iter().any(|l| l == lang))
        .count();
    let platform = req
        .iter()
        .filter(|r| r.platform.iter().any(|l| l == lang))
        .count();
    if missing == 0 && platform == 0 {
        Level::Full
    } else if missing == req.len() {
        Level::None
    } else {
        Level::Partial
    }
}

/// The job's kinds for coverage (working rows = the draft), with the R10 pack flag from the cache: a pinned pack
/// covers only when it is listed, readable, not excluded and has cached leaves (T5).
pub async fn kinds_of(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<CoverageKind>> {
    Ok(sqlx::query_as(
        "SELECT k.stage, k.kind, k.required, k.store_path AS store, \
                EXISTS (SELECT 1 FROM platform_pack p \
                         WHERE p.pack_key || '@' || p.version::text = ANY(k.packs) \
                           AND p.listed AND p.unreadable_at IS NULL AND p.excluded_by IS NULL \
                           AND EXISTS (SELECT 1 FROM platform_pack_leaf l \
                                        WHERE l.pack_key = p.pack_key AND l.version = p.version)) AS packs \
           FROM job_knowledge_kind k WHERE k.job_id = $1 ORDER BY k.stage, k.kind",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?)
}

/// The tenant's locally known documents: one per uploaded file without an error, in the upload's language.
pub async fn upload_docs(c: &mut PgConnection) -> ApiResult<Vec<CoverageDoc>> {
    Ok(sqlx::query_as(
        "SELECT u.id::text || ':' || f.ord::text AS id, u.store_id, u.language \
           FROM knowledge_upload u \
          CROSS JOIN LATERAL jsonb_array_elements(u.files) WITH ORDINALITY AS f(file, ord) \
          WHERE coalesce(f.file -> 'error', 'null'::jsonb) = 'null'::jsonb",
    )
    .fetch_all(&mut *c)
    .await?)
}

/// D110 (KH T8): the languages tenant leaf `store` holds documents in, from local state: the languages of its uploads
/// with at least one file without an error ([`upload_docs`]) and of the sources (any job's, enabled or not: a disabled
/// source's documents stay in the store) with at least one completed run that produced documents (`source_run`
/// `completed`, `documents > 0`, not `documents_removed`). A source that never ran, or ran empty, feeds nothing.
/// Languages compare as [`coverage_of`] compares them: the tag as stored, exactly (`en-GB` is not `en`). Empty = the
/// leaf is fed by nothing.
pub async fn leaf_languages(c: &mut PgConnection, store: Uuid) -> ApiResult<BTreeSet<String>> {
    let langs: Vec<String> = sqlx::query_scalar(
        "SELECT u.language FROM knowledge_upload u \
          WHERE u.store_id = $1 \
            AND EXISTS (SELECT 1 FROM jsonb_array_elements(u.files) AS f(file) \
                         WHERE coalesce(f.file -> 'error', 'null'::jsonb) = 'null'::jsonb) \
         UNION \
         SELECT s.language FROM source s \
          WHERE s.target_store_id = $1 \
            AND EXISTS (SELECT 1 FROM source_run r \
                         WHERE r.source_id = s.id AND r.status = 'completed' AND r.documents > 0 \
                           AND NOT r.documents_removed)",
    )
    .bind(store)
    .fetch_all(&mut *c)
    .await?;
    Ok(langs.into_iter().collect())
}

/// B2's `design::coverage::language_level` (local state only): the job's draft kinds × the tenant's uploaded
/// documents in `lang`, with R10 pack cells.
pub async fn language_level(c: &mut PgConnection, job: &JobRef, lang: &str) -> ApiResult<Level> {
    let kinds = kinds_of(&mut *c, job.id).await?;
    if !kinds.iter().any(|k| k.required) {
        return Ok(Level::Full);
    }
    let all = stores::all(&mut *c).await?;
    let docs = upload_docs(&mut *c).await?;
    let cov = coverage_of(&job.key, &[lang.to_string()], &kinds, &all, &docs);
    Ok(level(&cov, lang))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn store(id: u128, key: &str) -> StoreRow {
        StoreRow {
            id: Uuid::from_u128(id),
            key: key.into(),
            label: key.into(),
            parent_id: None,
            kind: "leaf".into(),
            areas: vec![],
            strata_store_id: None,
            region: "eu".into(),
            updated_at: Utc::now(),
            ensuring: false,
        }
    }

    fn kind(k: &str, s: Option<&str>, required: bool, packs: bool) -> CoverageKind {
        CoverageKind {
            stage: "support".into(),
            kind: k.into(),
            required,
            store: s.map(str::to_string),
            packs,
        }
    }

    fn doc(id: &str, store: u128, language: &str) -> CoverageDoc {
        CoverageDoc {
            id: id.into(),
            store_id: Uuid::from_u128(store),
            language: language.into(),
        }
    }

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    /// The vitest case of `tests/unit/tenant-T9-ingest.test.ts` "counts documents per kind × served language".
    #[test]
    fn matches_the_fixture_case() {
        let c = coverage_of(
            "j",
            &langs(&["nl", "en"]),
            &[
                kind("a", Some("sa"), true, false),
                kind("b", None, true, false),
                kind("c", Some("sc"), false, false),
            ],
            &[store(1, "sa"), store(2, "sc")],
            &[
                doc("d1", 1, "nl"),
                doc("d2", 1, "nl"),
                doc("d3", 1, "de"),
                doc("d4", 2, "en"),
            ],
        );
        #[allow(clippy::type_complexity)]
        let got: Vec<(&str, Vec<(&str, i64)>, Vec<&str>)> = c
            .rows
            .iter()
            .map(|r| {
                (
                    r.kind.as_str(),
                    r.documents.iter().map(|(k, n)| (k.as_str(), *n)).collect(),
                    r.missing.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a", vec![("en", 0), ("nl", 2)], vec!["en"]),
                ("b", vec![("en", 0), ("nl", 0)], vec!["nl", "en"]),
                ("c", vec![("en", 1), ("nl", 0)], vec!["nl"]),
            ]
        );
        assert_eq!(c.gaps, 3);
        assert_eq!(c.unserved, BTreeMap::from([("de".to_string(), 1)]));
        assert_eq!(
            gaps_by_language(&c),
            BTreeMap::from([("nl".to_string(), 1), ("en".to_string(), 2)])
        );
        assert_eq!(level(&c, "nl"), Level::Partial);
        assert_eq!(level(&c, "en"), Level::None);
        assert_eq!(c.rows[1].store, None, "an unbound kind has no store");
    }

    #[test]
    fn pack_only_cells_are_platform_and_partial() {
        let c = coverage_of(
            "j",
            &langs(&["nl"]),
            &[kind("a", None, true, true)],
            &[],
            &[],
        );
        assert_eq!(c.rows[0].platform, ["nl"]);
        assert!(c.rows[0].missing.is_empty());
        assert_eq!(c.gaps, 0);
        assert_eq!(level(&c, "nl"), Level::Partial);
        // tenant documents count first
        let c = coverage_of(
            "j",
            &langs(&["nl"]),
            &[kind("a", Some("sa"), true, true)],
            &[store(1, "sa")],
            &[doc("d1", 1, "nl")],
        );
        assert!(c.rows[0].platform.is_empty());
        assert_eq!(level(&c, "nl"), Level::Full);
        // no required kind: full (fixture parity)
        let c = coverage_of(
            "j",
            &langs(&["nl"]),
            &[kind("a", None, false, false)],
            &[],
            &[],
        );
        assert_eq!(level(&c, "nl"), Level::Full);
    }
}
