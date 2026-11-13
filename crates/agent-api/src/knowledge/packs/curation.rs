//! Platform knowledge curation over strata on the platform plane (plan PK-9; task T8). The HTTP surface is
//! crate::routes::platform_knowledge; this module holds what it decides: the platform plane, the shape checks the
//! agent runs before strata (strata re-checks everything, and owns the licence allow-list), and the one platform
//! transaction that references a pack from a template layer and tags the family.
//!
//! The platform plane is the `AGENT_PLATFORM_TENANT_ID` pair (the strata platform owner, handoff item 6). It needs no
//! agent `tenants` row: `strata::gate` reads only the token map and the tenant-scoped provisioning state, which is
//! empty for an id that was never provisioned (PK Q2, task-T8-report.md).
use agent_model::kinds::{Kind, Op};
use reqwest::Url;
use serde_json::{Value, json};
use std::net::IpAddr;
use uuid::Uuid;

use crate::audit::{self, Actor, PlatformEvent};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::{kinds, topic_keys};
use crate::netguard;
use crate::platform::db::{layers, lock_family, rows, tags};
use crate::platform::hooks::GeneratorGuard;
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::strata::rag::{PlatformPack, Provenance};
use crate::strata::{self, Plane, Service};
use crate::tenancy::begin_scoped;

/// At most this many topic keys per document (strata's limit).
pub const MAX_TOPIC_KEYS: usize = topic_keys::MAX_KEYS;
/// The longest topic key (strata's limit).
pub const MAX_TOPIC_KEY_CHARS: usize = topic_keys::MAX_KEY_CHARS;
/// The longest licence id the agent passes on (strata owns the allow-list).
pub const MAX_LICENSE_CHARS: usize = 64;
/// The longest provenance source URL.
pub const MAX_URL_CHARS: usize = 2048;
/// The highest pack version (0054 `knowledge_pack_pins_valid`).
pub const MAX_PACK_VERSION: i64 = 999_999_999;
/// At most this many URLs per strata_ingest job request (strata_ingest's limit).
pub const MAX_URLS: usize = 50;

/// 503 `platform_tenant_unset`: curation needs `AGENT_PLATFORM_TENANT_ID`.
pub fn platform_tenant_unset() -> ApiError {
    ApiError::Unavailable(
        "platform_tenant_unset",
        "Platform knowledge curation needs AGENT_PLATFORM_TENANT_ID (the strata platform owner)."
            .to_string(),
        None,
    )
}

/// The platform owner's plane of `service`, acting as `sub` (`X-Actor-Id` on strata_rag): 503
/// `platform_tenant_unset` without `AGENT_PLATFORM_TENANT_ID`, else `strata::gate` in a short scoped transaction
/// committed before any strata call.
pub async fn platform_plane<'a>(
    st: &'a AppState,
    service: Service,
    sub: &str,
) -> ApiResult<Plane<'a>> {
    let owner = st
        .cfg
        .platform_tenant_id
        .ok_or_else(platform_tenant_unset)?;
    let mut tx = begin_scoped(&st.db, owner).await?;
    let p = strata::gate(st, &mut tx, owner, service)
        .await?
        .with_actor(sub);
    tx.commit().await?;
    Ok(p)
}

/// Record one platform audit row in its own transaction.
pub async fn record(st: &AppState, actor: &Actor, e: PlatformEvent) -> ApiResult<()> {
    audit::record_platform_alone(&st.db, actor, e).await?;
    Ok(())
}

fn invalid(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::Unprocessable(code, message.into())
}

/// A topic key: 1..=128 characters of `[a-z0-9_./-]` (strata_rag / strata_ingest; crate::knowledge::topic_keys).
pub fn valid_topic_key(k: &str) -> bool {
    topic_keys::valid(k)
}

/// The topic keys of a platform document, trimmed, sorted and de-duplicated: 1..=32 valid keys (the shared
/// validator, crate::knowledge::topic_keys), else 422 `invalid_topic_keys` (handoff item 6: every platform document
/// needs them).
pub fn check_topic_keys(keys: &[String]) -> ApiResult<Vec<String>> {
    let out = topic_keys::normalize("topic_keys", keys.iter().map(String::as_str))
        .map_err(|m| invalid("invalid_topic_keys", m))?;
    if out.is_empty() {
        return Err(invalid(
            "invalid_topic_keys",
            format!("A platform document needs 1 to {MAX_TOPIC_KEYS} topic keys."),
        ));
    }
    Ok(out)
}

/// A public https URL (the shape part of `netguard::check_url`, query strings allowed: a source page may have
/// one): no credentials, no non-public address literal, no cluster / tailnet / LAN host name. Else 422 `code`.
pub fn check_public_url(raw: &str, code: &'static str) -> ApiResult<String> {
    let s = raw.trim();
    let refuse = |why: &str| {
        invalid(
            code,
            format!("The URL must be a public https:// address ({why})."),
        )
    };
    if s.is_empty() || s.len() > MAX_URL_CHARS {
        return Err(refuse("1 to 2048 characters"));
    }
    let Ok(u) = Url::parse(s) else {
        return Err(refuse("not a URL"));
    };
    if u.scheme() != "https" {
        return Err(refuse("https only"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(refuse("no credentials in the URL"));
    }
    let host = u
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err(refuse("no host"));
    }
    match host.parse::<IpAddr>() {
        Ok(ip) if !netguard::is_public(ip) => return Err(refuse("not a public address")),
        Ok(_) => {}
        Err(_) if netguard::internal_hostname(&host) => {
            return Err(refuse("not a public host name"));
        }
        Err(_) => {}
    }
    Ok(s.to_string())
}

/// An RFC 3339 timestamp, else 422 `invalid_provenance`.
pub fn check_retrieved_at(raw: &str) -> ApiResult<String> {
    let s = raw.trim();
    chrono::DateTime::parse_from_rfc3339(s).map_err(|_| {
        invalid(
            "invalid_provenance",
            "provenance.retrieved_at must be an RFC 3339 timestamp (e.g. 2026-10-21T08:00:00Z).",
        )
    })?;
    Ok(s.to_string())
}

/// A licence id: 1..=64 printable characters without whitespace. The allow-list is strata's (it answers 422).
pub fn check_license(raw: &str) -> ApiResult<String> {
    let s = raw.trim();
    let n = s.chars().count();
    if n == 0 || n > MAX_LICENSE_CHARS || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid(
            "invalid_provenance",
            format!(
                "provenance.license must be a licence id of 1 to {MAX_LICENSE_CHARS} characters (e.g. CC-BY-4.0)."
            ),
        ));
    }
    Ok(s.to_string())
}

/// `provenance {source_url, license, retrieved_at}` checked for shape (422 `invalid_provenance`).
pub fn check_provenance(p: &Provenance) -> ApiResult<Provenance> {
    Ok(Provenance {
        source_url: check_public_url(&p.source_url, "invalid_provenance")?,
        license: check_license(&p.license)?,
        retrieved_at: check_retrieved_at(&p.retrieved_at)?,
    })
}

/// A pack key (1–6 `/`-separated `[a-z0-9_-]+` segments, at most 512 characters), else 422 `invalid_pack`.
pub fn check_pack_key(raw: &str) -> ApiResult<String> {
    let k = raw.trim();
    if k.len() > 512 || !kinds::valid_pack_key(k) {
        let shown: String = k.chars().take(80).collect();
        return Err(invalid(
            "invalid_pack",
            format!("{shown:?} is not a pack key: 1 to 6 `/`-separated segments of [a-z0-9_-]."),
        ));
    }
    Ok(k.to_string())
}

/// A pack version 1..=999999999, else 422 `invalid_pack`.
pub fn check_pack_version(v: i64) -> ApiResult<i32> {
    if !(1..=MAX_PACK_VERSION).contains(&v) {
        return Err(invalid(
            "invalid_pack",
            format!("A pack version is an integer from 1 to {MAX_PACK_VERSION}."),
        ));
    }
    i32::try_from(v).map_err(|_| invalid("invalid_pack", "The pack version is out of range."))
}

/// The `knowledge_pack` row key `<stage>|<kind>|<pack_key>` (the row validator checks the parts: 422 `invalid_key`).
pub fn reference_key(stage: &str, kind: &str, pack_key: &str) -> String {
    format!("{}|{}|{pack_key}", stage.trim(), kind.trim())
}

/// The pack must be published and not retired in the owner listing: 404 `pack_not_found`, 409 `pack_unpublished`,
/// 409 `pack_retired`.
pub fn check_referenceable(owner_packs: &[PlatformPack], key: &str, version: i32) -> ApiResult<()> {
    let Some(p) = owner_packs
        .iter()
        .find(|p| p.key == key && p.version == version)
    else {
        return Err(ApiError::Missing(
            "pack_not_found",
            format!("No platform pack {key}@{version}."),
        ));
    };
    if p.published_at.is_none() {
        return Err(ApiError::Conflict(
            "pack_unpublished",
            format!("{key}@{version} is a draft; publish it before a template references it."),
        ));
    }
    if p.retired_at.is_some() {
        return Err(ApiError::Conflict(
            "pack_retired",
            format!("{key}@{version} is retired; a template cannot reference it any more."),
        ));
    }
    Ok(())
}

/// What a reference write answers: the new revision and the tag that carries it.
#[derive(Debug, Clone)]
pub struct Referenced {
    pub row: rows::Row,
    pub tag: tags::TemplateTag,
}

impl Referenced {
    /// `{row, tag: "<family>@v<N>", version}`.
    pub fn json(&self) -> Value {
        json!({
            "row": self.row,
            "tag": format!("{}@v{}", self.tag.family, self.tag.version),
            "family": self.tag.family,
            "version": self.tag.version,
        })
    }
}

/// The current (op, fields) of (`layer`, `knowledge_pack`, `key`), if a current row exists.
async fn current(
    conn: &mut sqlx::PgConnection,
    layer_id: Uuid,
    key: &str,
) -> ApiResult<Option<(String, Value)>> {
    Ok(sqlx::query_as(
        "SELECT op, fields FROM layer_row \
          WHERE layer_id = $1 AND kind = 'knowledge_pack' AND key = $2 AND is_current",
    )
    .bind(layer_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?)
}

/// One platform transaction (PK-9): write the `knowledge_pack` revision (`version = Some(v)`: add, or patch over a
/// current add; `None`: a tombstone) on `layer`, tag the layer's family, and one platform audit row. 409
/// `already_referenced` when the current row already references that version; 404 `reference_not_found` when a
/// tombstone would land on a current tombstone. The pack's published state is checked by the caller BEFORE this
/// (no strata call inside the transaction). The tag holds every current row of the family (B6-4), so other untagged
/// drafts of the family ship in it too.
pub async fn write_reference(
    st: &AppState,
    actor: &Actor,
    guard: &dyn GeneratorGuard,
    layer: &str,
    key: &str,
    version: Option<i32>,
    by: &str,
) -> ApiResult<Referenced> {
    let mut tx = begin_platform(&st.db).await?;
    let l = layers::layer_by_key(&mut tx, layer).await?;
    // the family lock first (write_revision and tag_family take it again: it is re-entrant in one transaction)
    lock_family(&mut tx, &l.family).await?;
    let now = current(&mut tx, l.id, key).await?;
    let (op, fields) = match (version, now.as_ref()) {
        (Some(v), Some((op, f))) if op != "tombstone" => {
            if f.get("version").and_then(Value::as_i64) == Some(i64::from(v)) {
                return Err(ApiError::Conflict(
                    "already_referenced",
                    format!("{layer} already references {key} at version {v}."),
                ));
            }
            (Op::Patch, json!({ "version": v }))
        }
        (Some(v), _) => (Op::Add, json!({ "version": v })),
        (None, Some((op, _))) if op == "tombstone" => {
            return Err(ApiError::Missing(
                "reference_not_found",
                format!("{layer} has no reference {key} to remove."),
            ));
        }
        (None, _) => (Op::Tombstone, json!({})),
    };
    let row = rows::write_revision(
        &mut tx,
        actor,
        layer,
        Kind::KnowledgePack,
        key,
        op,
        fields,
        by,
    )
    .await?;
    let note = match version {
        Some(v) => format!("knowledge_pack {key} → v{v} on {layer}"),
        None => format!("knowledge_pack {key} removed on {layer}"),
    };
    let tag = tags::tag_family(&mut tx, actor, &l.family, &note, by, guard, false).await?;
    let ev = match version {
        Some(v) => {
            PlatformEvent::new("platform.knowledge_pack.reference", key).detail("version", v)
        }
        None => PlatformEvent::new("platform.knowledge_pack.unreference", key),
    };
    audit::record_platform(
        &mut tx,
        actor,
        ev.detail("layer", layer)
            .detail("key", key)
            .detail("op", op.as_str())
            .detail("family", l.family.as_str())
            .detail("tag_version", tag.version),
    )
    .await?;
    tx.commit().await?;
    Ok(Referenced { row, tag })
}

/// strata_ingest's job `error.code` when a platform store refuses a job's provenance (#15: the licence is not on
/// `STRATA_PLATFORM_LICENSES`). The job fails and is not retried.
pub const PROVENANCE_REJECTED: &str = "provenance_rejected";

/// A strata_ingest job view as the curator reads it: strata's view unchanged, plus `curator_status` (strata's
/// `status`, or `licence_refused` for a [`PROVENANCE_REJECTED`] failure) and `curator_message` (what to do about a
/// refused licence; `null` otherwise). strata's own error message is kept in `error`.
pub fn curator_job(mut job: Value) -> Value {
    let refused = job["status"] == "failed" && job["error"]["code"] == PROVENANCE_REJECTED;
    let (status, message) = if refused {
        let license = job["meta"]["provenance"]["license"]
            .as_str()
            .or_else(|| job["provenance"]["license"].as_str())
            .map(|l| format!(" ({l})"))
            .unwrap_or_default();
        (
            json!("licence_refused"),
            json!(format!(
                "strata refused this document's licence{license} for platform knowledge and will not retry it. \
                 Ingest it again with a licence on strata's allow-list (STRATA_PLATFORM_LICENSES)."
            )),
        )
    } else {
        (job["status"].clone(), Value::Null)
    };
    if let Some(o) = job.as_object_mut() {
        o.insert("curator_status".into(), status);
        o.insert("curator_message".into(), message);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provenance_rejected_job_reads_licence_refused() {
        let j = curator_job(json!({"id": "j", "status": "failed",
            "error": {"code": "provenance_rejected", "message": "licence CC-BY-NC-4.0 not allowed"},
            "meta": {"provenance": {"license": "CC-BY-NC-4.0"}}}));
        assert_eq!(j["curator_status"], "licence_refused");
        let m = j["curator_message"].as_str().unwrap();
        assert!(
            m.contains("(CC-BY-NC-4.0)") && m.contains("STRATA_PLATFORM_LICENSES"),
            "{m}"
        );
        assert_eq!(
            j["error"]["message"], "licence CC-BY-NC-4.0 not allowed",
            "strata's error is kept"
        );
        // any other failure, and a queued job, keep strata's status
        let other = curator_job(json!({"status": "failed", "error": {"code": "fetch_failed"}}));
        assert_eq!(other["curator_status"], "failed");
        assert!(other["curator_message"].is_null());
        assert_eq!(
            curator_job(json!({"status": "queued"}))["curator_status"],
            "queued"
        );
    }

    fn pack(key: &str, v: i32, published: bool, retired: bool) -> PlatformPack {
        PlatformPack {
            key: key.into(),
            version: v,
            root_id: crate::strata::StrataStoreId(Uuid::nil()),
            published_at: published.then(|| "2026-10-21T00:00:00Z".to_string()),
            retired_at: retired.then(|| "2026-10-21T01:00:00Z".to_string()),
        }
    }

    #[test]
    fn topic_keys_are_required_bounded_and_charset_checked() {
        let ok = check_topic_keys(&[
            " returns.policy ".into(),
            "hr/leave".into(),
            "hr/leave".into(),
        ])
        .unwrap();
        assert_eq!(ok, ["hr/leave", "returns.policy"]);
        assert_eq!(
            check_topic_keys(&[]).unwrap_err().code(),
            "invalid_topic_keys"
        );
        assert_eq!(
            check_topic_keys(&["  ".into()]).unwrap_err().code(),
            "invalid_topic_keys"
        );
        for bad in ["Returns", "a b", "a|b", "ä"] {
            assert_eq!(
                check_topic_keys(&[bad.into()]).unwrap_err().code(),
                "invalid_topic_keys",
                "{bad:?}"
            );
        }
        assert!(check_topic_keys(&["a".repeat(MAX_TOPIC_KEY_CHARS)]).is_ok());
        assert!(check_topic_keys(&["a".repeat(MAX_TOPIC_KEY_CHARS + 1)]).is_err());
        let many: Vec<String> = (0..MAX_TOPIC_KEYS).map(|i| format!("k{i}")).collect();
        assert!(check_topic_keys(&many).is_ok());
        let too_many: Vec<String> = (0..=MAX_TOPIC_KEYS).map(|i| format!("k{i}")).collect();
        assert!(check_topic_keys(&too_many).is_err());
    }

    #[test]
    fn provenance_shape() {
        let good = Provenance {
            source_url: " https://www.gov.uk/guidance/x?y=1 ".into(),
            license: "OGL-UK-3.0".into(),
            retrieved_at: "2026-10-21T08:00:00Z".into(),
        };
        let p = check_provenance(&good).unwrap();
        assert_eq!(p.source_url, "https://www.gov.uk/guidance/x?y=1");
        for url in [
            "http://example.org/a",
            "https://user:pw@example.org/a",
            "https://10.0.0.1/a",
            "https://127.0.0.1/a",
            "https://[::1]/a",
            "https://minio/a",
            "https://postgres.internal/a",
            "https://x.svc.cluster.local/a",
            "not a url",
            "",
        ] {
            let bad = Provenance {
                source_url: url.into(),
                ..good.clone()
            };
            assert_eq!(
                check_provenance(&bad).unwrap_err().code(),
                "invalid_provenance",
                "{url:?}"
            );
        }
        for when in ["2026-10-21", "yesterday", "2026-10-21 08:00"] {
            let bad = Provenance {
                retrieved_at: when.into(),
                ..good.clone()
            };
            assert_eq!(
                check_provenance(&bad).unwrap_err().code(),
                "invalid_provenance"
            );
        }
        let long = "x".repeat(MAX_LICENSE_CHARS + 1);
        for lic in ["", "CC BY", long.as_str()] {
            let bad = Provenance {
                license: lic.to_string(),
                ..good.clone()
            };
            assert_eq!(
                check_provenance(&bad).unwrap_err().code(),
                "invalid_provenance"
            );
        }
        // the allow-list is strata's: a non-redistributable id passes the shape check
        let nc = Provenance {
            license: "CC-BY-NC-4.0".into(),
            ..good
        };
        assert!(check_provenance(&nc).is_ok());
    }

    #[test]
    fn pack_key_and_version() {
        assert_eq!(check_pack_key(" job/pk_x ").unwrap(), "job/pk_x");
        for bad in ["", "Job", "a/", "a/b/c/d/e/f/g", "a@1"] {
            assert_eq!(
                check_pack_key(bad).unwrap_err().code(),
                "invalid_pack",
                "{bad:?}"
            );
        }
        assert_eq!(check_pack_version(1).unwrap(), 1);
        assert_eq!(check_pack_version(MAX_PACK_VERSION).unwrap(), 999_999_999);
        assert!(check_pack_version(0).is_err());
        assert!(check_pack_version(MAX_PACK_VERSION + 1).is_err());
    }

    #[test]
    fn only_published_unretired_packs_are_referenceable() {
        let list = [
            pack("job/a", 1, true, false),
            pack("job/a", 2, false, false),
            pack("job/a", 3, true, true),
        ];
        assert!(check_referenceable(&list, "job/a", 1).is_ok());
        assert_eq!(
            check_referenceable(&list, "job/a", 2).unwrap_err().code(),
            "pack_unpublished"
        );
        assert_eq!(
            check_referenceable(&list, "job/a", 3).unwrap_err().code(),
            "pack_retired"
        );
        assert_eq!(
            check_referenceable(&list, "job/b", 1).unwrap_err().code(),
            "pack_not_found"
        );
    }

    #[test]
    fn reference_key_joins_the_parts() {
        assert_eq!(reference_key(" * ", "*", "job/a"), "*|*|job/a");
        assert_eq!(reference_key("support", "faq", "base"), "support|faq|base");
    }
}
