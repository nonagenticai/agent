//! The release area routes (B2 §4): target, settings, languages, refinements and business rules. Each
//! submodule owns its routes; this module only merges them.
use axum::Router;

use crate::state::AppState;

pub mod apply;
pub mod conflicts;
pub mod design;
pub mod draft;
pub mod readiness;
pub mod refine;
pub mod releases;
pub mod rules;
pub mod settings;
pub mod target;

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(target::routes())
        .merge(settings::routes())
        .merge(refine::routes())
        .merge(rules::routes())
        .merge(readiness::routes())
        .merge(releases::routes())
        .merge(apply::routes())
        .merge(conflicts::routes())
        .merge(draft::routes())
        .merge(design::routes())
}
