//! The draft-vs-live diff (spec B2 §1, D62): working rows vs the live snapshot, place by place. A port of
//! `fixtures/ledger.ts` `diffDesign` (+ `placesIn`). The ledger only annotates each differing place with its change
//! history; it never decides what differs.
use std::collections::BTreeSet;

use sqlx::PgConnection;

use super::ledger::{self, LedgerRow};
use super::read::{DesignPlace, place_of, same};
use super::snapshot::{internal, job_state, stored};
use super::{DesignOf, JobRef, Place, Registry, Snap};
use crate::error::ApiResult;

/// One place where the draft differs from live (fixture `DesignDelta`).
#[derive(Clone, Debug, PartialEq)]
pub struct DesignDelta {
    pub place: DesignPlace,
    pub live: Snap,
    pub draft: Snap,
    /// Ledger rows (history) of unpublished changes at this place, least recently written first. A projection
    /// kind's row (e.g. `entity_hidden`) is listed at its parent place.
    pub changes: Vec<LedgerRow>,
    /// The live release whose snapshot does not store this place's kind (§2.5 coverage: a kind newer than the
    /// release, until `coverage::backfill` covers it). Production reads the working rows there today
    /// (`read::value` falls through), and publishing makes the draft value part of the release: the operator must
    /// see that before publishing (owner ruling 2026-10-22). `live` is then what Production reads (= `draft`).
    /// `None` = the live design covers the kind (or the job was never released: its baseline is live).
    pub uncovered: Option<String>,
}

/// The job's live release and the snapshot kinds it does not store (sorted), or `None` when the job has no live
/// release (never released: the baseline is live, see [`super::read::value`]). An empty list = fully covered.
pub async fn uncovered_kinds(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Option<(String, Vec<&'static str>)>> {
    let st = job_state(&mut *c, job).await?;
    let Some(id) = st.current_release_id.clone() else {
        return Ok(None);
    };
    let Some(snap) = stored(&mut *c, reg, job, &st, &DesignOf::Live).await? else {
        return Ok(None);
    };
    let kinds = reg
        .snapshot_kinds()
        .into_iter()
        .filter(|k| !snap.covers(k))
        .collect();
    Ok(Some((id, kinds)))
}

/// Every snapshot place — of the draft or of the live snapshot — where the working rows differ from live
/// (`proj`-equality). A kind the live snapshot does not cover is live = draft (§2.5 coverage), so it never differs by
/// value; when the job HAS a live release, each such place with a draft value is still listed, marked
/// [`DesignDelta::uncovered`] (publishing makes it part of the release). Never released (baseline live): not listed.
pub async fn diff(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<DesignDelta>> {
    let st = job_state(&mut *c, job).await?;
    let Some(snap) = stored(&mut *c, reg, job, &st, &DesignOf::Live).await? else {
        return Err(internal("the live design has no stored snapshot".into()));
    };
    let live_release = st.current_release_id.clone();
    let ledger = ledger::of_job(&mut *c, job).await?;
    let at = Place {
        tenant: st.tenant_id,
        job: Some(job),
    };
    let mut places: Vec<DesignPlace> = Vec::new();
    let mut seen = BTreeSet::new();
    for kind in reg.snapshot_kinds() {
        let t = reg.target(kind)?;
        let Some(refs) = t.places(&mut *c, at).await? else {
            continue;
        };
        for r in refs {
            let key = t.key(&r);
            if seen.insert(key.clone()) {
                places.push(DesignPlace {
                    key,
                    kind: kind.to_string(),
                    r#ref: r,
                });
            }
        }
    }
    for row in snap.rows() {
        if seen.insert(row.key.clone()) {
            places.push(DesignPlace {
                key: row.key.clone(),
                kind: row.kind.clone(),
                r#ref: row.r#ref.clone(),
            });
        }
    }
    let mut out = Vec::new();
    for p in places {
        let t = reg.target(&p.kind)?;
        let draft = t.get(&mut *c, at, &p.r#ref).await?;
        let (live, uncovered) = if snap.covers(&p.kind) {
            (snap.row(&p.key).and_then(|r| r.snap.clone()), None)
        } else {
            (draft.clone(), live_release.clone())
        };
        match &uncovered {
            // uncovered: listed only when there is a draft value to make live
            Some(_) if draft.is_none() => continue,
            Some(_) => {}
            None if same(t, &live, &draft) => continue,
            None => {}
        }
        let mut changes = Vec::new();
        for d in &ledger {
            if place_of(reg, &d.kind, &d.r#ref)?.key == p.key {
                changes.push(d.clone());
            }
        }
        out.push(DesignDelta {
            place: p,
            live,
            draft,
            changes,
            uncovered,
        });
    }
    Ok(out)
}
