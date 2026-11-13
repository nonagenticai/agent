//! Extension points of the platform model (plan B6 §4, Ruling 11). B6 ships [`NoHooks`]; B8 implements
//! [`GeneratorGuard`] (a generator run whose validation is red blocks a tag, 422 `validation_red`), B7 and B8
//! implement [`ObjectContextSources`] (connector templates, journey activities). T8 registers them on `AppState`.
pub use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;

use crate::error::ApiResult;

/// What blocks tagging a family: the generator's red validation items for it (empty = nothing blocks).
///
/// Async over the caller's connection (plan decision R12, B8 plan; changed by B6b `tagLayerRevisions`): B8's
/// implementation reads its generator runs in the tagging transaction (`tag_family` holds the family lock), so a
/// run that turns red concurrently is seen under the same lock.
pub trait GeneratorGuard: Send + Sync {
    fn red_items<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        family: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<String>>>;
}

/// Where an object's systems and activities come from (P10 object context).
///
/// `activities` is async over the caller's connection (B8 T10: the journey grid is read from the layer store, so the
/// implementation needs the transaction the route already holds); `systems` stays plain, B7's list is built by the
/// route itself.
pub trait ObjectContextSources: Send + Sync {
    fn systems(&self, object: &str) -> Vec<Value>;
    fn activities<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        object: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<Value>>>;
}

/// The default: nothing red, no systems, no activities.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl GeneratorGuard for NoHooks {
    fn red_items<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        _family: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl ObjectContextSources for NoHooks {
    fn systems(&self, _object: &str) -> Vec<Value> {
        Vec::new()
    }

    fn activities<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        _object: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<Value>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_hooks_return_nothing() {
        // `red_items` and `activities` need a connection: `tests/platform_store.rs` (`validation_red_422`,
        // `nothing_to_tag_409`) and `tests/b8/generator_drive.rs` cover them over a real database.
        let h = NoHooks;
        assert!(h.systems("order").is_empty());
        let _guard: &dyn GeneratorGuard = &h;
    }
}
