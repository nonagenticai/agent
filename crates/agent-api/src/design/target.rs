//! A design target (spec B2 §2.2): how the ledger reads and writes one kind of design place. The Rust form of the
//! fixture `Target` interface (`web/src/lib/fixtures/ledger.ts`); one target per ledger kind of [`super::KINDS`].
use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;

use super::{DraftRef, Place, Scope, Snap};
use crate::error::ApiResult;

pub trait DesignTarget: Send + Sync + 'static {
    /// The ledger kind, e.g. `"gate"`. Must be one of [`super::KINDS`].
    fn kind(&self) -> &'static str;
    fn scope(&self) -> Scope;
    /// The place's stable key. MUST equal the fixture `TARGETS[kind].key(ref)` (checked against
    /// `tests/fixtures/design-kinds.json`).
    fn key(&self, r: &DraftRef) -> String;
    /// A projection kind (`entity_hidden`/`_synonyms`/`_attributes`): its `places` returns `None` and it is not
    /// stored in snapshots. Must agree with `places` (the fixture export records it per kind).
    fn is_projection(&self) -> bool {
        false
    }
    /// Draft value at a place, read from the plan's working rows.
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>>;
    /// Write a value into the working rows (discard, follow-live after apply/rollback, restore). `None` = remove.
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>>;
    /// Every place the job (or tenant) has. `None` marks a projection kind (not stored in snapshots).
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>>;
    /// What counts as "the same value" (fixture `proj`); default: the whole snap.
    fn proj(&self, v: &Snap) -> Value {
        v.clone().unwrap_or(Value::Null)
    }
    /// One-line text for diffs and change rows (fixture `text`).
    fn text(&self, v: &Snap, r: &DraftRef) -> Option<String>;
    /// Projection kinds only: the snapshot place they live in (fixture `placeOf`).
    fn parent(&self, _r: &DraftRef) -> Option<(&'static str, DraftRef)> {
        None
    }
    /// Projection kinds only: read their field from the parent place's value (fixture `valueIn`).
    fn project(&self, _parent: &Snap, _r: &DraftRef) -> Snap {
        None
    }
    /// Side effect when a bundled change is published (fixture `published`, e.g. procedure `released_in`).
    fn published<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
        _r: &'a DraftRef,
        _release: &'a str,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
