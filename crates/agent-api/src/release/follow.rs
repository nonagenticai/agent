//! Live moves under the draft (spec B2 §1, §5 apply step 9; D62/D69): a port of `fixtures/ledger.ts` `planMoves` /
//! `followLive` / `valueIn`. Apply (a cut, a rollback or a re-apply) made a release current: live moved from the
//! `from` design to the `to` design (both from `design::take_snapshot`). Places without an unpublished change follow
//! live (their working rows are rewritten through the target's `set`), unpublished changes keep their draft value.
use std::collections::{BTreeSet, HashMap};

use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::read::{DesignPlace, place_of, same};
use crate::design::{
    DesignRow, DraftRef, JobRef, Place, Registry, Scope, Snap, ledger, snap_to_json,
};
use crate::error::ApiResult;
use crate::runtime::design::nudge_on_commit;
use crate::runtime::scope::Env as NudgeEnv;

/// One place whose live value moves (fixture `DesignMove`).
#[derive(Clone, Debug, PartialEq)]
pub struct Move {
    pub place: DesignPlace,
    pub now: Snap,
    pub to: Snap,
}

/// A stored design by place key.
pub struct Indexed<'a> {
    rows: HashMap<&'a str, &'a DesignRow>,
}

impl<'a> Indexed<'a> {
    pub fn new(rows: &'a [DesignRow]) -> Self {
        let mut m = HashMap::with_capacity(rows.len());
        for r in rows {
            m.entry(r.key.as_str()).or_insert(r);
        }
        Self { rows: m }
    }

    /// The value stored at place `key` (nothing when the design has no such place).
    pub fn at(&self, key: &str) -> Snap {
        self.rows.get(key).and_then(|r| r.snap.clone())
    }

    /// Fixture `valueIn`: the value at any location — a projection kind is read from its parent place.
    pub fn value_in(&self, reg: &Registry, kind: &str, r: &DraftRef) -> ApiResult<Snap> {
        let p = place_of(reg, kind, r)?;
        let val = self.at(&p.key);
        if p.kind == kind || val.is_none() {
            return Ok(val);
        }
        Ok(reg.target(kind)?.project(&val, r))
    }
}

/// Fixture `placesIn`: every place of the job's working rows (snapshot kinds), then every place of `snaps` not yet
/// listed.
async fn places_in(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    snaps: &[&[DesignRow]],
) -> ApiResult<Vec<DesignPlace>> {
    let at = Place {
        tenant,
        job: Some(job),
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for kind in reg.snapshot_kinds() {
        let t = reg.target(kind)?;
        let Some(refs) = t.places(&mut *c, at).await? else {
            continue;
        };
        for r in refs {
            let key = t.key(&r);
            if seen.insert(key.clone()) {
                out.push(DesignPlace {
                    key,
                    kind: kind.to_string(),
                    r#ref: r,
                });
            }
        }
    }
    for s in snaps {
        for r in s.iter() {
            if seen.insert(r.key.clone()) {
                out.push(DesignPlace {
                    key: r.key.clone(),
                    kind: r.kind.clone(),
                    r#ref: r.r#ref.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// Fixture `planMoves`: every place whose value differs between `from` and `to` (the kind's `proj`-equality).
pub async fn plan_moves(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    from: &[DesignRow],
    to: &[DesignRow],
) -> ApiResult<Vec<Move>> {
    let (a, b) = (Indexed::new(from), Indexed::new(to));
    let mut out = Vec::new();
    for place in places_in(&mut *c, reg, tenant, job, &[from, to]).await? {
        let now = a.at(&place.key);
        let next = b.at(&place.key);
        if !same(reg.target(&place.kind)?, &now, &next) {
            out.push(Move {
                place,
                now,
                to: next,
            });
        }
    }
    Ok(out)
}

/// Fixture `followLive`, after the caller made the release whose design is `to` current (live moved from `from`):
/// - a moved place with no ledger row follows live: a job kind's working rows are set to the new live value; a
///   tenant-wide kind (D69) is never rewritten by one job's apply (the diff shows it until this job publishes it);
/// - each ledger row at a moved place keeps its draft value and is flagged `live_moved {release, at, was}` (`was` in
///   the stored snap form `{"v": …}` / null) — unless its own value did not move, or this apply publishes it
///   (`publishing`: the ledger keys the cut bundled);
/// - then every ledger row whose draft value IS the new live value leaves the ledger, unless an approved cut still
///   bundles it;
/// - when a working row changed or a ledger row was flagged, `jobs.draft_rev` is bumped once and the job's `test`
///   config nudge is queued for the caller's commit (B4 §0 R4; never in a handler).
///
/// Returns the ledger keys whose live value moved under them (`release_rollback.draft_moved`).
#[allow(clippy::too_many_arguments)]
pub async fn follow_live(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    from: &[DesignRow],
    to: &[DesignRow],
    release_id: &str,
    at: &str,
    publishing: &BTreeSet<String>,
) -> ApiResult<Vec<String>> {
    let (a, b) = (Indexed::new(from), Indexed::new(to));
    let place = Place {
        tenant,
        job: Some(job),
    };
    let mut moved: Vec<String> = Vec::new();
    let mut rewritten = false;
    let rows = ledger::of_job(&mut *c, job).await?;
    for m in plan_moves(&mut *c, reg, tenant, job, from, to).await? {
        let mut here = Vec::new();
        for d in &rows {
            if place_of(reg, &d.kind, &d.r#ref)?.key == m.place.key {
                here.push(d);
            }
        }
        if here.is_empty() {
            let t = reg.target(&m.place.kind)?;
            if t.scope() != Scope::Tenant {
                let draft = t.get(&mut *c, place, &m.place.r#ref).await?;
                t.set(&mut *c, place, &m.place.r#ref, &m.to).await?;
                rewritten |= !same(t, &draft, &m.to);
            }
            continue;
        }
        for d in here {
            let t = reg.target(&d.kind)?;
            let was = a.value_in(reg, &d.kind, &d.r#ref)?;
            let next = b.value_in(reg, &d.kind, &d.r#ref)?;
            if same(t, &was, &next) || publishing.contains(&d.key) {
                continue;
            }
            let mark = json!({"release": release_id, "at": at, "was": snap_to_json(&was)});
            sqlx::query("UPDATE draft_ledger SET live_moved = $2 WHERE id = $1")
                .bind(d.id)
                .bind(mark)
                .execute(&mut *c)
                .await?;
            moved.push(d.key.clone());
        }
    }
    for d in ledger::of_job(&mut *c, job).await? {
        let t = reg.target(&d.kind)?;
        let live = b.value_in(reg, &d.kind, &d.r#ref)?;
        let draft = t.get(&mut *c, place, &d.r#ref).await?;
        if same(t, &draft, &live) && !ledger::bundled_in_approved(&mut *c, job, &d.key).await? {
            sqlx::query("DELETE FROM draft_ledger WHERE id = $1")
                .bind(d.id)
                .execute(&mut *c)
                .await?;
        }
    }
    // B4 §0 R4: the draft moved under the job (working rows rewritten or ledger rows flagged) — one `draft_rev`
    // bump, so a test case stamped `CaseDesign::Draft{draft_rev}` before it never claims the new draft; the test
    // scope re-reads it once the caller commits
    if rewritten || !moved.is_empty() {
        sqlx::query("UPDATE jobs SET draft_rev = draft_rev + 1 WHERE id = $1")
            .bind(job.id)
            .execute(&mut *c)
            .await?;
        nudge_on_commit(&mut *c, &[job.id], NudgeEnv::Test).await?;
    }
    Ok(moved)
}
