//! The registered design targets, by kind (spec B2 §2.2). Built once at boot ([`super::registry()`]) and shared as
//! `AppState.design`.
use std::collections::BTreeMap;
use std::sync::Arc;

use super::KINDS;
use super::read::SnapshotCache;
use super::target::DesignTarget;
use crate::error::{ApiError, ApiResult};
use crate::release::compile::TemplateCompiler;

#[derive(Default)]
pub struct Registry {
    targets: BTreeMap<&'static str, Arc<dyn DesignTarget>>,
    /// The release snapshot LRU of the read API (spec §2.4): on the registry so every design read reaches it.
    snapshots: SnapshotCache,
    /// The template compiler [`super::registry()`] was built with (B5 T12): the runtime's design read
    /// (`RuntimeDesign::read`) folds the job design at the design's pins through it. `None` for a registry built by
    /// hand (tests): the runtime then reads the template content alone and an empty job design.
    compiler: Option<Arc<dyn TemplateCompiler>>,
}

impl Registry {
    /// Keep `compiler` for the runtime's job-design read ([`Registry::compiler`]).
    pub fn set_compiler(&mut self, compiler: Arc<dyn TemplateCompiler>) {
        self.compiler = Some(compiler);
    }

    /// The template compiler of a production registry ([`super::registry()`]); `None` for a hand-built one.
    pub fn compiler(&self) -> Option<&dyn TemplateCompiler> {
        self.compiler.as_deref()
    }

    /// Panics on a duplicate kind or a kind not in [`KINDS`]: both are programming errors caught at boot.
    pub fn register(&mut self, t: Arc<dyn DesignTarget>) {
        let kind = t.kind();
        assert!(
            KINDS.contains(&kind),
            "design target kind `{kind}` is not in design::KINDS"
        );
        assert!(
            !self.targets.contains_key(kind),
            "design target kind `{kind}` is registered twice"
        );
        self.targets.insert(kind, t);
    }

    /// TEST ONLY — never call from production code. Registers a target whose kind starts with `scratch` and is not
    /// in [`KINDS`] (a scratch kind exercising the engine without a real plan's tables). Panics otherwise, or on a
    /// duplicate kind.
    #[doc(hidden)]
    pub fn register_scratch(&mut self, t: Arc<dyn DesignTarget>) {
        let kind = t.kind();
        assert!(
            kind.starts_with("scratch") && !KINDS.contains(&kind),
            "register_scratch takes only test kinds named scratch…, not `{kind}`"
        );
        assert!(
            !self.targets.contains_key(kind),
            "design target kind `{kind}` is registered twice"
        );
        self.targets.insert(kind, t);
    }

    /// The release snapshot cache (spec §2.4).
    pub(crate) fn snapshots(&self) -> &SnapshotCache {
        &self.snapshots
    }

    /// The target of `kind`. An unknown kind is a programming error (500), never a client error.
    pub fn target(&self, kind: &str) -> ApiResult<&dyn DesignTarget> {
        match self.targets.get(kind) {
            Some(t) => Ok(&**t),
            None => Err(ApiError::Internal(anyhow::anyhow!(
                "no design target is registered for kind `{kind}`"
            ))),
        }
    }

    /// Every registered kind, sorted.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.targets.keys().copied().collect()
    }

    /// Registered kinds a snapshot stores (every kind but the projections), sorted.
    pub fn snapshot_kinds(&self) -> Vec<&'static str> {
        self.targets
            .iter()
            .filter(|(_, t)| !t.is_projection())
            .map(|(k, _)| *k)
            .collect()
    }
}
