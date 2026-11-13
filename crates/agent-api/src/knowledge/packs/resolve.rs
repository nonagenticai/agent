//! Cut-time pack resolution: listing, exclusions and leaf caches, conflicts and the release snapshot (plan PK-4..PK-6;
//! task T4).
//!
//! - [`prepare_cut`] runs BEFORE the cut's transaction (one call in `routes/release/releases.rs`): it reads the draft's
//!   pins (plus, with `adopt_pins`, the `knowledge_pack` pins of the adopted families' latest tags) in a short
//!   transaction, then [`refresh_pins`]. No strata call is ever made inside a transaction (PK-4).
//! - [`refresh_pins`] (also T5's Draft search) calls strata with no transaction open: the tenant listing, the tenant's
//!   exclusions, the platform owner's listing (when `AGENT_PLATFORM_TENANT_ID` is set and its gate passes) and
//!   `…/{key}/{version}/stores` for every pin without leaf rows; then writes one short transaction: `platform_pack`
//!   (the `record_listing` semantics, plus `state` from the owner listing, kept as cached without it, and `excluded_by`) and
//!   `platform_pack_leaf` (DELETE + INSERT per fetched pin). Published leaves never change, so a cached pin is not
//!   fetched again.
//! - [`check`] (approve step 5b, cache only, in the cut's transaction): every pin is usable or a violation
//!   `knowledge:<stage>:<kind>/<pin>` naming the reason ([`Conflict`]).
//! - [`snapshot`] (approve step 10b): each pin's leaves into `release_pack_leaf` (immutable), from the leaf cache, else
//!   from the job's newest release (PK-6).
//!
//! A pin the owner listing does not know is cached as a placeholder row (`root_id` = the nil uuid, `listed = false`,
//! `state = 'missing'`): `platform_pack.root_id` is NOT NULL, and the placeholder is what tells [`check`] "missing"
//! from "not classified" (no row = `unavailable`, PK-5). A later listing that names the pack overwrites the root.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::{self, DesignOf, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::kinds::{self, PackPin};
use crate::release::bundle::BundleViolation;
use crate::state::AppState;
use crate::strata::rag::{self, Exclusion, PackStores, PlatformPack, StoreKind};
use crate::strata::{self, Plane, Service};
use crate::tenancy::begin_scoped;

/// The pins of one knowledge kind of a design (`{store, packs}` values with at least one pack).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KindPins {
    pub stage: String,
    pub kind: String,
    pub pins: Vec<String>,
}

/// Why a pin cannot be cut (PK-4/PK-5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// The tenant excluded the pack (the exclusion prefix).
    Excluded(String),
    /// The pack is a draft.
    Unpublished,
    /// The pack is retired and the job's newest release did not pin it.
    Retired,
    /// The platform has no such pack.
    Missing,
    /// Not resolved: strata did not answer, or the platform plane is not configured and the pack is not readable.
    Unavailable,
}

impl Conflict {
    /// The reason code: `excluded | unpublished | retired | missing | unavailable`.
    pub fn reason(&self) -> &'static str {
        match self {
            Conflict::Excluded(_) => "excluded",
            Conflict::Unpublished => "unpublished",
            Conflict::Retired => "retired",
            Conflict::Missing => "missing",
            Conflict::Unavailable => "unavailable",
        }
    }

    /// Plain English naming the pin and the reason.
    pub fn message(&self, pin: &str) -> String {
        match self {
            Conflict::Excluded(prefix) => format!(
                "Platform pack {pin} is excluded for this tenant (exclusion {prefix}); remove the pin or lift the exclusion."
            ),
            Conflict::Unpublished => {
                format!("Platform pack {pin} is not published yet; pin a published version.")
            }
            Conflict::Retired => format!(
                "Platform pack {pin} is retired; only a job whose last release pinned it keeps it. Pin a current version."
            ),
            Conflict::Missing => format!("Platform pack {pin} does not exist on the platform."),
            Conflict::Unavailable => format!(
                "Platform pack {pin} could not be resolved: the knowledge service did not confirm it. Try again shortly."
            ),
        }
    }
}

/// One `platform_pack` row as [`check`] reads it.
#[derive(Clone, Debug, sqlx::FromRow)]
struct CacheRow {
    pack_key: String,
    version: i32,
    listed: bool,
    state: Option<String>,
    excluded_by: Option<String>,
}

/// The verdict on one pin (PK-5/PK-6): excluded always wins; a pin of the job's newest release is kept (retired
/// included); else the owner listing's state decides, and a published or unclassified pin needs leaf rows (an
/// unclassified one must also be in the tenant listing).
fn verdict(row: Option<&CacheRow>, has_leaves: bool, in_prior: bool) -> Option<Conflict> {
    if let Some(prefix) = row.and_then(|r| r.excluded_by.as_deref()) {
        return Some(Conflict::Excluded(prefix.to_string()));
    }
    if in_prior {
        return None;
    }
    match row.and_then(|r| r.state.as_deref()) {
        Some("retired") => Some(Conflict::Retired),
        Some("draft") => Some(Conflict::Unpublished),
        Some("missing") => Some(Conflict::Missing),
        Some(_) if has_leaves => None,
        None if has_leaves && row.is_some_and(|r| r.listed) => None,
        _ => Some(Conflict::Unavailable),
    }
}

/// The owner listing's state of `p`.
fn owner_state(p: &PlatformPack) -> &'static str {
    if p.retired_at.is_some() {
        "retired"
    } else if p.published_at.is_some() {
        "published"
    } else {
        "draft"
    }
}

/// The exclusion prefix covering `key` segment-wise (`industry/retail` covers `industry/retail/hr`, not
/// `industry/retailx`): the shortest, then the first by name; `None` when none does.
fn excluded_by(key: &str, list: &[Exclusion]) -> Option<String> {
    list.iter()
        .map(|x| x.key_prefix.as_str())
        .filter(|p| kinds::valid_pack_key(p))
        .filter(|p| {
            key == *p
                || key
                    .strip_prefix(p)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .min_by_key(|p| (p.len(), *p))
        .map(str::to_string)
}

fn distinct_pins(by_kind: &[KindPins]) -> Vec<String> {
    by_kind
        .iter()
        .flat_map(|k| k.pins.iter().cloned())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Reads

/// The pins of every knowledge kind in the job's draft (`design::values_of_kind(Draft, "knowledge")`), by stage then
/// kind; kinds without packs are left out.
pub async fn draft_pins(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<KindPins>> {
    let rows = design::values_of_kind(&mut *c, reg, job, &DesignOf::Draft, kinds::KIND).await?;
    let mut out: Vec<KindPins> = Vec::new();
    for r in rows {
        let Some(v) = r.snap.as_ref() else {
            continue;
        };
        let (_, pins) = kinds::value_parts(v)?;
        if pins.is_empty() {
            continue;
        }
        let part = |f: &str| {
            r.r#ref
                .get(f)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        out.push(KindPins {
            stage: part("stage"),
            kind: part("kind"),
            pins,
        });
    }
    out.sort();
    Ok(out)
}

/// The `knowledge_pack` pins (`<pack_key>@<fields.version>`, tombstones left out) of the latest non-provisional tag
/// of each family in `families`: a superset of what an adopt can re-pin, read without T3's fold.
async fn adopted_pack_pins(c: &mut PgConnection, families: &[String]) -> ApiResult<Vec<String>> {
    let raw: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT DISTINCT split_part(r.key, '|', 3) || '@' || (r.fields ->> 'version') \
           FROM layer_tag_row m JOIN layer_row r ON r.id = m.row_id \
          WHERE m.family = ANY($1::text[]) \
            AND m.version = (SELECT max(t.version) FROM layer_tag t \
                              WHERE t.family = m.family AND NOT t.provisional) \
            AND r.kind = 'knowledge_pack' AND r.op <> 'tombstone' AND r.fields ? 'version'",
    )
    .bind(families)
    .fetch_all(&mut *c)
    .await?;
    Ok(raw
        .into_iter()
        .flatten()
        .filter(|p| kinds::parse_pin(p).is_some())
        .collect())
}

/// The pins of `pins` that have `platform_pack_leaf` rows (scoped tenant).
async fn leaf_pins(c: &mut PgConnection, pins: &[String]) -> ApiResult<BTreeSet<String>> {
    let v: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pack_key || '@' || version::text FROM platform_pack_leaf \
          WHERE pack_key || '@' || version::text = ANY($1::text[])",
    )
    .bind(pins)
    .fetch_all(&mut *c)
    .await?;
    Ok(v.into_iter().collect())
}

/// The newest release of `job` other than `except` (`""` = none excepted).
async fn newest_release(
    c: &mut PgConnection,
    job: Uuid,
    except: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT release_id FROM job_release WHERE job_id = $1 AND release_id <> $2 \
          ORDER BY approved_at DESC, release_id DESC LIMIT 1",
    )
    .bind(job)
    .bind(except)
    .fetch_optional(&mut *c)
    .await?)
}

/// The pins of `pins` that release `release` of `job` snapshotted.
async fn release_pins(
    c: &mut PgConnection,
    job: Uuid,
    release: &str,
    pins: &[String],
) -> ApiResult<BTreeSet<String>> {
    let v: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pin FROM release_pack_leaf \
          WHERE job_id = $1 AND release_id = $2 AND pin = ANY($3::text[])",
    )
    .bind(job)
    .bind(release)
    .bind(pins)
    .fetch_all(&mut *c)
    .await?;
    Ok(v.into_iter().collect())
}

/// The pins of `pins` the job's newest release snapshotted (PK-6).
async fn prior_pins(
    c: &mut PgConnection,
    job: Uuid,
    pins: &[String],
) -> ApiResult<BTreeSet<String>> {
    match newest_release(&mut *c, job, "").await? {
        Some(r) => release_pins(&mut *c, job, &r, pins).await,
        None => Ok(BTreeSet::new()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Before the cut (no transaction across strata)

/// Pack resolution is off for this deployment: no strata_rag URL is configured, so no pin can be resolved, snapshotted
/// or searched. A cut then makes no strata call and neither checks nor snapshots pins (they stay inert design values;
/// a release search degrades them). With strata configured, a pin strata cannot confirm is refused (`unavailable`).
pub fn resolution_off(st: &AppState) -> bool {
    st.cfg.strata.rag_url.is_none()
}

/// Prepare the caches a cut of `job` checks (PK-4): the draft's pins, plus with a non-empty `adopt` the
/// `knowledge_pack` pins of the adopted families' latest tags, refreshed by [`refresh_pins`]. No pins = no strata
/// call. When strata fails and every pin already has leaf rows (or the job's newest release snapshotted it), the cut
/// goes on from the cache (published leaves never change; logged); otherwise the error is returned (503
/// `unavailable` when strata is down).
pub async fn prepare_cut(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    adopt: Option<&[String]>,
) -> ApiResult<()> {
    if resolution_off(st) {
        return Ok(());
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let mut pins: BTreeSet<String> = distinct_pins(&draft_pins(&mut tx, &st.design, job).await?)
        .into_iter()
        .collect();
    if let Some(families) = adopt.filter(|a| !a.is_empty()) {
        pins.extend(adopted_pack_pins(&mut tx, families).await?);
    }
    tx.commit().await?;
    if pins.is_empty() {
        return Ok(());
    }
    let pins: Vec<String> = pins.into_iter().collect();
    let Err(e) = refresh_pins(st, tenant, &pins).await else {
        return Ok(());
    };
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let cached = leaf_pins(&mut tx, &pins).await?;
    let prior = prior_pins(&mut tx, job.id, &pins).await?;
    tx.commit().await?;
    if pins.iter().all(|p| cached.contains(p) || prior.contains(p)) {
        tracing::warn!(
            tenant = %tenant,
            job = %job.key,
            code = e.code(),
            "knowledge_pack_refresh_failed_cut_uses_cache"
        );
        return Ok(());
    }
    Err(e)
}

/// Warm the draft's pack caches for the staff test chat (env `test`), from the app side: the runtime (`agent_runtime`)
/// may only READ `platform_pack` / `platform_pack_leaf` (0128), so before the first cut nothing else fills them and a
/// knowledge question in the test chat found its pins `unresolved`. Called as a test conversation starts and on each
/// test message; a read of the draft's pins, and a [`refresh_pins`] only for pins without cached leaves. Best effort:
/// a failure is logged and the turn runs on (the runtime then searches the tenant's own stores and names the packs it
/// could not search). Resolution off = nothing to do.
pub async fn warm_draft(st: &AppState, tenant: Uuid, job: &JobRef) {
    if resolution_off(st) {
        return;
    }
    let res: ApiResult<()> = async {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        let pins = distinct_pins(&draft_pins(&mut tx, &st.design, job).await?);
        let cached = leaf_pins(&mut tx, &pins).await?;
        tx.commit().await?;
        let cold: Vec<String> = pins.into_iter().filter(|p| !cached.contains(p)).collect();
        if cold.is_empty() {
            return Ok(());
        }
        refresh_pins(st, tenant, &cold).await
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(tenant = %tenant, job = %job.key, code = e.code(), "knowledge_draft_warm_failed");
    }
}

/// The platform owner's rag plane: `AGENT_PLATFORM_TENANT_ID` set and its gate passing, else `None` (logged).
async fn owner_plane(st: &AppState) -> Option<Plane<'_>> {
    let owner = st.cfg.platform_tenant_id?;
    let res: ApiResult<Plane<'_>> = async {
        let mut tx = begin_scoped(&st.db, owner).await?;
        let p = strata::gate(st, &mut tx, owner, Service::Rag).await?;
        tx.commit().await?;
        Ok(p)
    }
    .await;
    match res {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(code = e.code(), "platform_owner_plane_unavailable");
            None
        }
    }
}

/// Refresh the tenant's pack caches for `pins` (PK-4, PK-5): the strata calls first with no transaction open (tenant
/// listing, exclusions, the owner listing when available, `/stores` for each pin without leaf rows), then one short
/// transaction writing `platform_pack` and `platform_pack_leaf`. A strata error is returned and nothing is written.
pub async fn refresh_pins(st: &AppState, tenant: Uuid, pins: &[String]) -> ApiResult<()> {
    let wanted: Vec<PackPin> = pins
        .iter()
        .filter_map(|p| kinds::parse_pin(p))
        .collect::<BTreeSet<PackPin>>()
        .into_iter()
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let all: Vec<String> = wanted.iter().map(PackPin::pin).collect();
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let p = strata::gate(st, &mut tx, tenant, Service::Rag).await?;
    let cached = leaf_pins(&mut tx, &all).await?;
    tx.commit().await?;
    let listing = rag::platform_packs(&st.http, &p).await?;
    let exclusions = rag::exclusions(&st.http, &p).await?;
    let owner = match owner_plane(st).await {
        Some(op) => match rag::platform_packs(&st.http, &op).await {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(code = e.code(), "platform_owner_listing_unavailable");
                None
            }
        },
        None => None,
    };
    let mut fetched: Vec<(&PackPin, Option<PackStores>)> = Vec::new();
    for pin in &wanted {
        if cached.contains(&pin.pin()) {
            continue;
        }
        let stores = rag::pack_stores(&st.http, &p, &pin.key, pin.version).await?;
        fetched.push((pin, stores));
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    write_cache(
        &mut tx,
        tenant,
        &wanted,
        &listing,
        owner.as_deref(),
        &exclusions,
    )
    .await?;
    for (pin, stores) in &fetched {
        write_leaves(&mut tx, tenant, pin, stores.as_ref()).await?;
    }
    tx.commit().await?;
    Ok(())
}

fn published_at(p: &PlatformPack) -> Option<DateTime<Utc>> {
    p.published_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// `platform_pack`: the tenant listing as `kinds::record_listing` writes it; the wanted pins only the owner lists
/// (unlisted) or that it does not know (a `missing` placeholder); then every cached row's `state` (from the owner
/// listing; left as cached without it) and `excluded_by`.
async fn write_cache(
    c: &mut PgConnection,
    tenant: Uuid,
    wanted: &[PackPin],
    listing: &[PlatformPack],
    owner: Option<&[PlatformPack]>,
    exclusions: &[Exclusion],
) -> ApiResult<()> {
    kinds::record_listing(&mut *c, tenant, listing).await?;
    let listed: BTreeSet<String> = kinds::pinnable(listing)
        .map(|p| format!("{}@{}", p.key, p.version))
        .collect();
    if let Some(owner) = owner {
        for pin in wanted.iter().filter(|w| !listed.contains(&w.pin())) {
            match owner
                .iter()
                .find(|o| o.key == pin.key && o.version == pin.version)
            {
                Some(o) => {
                    sqlx::query(
                        "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, published_at, listed, \
                                                    checked_at) \
                         VALUES ($1, $2, $3, $4, $5, false, clock_timestamp()) \
                         ON CONFLICT (tenant_id, pack_key, version) DO UPDATE \
                            SET root_id = EXCLUDED.root_id, published_at = EXCLUDED.published_at, \
                                checked_at = EXCLUDED.checked_at",
                    )
                    .bind(tenant)
                    .bind(&o.key)
                    .bind(o.version)
                    .bind(o.root_id.0)
                    .bind(published_at(o))
                    .execute(&mut *c)
                    .await?;
                }
                None => {
                    sqlx::query(
                        "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, published_at, listed, \
                                                    checked_at) \
                         VALUES ($1, $2, $3, $4, NULL, false, clock_timestamp()) \
                         ON CONFLICT (tenant_id, pack_key, version) DO NOTHING",
                    )
                    .bind(tenant)
                    .bind(&pin.key)
                    .bind(pin.version)
                    .bind(Uuid::nil())
                    .execute(&mut *c)
                    .await?;
                }
            }
        }
    }
    let rows: Vec<(String, i32)> =
        sqlx::query_as("SELECT pack_key, version FROM platform_pack ORDER BY pack_key, version")
            .fetch_all(&mut *c)
            .await?;
    for (key, version) in rows {
        // Without the owner listing (no owner configured, or its plane/listing failed) the cached `state` stays as
        // the last owner listing left it: a retired or draft pin keeps its real reason instead of collapsing to
        // "unavailable". A row the owner never classified keeps its NULL (→ unavailable unless listed + leaves).
        let state: Option<&str> = owner.map(|o| {
            o.iter()
                .find(|p| p.key == key && p.version == version)
                .map_or("missing", owner_state)
        });
        sqlx::query(
            "UPDATE platform_pack SET state = CASE WHEN $5 THEN $3 ELSE state END, excluded_by = $4 \
              WHERE pack_key = $1 AND version = $2 \
                AND (($5 AND state IS DISTINCT FROM $3) OR excluded_by IS DISTINCT FROM $4)",
        )
        .bind(&key)
        .bind(version)
        .bind(state)
        .bind(excluded_by(&key, exclusions))
        .bind(owner.is_some())
        .execute(&mut *c)
        .await?;
    }
    Ok(())
}

/// `platform_pack_leaf` of `pin`: DELETE, then one row per leaf of `stores` (`None` = not readable: no rows).
async fn write_leaves(
    c: &mut PgConnection,
    tenant: Uuid,
    pin: &PackPin,
    stores: Option<&PackStores>,
) -> ApiResult<()> {
    sqlx::query("DELETE FROM platform_pack_leaf WHERE pack_key = $1 AND version = $2")
        .bind(&pin.key)
        .bind(pin.version)
        .execute(&mut *c)
        .await?;
    let Some(s) = stores else {
        return Ok(());
    };
    for l in s.stores.iter().filter(|x| x.kind == StoreKind::Leaf) {
        let embed: Option<String> = (!l.embed_model.is_empty()).then(|| l.embed_model.clone());
        sqlx::query(
            "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, parent_id, path, embed_model, \
                                             chunk_count, checked_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp()) \
             ON CONFLICT (tenant_id, pack_key, version, leaf_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(&pin.key)
        .bind(pin.version)
        .bind(l.id.0)
        .bind(l.parent_id.map(|x| x.0))
        .bind(&l.path)
        .bind(embed)
        .bind(l.chunk_count.max(0))
        .execute(&mut *c)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// In the cut (cache only)

/// Approve step 5b: every pin of `pins_by_kind` checked against the cache ([`verdict`]); one violation
/// `{path: "knowledge:<stage>:<kind>/<pin>", message}` per unusable pin, by path. Empty = the cut may go on.
pub async fn check(
    c: &mut PgConnection,
    job: &JobRef,
    pins_by_kind: &[KindPins],
) -> ApiResult<Vec<BundleViolation>> {
    let all = distinct_pins(pins_by_kind);
    if all.is_empty() {
        return Ok(Vec::new());
    }
    let cache: BTreeMap<String, CacheRow> = sqlx::query_as::<_, CacheRow>(
        "SELECT pack_key, version, listed, state, excluded_by FROM platform_pack \
          WHERE pack_key || '@' || version::text = ANY($1::text[])",
    )
    .bind(&all)
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(|r| (format!("{}@{}", r.pack_key, r.version), r))
    .collect();
    let leaves = leaf_pins(&mut *c, &all).await?;
    let prior = prior_pins(&mut *c, job.id, &all).await?;
    let mut out: Vec<(String, String)> = Vec::new();
    for k in pins_by_kind {
        for pin in &k.pins {
            if let Some(conflict) =
                verdict(cache.get(pin), leaves.contains(pin), prior.contains(pin))
            {
                out.push((
                    format!("knowledge:{}:{}/{pin}", k.stage, k.kind),
                    conflict.message(pin),
                ));
            }
        }
    }
    out.sort();
    Ok(out
        .into_iter()
        .map(|(path, message)| BundleViolation { path, message })
        .collect())
}

/// Approve step 10b: for every distinct pin, its leaves into `release_pack_leaf` of the new release `release_id`:
/// from `platform_pack_leaf`, else from the job's newest earlier release (PK-6). Runs only for a new release.
pub async fn snapshot(
    c: &mut PgConnection,
    tenant: Uuid,
    job: &JobRef,
    release_id: &str,
    pins_by_kind: &[KindPins],
) -> ApiResult<()> {
    let all = distinct_pins(pins_by_kind);
    if all.is_empty() {
        return Ok(());
    }
    let prior = newest_release(&mut *c, job.id, release_id).await?;
    for pin in &all {
        let Some(p) = kinds::parse_pin(pin) else {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "a knowledge pack pin is malformed: {pin}"
            )));
        };
        let n = sqlx::query(
            "INSERT INTO release_pack_leaf (tenant_id, job_id, release_id, pin, leaf_id, embed_model) \
             SELECT $1, $2, $3, $4, leaf_id, embed_model FROM platform_pack_leaf \
              WHERE pack_key = $5 AND version = $6",
        )
        .bind(tenant)
        .bind(job.id)
        .bind(release_id)
        .bind(pin)
        .bind(&p.key)
        .bind(p.version)
        .execute(&mut *c)
        .await?
        .rows_affected();
        if n == 0
            && let Some(prev) = &prior
        {
            sqlx::query(
                "INSERT INTO release_pack_leaf (tenant_id, job_id, release_id, pin, leaf_id, embed_model) \
                 SELECT $1, $2, $3, $4, leaf_id, embed_model FROM release_pack_leaf \
                  WHERE job_id = $2 AND release_id = $5 AND pin = $4",
            )
            .bind(tenant)
            .bind(job.id)
            .bind(release_id)
            .bind(pin)
            .bind(prev)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(listed: bool, state: Option<&str>, excluded: Option<&str>) -> CacheRow {
        CacheRow {
            pack_key: "industry/retail".into(),
            version: 1,
            listed,
            state: state.map(str::to_string),
            excluded_by: excluded.map(str::to_string),
        }
    }

    fn x(prefix: &str) -> Exclusion {
        Exclusion {
            key_prefix: prefix.into(),
            reason: "r".into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn verdict_follows_pk5_and_pk6() {
        let published = row(true, Some("published"), None);
        assert_eq!(verdict(Some(&published), true, false), None);
        assert_eq!(
            verdict(Some(&published), false, false),
            Some(Conflict::Unavailable)
        );
        // excluded wins over a previous pin
        let ex = row(false, Some("published"), Some("industry"));
        assert_eq!(
            verdict(Some(&ex), true, true),
            Some(Conflict::Excluded("industry".into()))
        );
        // retired: kept when the newest release pinned it, refused as a new pin (leaves cached or not)
        let retired = row(false, Some("retired"), None);
        assert_eq!(verdict(Some(&retired), true, true), None);
        assert_eq!(
            verdict(Some(&retired), true, false),
            Some(Conflict::Retired)
        );
        assert_eq!(
            verdict(Some(&row(false, Some("draft"), None)), false, false),
            Some(Conflict::Unpublished)
        );
        assert_eq!(
            verdict(Some(&row(false, Some("missing"), None)), false, false),
            Some(Conflict::Missing)
        );
        // no owner listing: listed + leaves is usable, anything else is unavailable
        assert_eq!(verdict(Some(&row(true, None, None)), true, false), None);
        assert_eq!(
            verdict(Some(&row(false, None, None)), true, false),
            Some(Conflict::Unavailable)
        );
        assert_eq!(verdict(None, false, false), Some(Conflict::Unavailable));
        assert_eq!(verdict(None, false, true), None);
    }

    #[test]
    fn exclusion_prefix_is_segment_wise_and_shortest() {
        let list = [x("industry/retail"), x("industry"), x("base")];
        assert_eq!(
            excluded_by("industry/retail/hr", &list).as_deref(),
            Some("industry")
        );
        assert_eq!(excluded_by("base", &list).as_deref(), Some("base"));
        assert_eq!(excluded_by("basement", &list), None);
        assert_eq!(
            excluded_by("industry/retailx", &[x("industry/retail")]),
            None
        );
        assert_eq!(excluded_by("job/a", &list), None);
    }

    #[test]
    fn owner_state_reads_the_timestamps() {
        let mut p = PlatformPack {
            key: "base".into(),
            version: 1,
            root_id: crate::strata::StrataStoreId(Uuid::nil()),
            published_at: None,
            retired_at: None,
        };
        assert_eq!(owner_state(&p), "draft");
        p.published_at = Some("2026-10-21T00:00:00Z".into());
        assert_eq!(owner_state(&p), "published");
        p.retired_at = Some("2026-10-21T01:00:00Z".into());
        assert_eq!(owner_state(&p), "retired");
    }

    #[test]
    fn every_reason_names_the_pin() {
        for c in [
            Conflict::Excluded("industry".into()),
            Conflict::Unpublished,
            Conflict::Retired,
            Conflict::Missing,
            Conflict::Unavailable,
        ] {
            let m = c.message("industry/retail@2");
            assert!(m.contains("industry/retail@2"), "{m}");
            assert!(!c.reason().is_empty());
        }
    }
}
