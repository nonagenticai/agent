//! Approve = cut a release (spec B2 §5 "Approve", D54/D62; a port of the fixture `approveRelease` and 05 §4.2). One
//! READ COMMITTED transaction, the caller's (job-scoped, `design::ensure_job` already run):
//!
//! 1. tenant suspended → 409 `tenant_suspended`;
//! 2. lock the job (`FOR UPDATE`: serialises with `design::track`, discard and apply);
//! 3. the draft selection: none → 409 `selection_missing`; `selection_seen_at` ≠ its `updated_at` → 409
//!    `selection_changed`; without the D106/D115 hierarchy → 422 `hierarchy_incomplete` /
//!    `department_industry_mismatch` / `duplicate_department` (`platform::templates::hierarchy_refusal`);
//! 4. the pins the cut compiles (`adopt_pins`): 422 `unknown_layer`, 409 `nothing_to_adopt`;
//! 5. compile: 422 `bundle_incomplete` (+`violations`) / `compile_failed` (+`errors`); then the `PinsAdopted` hooks;
//!    5b. the draft's platform pack pins against the caches `knowledge::packs::resolve::prepare_cut` refreshed before
//!    this transaction (PK-4..PK-6): 422 `compile_failed` (+`errors[{path: "knowledge:<stage>:<kind>/<pin>"}]`);
//!    skipped (and so is 10b) when no strata_rag is configured (`resolve::resolution_off`);
//! 6. the draft design at every snapshot place (tenant kinds included) and the release hash;
//! 7. nothing new since the current release → 409 `nothing_to_publish`; the newest release has this hash → reuse it
//!    (`created: false`);
//! 8. with `apply`: the publish gate over the DRAFT (409 `release_not_ready` + `blocking`);
//! 9. mint `cfg-<UTC date>.<N>`;
//! 10. insert `job_release` (`approved`), then its `release_snapshot` rows, then `release_bundled_draft` (the ledger
//!     rows the cut bundles) — the snapshot rows go in before anything reads the release (the snapshot LRU caches a
//!     release by its hash); 10b. each pack pin's leaves into `release_pack_leaf` (immutable; PK-4/PK-6);
//! 11. D15: a cut that adopts newer pins opens its upgrade conflicts now (`conflicts::raise_upgrade`);
//! 12. audit `release.approve`;
//! 13. with `apply`: apply the release (new or reused) in this same transaction (`release::apply::apply`).
//!
//! Step 7 compares like with like: a stored release's identity is recomputed NOW from what it stores
//! (`release_hash(bundle_hash, take_snapshot(Release(id)))`), never read from `job_release.hash`, which covers only
//! the kinds registered at that cut (a kind registered later would make every job look changed). The
//! `ReleaseApproved` hooks run after the caller commits ([`crate::release::hooks`]).
use std::collections::BTreeMap;

use agent_model::compile::Selection;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::bundle::RuleSlot;
use super::compile::{adopted_pins, bundle_incomplete, compile_failed, run_pins_adopted};
use super::conflicts;
use super::hash::{bundle_hash, decimals_as_text};
use super::mint;
use crate::audit::{self, Event};
use crate::auth::TenantStatus;
use crate::design::{self, DesignOf, DesignRow, JobRef, snap_to_json};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::packs::resolve;
use crate::platform::templates::TemplatePin;
use crate::readiness::{ReadOf, ReadinessCx};
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// `POST /api/jobs/{job}/releases` body (fixture `ApproveInput`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveInput {
    pub apply: bool,
    /// The `selection_updated_at` the page read (`null` while no selection was ever saved).
    #[serde(default)]
    pub selection_seen_at: Option<String>,
    /// D15 (opt-in): the template families whose newest tag this cut adopts.
    #[serde(default)]
    pub adopt_pins: Option<Vec<String>>,
}

/// What approve did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    /// The release cut, or the newest release reused (same hash).
    pub release_id: String,
    /// `false` = the newest release already had this hash: no row, no audit (05 §4.2 step 7).
    pub created: bool,
    /// The input asked to apply the release in the same transaction (§5 step 13).
    pub apply_requested: bool,
    /// The release was made current by this approve (step 13); `false` without `apply`, or when it already was.
    pub applied: bool,
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// `expected` (the client's `selection_seen_at`) names the stored `updated_at`: both absent, or the same instant
/// (fixture: `input.selection_seen_at !== t.updated_at`). Anything unparseable is a mismatch.
pub fn same_instant(expected: Option<&str>, at: Option<DateTime<Utc>>) -> bool {
    match (expected, at) {
        (None, None) => true,
        (Some(s), Some(t)) => {
            DateTime::parse_from_rfc3339(s.trim()).is_ok_and(|d| d.with_timezone(&Utc) == t)
        }
        _ => false,
    }
}

/// The release identity (§5): `bundle_hash(canonical {"bundle_hash", "design": [[key, snap] sorted by key]})`, each
/// snap in its stored form (`{"v": v}` or null, so "nothing" and a JSON null differ). A design value may carry a
/// decimal (a gate's `threshold_value`, e.g. `0.7`) while `bundle_hash` is integers-only: a non-integer number is
/// hashed as its JSON text ([`decimals_as_text`]), as `job_design::base::gate_rule_hash` does.
pub fn release_hash(bundle_hash_value: &str, design: &[DesignRow]) -> ApiResult<String> {
    let mut rows: Vec<&DesignRow> = design.iter().collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    let places: Vec<Value> = rows
        .iter()
        .map(|r| json!([r.key, snap_to_json(&r.snap).map(decimals_as_text)]))
        .collect();
    bundle_hash(&json!({"bundle_hash": bundle_hash_value, "design": places}))
        .map_err(|e| internal(format!("the release design cannot be hashed: {e}")))
}

#[derive(sqlx::FromRow)]
struct TargetRow {
    selection: Option<Value>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct HeadRow {
    release_id: String,
    bundle_hash: String,
}

/// The identity of stored release `id` as of now (module docs, step 7), or `None` when the release does not store a
/// kind the draft ledger holds an edit for (`edited` = the place kinds of the job's ledger rows).
/// `take_snapshot(Release)` fills an uncovered kind from the working rows (= the draft), so without this check such a
/// release always "equals" the draft and the cut would reuse it: the edited uncovered value would never become part
/// of a release (owner ruling 2026-10-22: publishing makes it live and covers the kind). A late-registered kind
/// nobody edited has no ledger row and keeps the old rule: it does not make every job look changed (and with no
/// ledger rows at all, step 7's `nothing_to_publish` still holds).
async fn stored_identity(
    c: &mut PgConnection,
    reg: &crate::design::Registry,
    jr: &JobRef,
    head: &HeadRow,
    edited: &[String],
) -> ApiResult<Option<String>> {
    if !edited.is_empty() {
        let stored_kinds: Vec<String> = sqlx::query_scalar(
            "SELECT snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
        )
        .bind(jr.id)
        .bind(&head.release_id)
        .fetch_one(&mut *c)
        .await?;
        if edited.iter().any(|k| !stored_kinds.contains(k)) {
            return Ok(None);
        }
    }
    let of = DesignOf::Release(head.release_id.clone());
    let rows = design::take_snapshot(&mut *c, reg, jr, &of).await?;
    release_hash(&head.bundle_hash, &rows).map(Some)
}

/// 409 `tenant_suspended` when the tenant is suspended. The status is read from the row `FOR SHARE` (T11 M1 / T12
/// M4), not from the request context: a suspend that committed after the request was authorised is seen, and one
/// that runs now waits for this transaction. Callers take it before the job lock (tenants → jobs).
pub(crate) async fn suspended(c: &mut PgConnection, j: &JobCtx) -> ApiResult<()> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT status, name FROM tenants WHERE id = $1 FOR SHARE")
            .bind(j.t.tenant)
            .fetch_optional(&mut *c)
            .await?;
    let Some((status, name)) = row else {
        return Ok(());
    };
    if status != TenantStatus::Suspended.as_str() {
        return Ok(());
    }
    Err(ApiError::Conflict(
        "tenant_suspended",
        format!("{name} is suspended; releases are frozen until it is resumed."),
    ))
}

/// The selection as a release freezes it (`ResolvedSelection`): the selection with the profile's values inlined.
fn resolved_selection(
    sel: &Selection,
    profile_values: &BTreeMap<String, String>,
) -> ApiResult<Value> {
    let mut v = serde_json::to_value(sel)
        .map_err(|e| internal(format!("a selection does not serialise: {e}")))?;
    let pv: Map<String, Value> = profile_values
        .iter()
        .map(|(k, x)| (k.clone(), Value::String(x.clone())))
        .collect();
    let Some(m) = v.as_object_mut() else {
        return Err(internal("a selection is not a JSON object".to_string()));
    };
    m.insert("profile_values".to_string(), Value::Object(pv));
    Ok(v)
}

fn to_value<T: serde::Serialize>(what: &str, x: &T) -> ApiResult<Value> {
    serde_json::to_value(x).map_err(|e| internal(format!("{what} does not serialise: {e}")))
}

/// Cut a release of the job's draft (§5 steps 1–10 and 12; module docs), in the caller's job-scoped transaction
/// `c`. The caller ran `design::ensure_job`, commits, and then runs the approved hooks when `created`.
pub async fn approve(
    c: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    input: &ApproveInput,
) -> ApiResult<Cut> {
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let reg = st.design.as_ref();
    let compiler = st.compiler.as_ref();
    // 1–2
    suspended(&mut *c, j).await?;
    let job_pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(jr.id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "That job does not exist.".into()))?;
    let job_pins: Vec<TemplatePin> = serde_json::from_value(job_pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    // 3
    let target = sqlx::query_as::<_, TargetRow>(
        "SELECT selection, updated_at FROM job_target WHERE job_id = $1",
    )
    .bind(jr.id)
    .fetch_optional(&mut *c)
    .await?;
    let (stored, updated_at) = match target {
        Some(t) => (t.selection, t.updated_at),
        None => (None, None),
    };
    let Some(stored) = stored else {
        return Err(ApiError::Conflict(
            "selection_missing",
            "Pick a selection before approving.".into(),
        ));
    };
    if !same_instant(input.selection_seen_at.as_deref(), updated_at) {
        return Err(ApiError::Conflict(
            "selection_changed",
            "The selection changed after you opened this page; review it and approve again.".into(),
        ));
    }
    let sel: Selection = serde_json::from_value(stored)
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?;
    // D106/D115: no cut without one industry and one or more of its departments (a legacy selection included)
    crate::platform::templates::hierarchy_refusal(&sel)?;
    // D111: the region must be one of the platform's (422 `unknown_region`)
    crate::platform::templates::region_refusal(&mut *c, &sel).await?;
    // 4
    let adopt: Vec<String> = input.adopt_pins.clone().unwrap_or_default();
    let pins = adopted_pins(&mut *c, compiler, &job_pins, &adopt).await?;
    // 5
    let compiled = compiler
        .compile(&mut *c, &sel, &pins)
        .await
        .map_err(ApiError::from)?;
    if !compiled.violations.is_empty() {
        return Err(bundle_incomplete(&compiled.violations));
    }
    run_pins_adopted(&st.pins_adopted, &mut *c, &jr, &job_pins, &pins, &compiled).await?;
    // 5b (nothing to check or snapshot while pack resolution is off: no strata_rag configured)
    let pack_pins = if resolve::resolution_off(st) {
        Vec::new()
    } else {
        resolve::draft_pins(&mut *c, reg, &jr).await?
    };
    let pack_conflicts = resolve::check(&mut *c, &jr, &pack_pins).await?;
    if !pack_conflicts.is_empty() {
        return Err(compile_failed(&pack_conflicts));
    }
    // 6
    design::ensure_baseline(&mut *c, reg, &jr).await?;
    let mut snapshot = design::take_snapshot(&mut *c, reg, &jr, &DesignOf::Draft).await?;
    snapshot.sort_by(|a, b| a.key.cmp(&b.key));
    let draft_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
            .bind(jr.id)
            .fetch_one(&mut *c)
            .await?;
    let hash = release_hash(&compiled.bundle_hash, &snapshot)?;
    // the place kinds the draft ledger holds edits for (step 7: an edit of a kind a release does not store)
    let mut edited: Vec<String> = Vec::new();
    for d in design::ledger::of_job(&mut *c, &jr).await? {
        let k = design::read::place_of(reg, &d.kind, &d.r#ref)?.kind;
        if !edited.contains(&k) {
            edited.push(k);
        }
    }
    // 7
    let current = sqlx::query_as::<_, HeadRow>(
        "SELECT release_id, bundle_hash FROM job_release WHERE job_id = $1 AND state = 'current'",
    )
    .bind(jr.id)
    .fetch_optional(&mut *c)
    .await?;
    let mut current_bundle: Option<Value> = None;
    if let Some(cur) = &current {
        current_bundle = sqlx::query_scalar(
            "SELECT bundle FROM job_release WHERE job_id = $1 AND release_id = $2",
        )
        .bind(jr.id)
        .bind(&cur.release_id)
        .fetch_optional(&mut *c)
        .await?;
    }
    let current_hash = match &current {
        Some(cur) if draft_count == 0 => stored_identity(&mut *c, reg, &jr, cur, &[]).await?,
        _ => None,
    };
    if let Some(cur) = &current
        && current_hash.as_deref() == Some(hash.as_str())
    {
        return Err(ApiError::Conflict(
            "nothing_to_publish",
            format!(
                "Nothing to publish: no draft changes and no template change since {}.",
                cur.release_id
            ),
        ));
    }
    let newest = sqlx::query_as::<_, HeadRow>(
        "SELECT release_id, bundle_hash FROM job_release WHERE job_id = $1 \
          ORDER BY approved_at DESC, release_id DESC LIMIT 1",
    )
    .bind(jr.id)
    .fetch_optional(&mut *c)
    .await?;
    if let Some(n) = newest
        && stored_identity(&mut *c, reg, &jr, &n, &edited)
            .await?
            .as_deref()
            == Some(hash.as_str())
    {
        let applied = if input.apply {
            super::apply::apply(&mut *c, st, j, &n.release_id)
                .await?
                .applied
        } else {
            false
        };
        return Ok(Cut {
            release_id: n.release_id,
            created: false,
            apply_requested: input.apply,
            applied,
        });
    }
    // 8
    if input.apply {
        let mut rules: Vec<RuleSlot> = Vec::new();
        for r in &compiled.bundle.rules {
            if !rules.iter().any(|s| s.id == r.id) {
                rules.push(r.clone());
            }
        }
        let cx = ReadinessCx {
            job: &jr,
            of: ReadOf::Draft,
            design: DesignOf::Draft,
            selection: Some(&sel),
            requires: &compiled.bundle.requires,
            causes: &compiled.bundle.causes,
            rules: Some(rules.as_slice()),
            reg,
            compiler,
        };
        st.readiness.gate(&mut *c, &cx, None).await?;
    }
    // 9
    let id = mint::mint(&mut *c, &jr, &mint::day_of(Utc::now())).await?;
    // 10
    let kinds: Vec<String> = reg.snapshot_kinds().iter().map(|k| k.to_string()).collect();
    let selection = resolved_selection(&sel, &compiled.bundle.profile_values)?;
    let provenance = json!({"overlay_revs": [], "seed_versions": [1]});
    let bundle_json = to_value("the bundle", &compiled.bundle)?;
    // rollbacks_seen: the rollbacks logged before this cut (fixture `rollbacksOf(job).length`); apply re-marks as
    // re-applied only the rows rolled back after it. Immutable once inserted.
    sqlx::query(
        "INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins, \
                                  provenance, requires, causes, state, approved_by, approved_at, snapshot_kinds, \
                                  rollbacks_seen) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'approved', $12, clock_timestamp(), $13, \
                 (SELECT count(*)::int FROM release_rollback WHERE job_id = $2))",
    )
    .bind(j.t.tenant)
    .bind(jr.id)
    .bind(&id)
    .bind(&hash)
    .bind(compiled.bundle_hash.as_str())
    .bind(&bundle_json)
    .bind(selection)
    .bind(to_value("the pins", &pins)?)
    .bind(provenance)
    .bind(to_value("the requirements", &compiled.bundle.requires)?)
    .bind(to_value("the causes", &compiled.bundle.causes)?)
    .bind(j.t.caller.sub.as_str())
    .bind(kinds)
    .execute(&mut *c)
    .await?;
    for r in &snapshot {
        sqlx::query(
            "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(j.t.tenant)
        .bind(jr.id)
        .bind(&id)
        .bind(&r.key)
        .bind(&r.kind)
        .bind(Value::Object(r.r#ref.clone()))
        .bind(snap_to_json(&r.snap))
        .execute(&mut *c)
        .await?;
    }
    // 10b
    resolve::snapshot(&mut *c, j.t.tenant, &jr, &id, &pack_pins).await?;
    sqlx::query(
        "INSERT INTO release_bundled_draft \
                (tenant_id, job_id, release_id, key, ledger_id, kind, ref, area, page, label, after, at, by_sub) \
         SELECT tenant_id, job_id, $2, key, id, kind, ref, area, page, label, after, at, by_sub \
           FROM draft_ledger WHERE job_id = $1",
    )
    .bind(jr.id)
    .bind(&id)
    .execute(&mut *c)
    .await?;
    // 11
    conflicts::raise_upgrade(
        &mut *c,
        &st.conflict_detectors,
        j.t.tenant,
        &jr,
        &id,
        &job_pins,
        &pins,
        current_bundle.as_ref(),
        &bundle_json,
    )
    .await?;
    // 12
    let sel_json = to_value("the selection", &sel)?;
    audit::record(
        &mut *c,
        &j.t.actor(),
        Event::new("release.approve", "release", id.clone())
            .job(&jr.key)
            .detail("release_id", id.clone())
            .detail("hash", hash)
            .detail("selection", sel_json)
            .detail("adopt_pins", adopt)
            .detail("draft_count", draft_count),
    )
    .await?;
    // 13
    let applied = if input.apply {
        super::apply::apply(&mut *c, st, j, &id).await?.applied
    } else {
        false
    };
    Ok(Cut {
        release_id: id,
        created: true,
        apply_requested: input.apply,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, snap: Option<Value>) -> DesignRow {
        DesignRow {
            key: key.to_string(),
            kind: "languages".to_string(),
            r#ref: Map::new(),
            snap,
        }
    }

    #[test]
    fn release_hash_is_order_free_and_tells_nothing_from_null() {
        let b = "sha256:".to_string() + &"0".repeat(64);
        let a = [row("a", Some(json!(["en"]))), row("b", None)];
        let swapped = [row("b", None), row("a", Some(json!(["en"])))];
        let h = release_hash(&b, &a).unwrap();
        assert!(h.starts_with("sha256:") && h.len() == 71, "{h}");
        assert_eq!(h, release_hash(&b, &swapped).unwrap());
        let null = [row("a", Some(json!(["en"]))), row("b", Some(Value::Null))];
        assert_ne!(h, release_hash(&b, &null).unwrap());
        let other = "sha256:".to_string() + &"1".repeat(64);
        assert_ne!(h, release_hash(&other, &a).unwrap());
        // a decimal in a design value (a gate question's threshold) hashes; equal decimals hash equally
        let dec = [row(
            "q",
            Some(json!({"threshold_value": 0.7, "n": [1, 0.25]})),
        )];
        let d = release_hash(&b, &dec).unwrap();
        let same = [row(
            "q",
            Some(json!({"n": [1, 0.25], "threshold_value": 0.7})),
        )];
        assert_eq!(d, release_hash(&b, &same).unwrap());
        let other_dec = [row(
            "q",
            Some(json!({"threshold_value": 0.8, "n": [1, 0.25]})),
        )];
        assert_ne!(d, release_hash(&b, &other_dec).unwrap());
    }

    #[test]
    fn seen_at_compares_instants() {
        let t = DateTime::parse_from_rfc3339("2026-10-20T10:00:00.5Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_instant(None, None));
        assert!(same_instant(Some("2026-10-20T12:00:00.5+02:00"), Some(t)));
        assert!(!same_instant(None, Some(t)));
        assert!(!same_instant(Some("2026-10-20T10:00:00Z"), Some(t)));
        assert!(!same_instant(Some("x"), None));
    }
}
