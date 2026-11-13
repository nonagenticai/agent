//! B8 generator (P18): the LLM drafter and its metering (T8). Runs, steps and routes (T9+) build on these.
use std::time::Duration;

use anyhow::{Context, Result};
use uuid::Uuid;

pub mod llm;
pub mod prompts;
pub mod run;
pub mod store;
pub mod usage;

/// `GENERATOR_DRAFT_TIMEOUT_SECS` default (B8-5: under Cloudflare's ~100 s).
pub const DEFAULT_DRAFT_TIMEOUT_SECS: u64 = 80;

/// The generator's config (R8, D89).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratorConfig {
    /// `GENERATOR_PROVIDER_ID`: the platform runs' provider record (a B3 provider of the platform-owned tenant).
    /// Unset: platform drafting answers 503 `unavailable` "generator provider not configured".
    pub provider_id: Option<Uuid>,
    /// `GENERATOR_MODEL`: the model to call on it (default: the record's first model).
    pub model: Option<String>,
    /// `GENERATOR_DRAFT_TIMEOUT_SECS` (default 80, 1..=95): the budget of one whole step.
    pub draft_timeout: Duration,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            provider_id: None,
            model: None,
            draft_timeout: Duration::from_secs(DEFAULT_DRAFT_TIMEOUT_SECS),
        }
    }
}

impl GeneratorConfig {
    /// Read from trimmed, non-empty env values (crate::config's `trimmed`).
    pub fn from_lookup(trimmed: &impl Fn(&str) -> Option<String>) -> Result<Self> {
        let provider_id = match trimmed("GENERATOR_PROVIDER_ID") {
            None => None,
            Some(v) => Some(
                Uuid::parse_str(&v)
                    .ok()
                    .filter(|u| !u.is_nil())
                    .context("GENERATOR_PROVIDER_ID must be a non-nil provider uuid")?,
            ),
        };
        let secs = match trimmed("GENERATOR_DRAFT_TIMEOUT_SECS") {
            None => DEFAULT_DRAFT_TIMEOUT_SECS,
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|s| (1..=95).contains(s))
                .context("GENERATOR_DRAFT_TIMEOUT_SECS must be a number of seconds from 1 to 95")?,
        };
        let cfg = Self {
            provider_id,
            model: trimmed("GENERATOR_MODEL"),
            draft_timeout: Duration::from_secs(secs),
        };
        if cfg.provider_id.is_none() {
            tracing::info!(
                "generator provider not configured (GENERATOR_PROVIDER_ID unset): platform drafting is off"
            );
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(vars: &[(&str, &str)]) -> Result<GeneratorConfig> {
        let m: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        GeneratorConfig::from_lookup(&|k: &str| m.get(k).cloned())
    }

    #[test]
    fn config_defaults_and_validation() {
        assert_eq!(cfg(&[]).unwrap(), GeneratorConfig::default());
        let id = "0190d7a0-0000-7000-8000-000000000001";
        let c = cfg(&[
            ("GENERATOR_PROVIDER_ID", id),
            ("GENERATOR_MODEL", "gpt-4.1"),
            ("GENERATOR_DRAFT_TIMEOUT_SECS", "30"),
        ])
        .unwrap();
        assert_eq!(c.provider_id, Some(Uuid::parse_str(id).unwrap()));
        assert_eq!(c.model.as_deref(), Some("gpt-4.1"));
        assert_eq!(c.draft_timeout, Duration::from_secs(30));
        assert!(cfg(&[("GENERATOR_PROVIDER_ID", "nope")]).is_err());
        assert!(cfg(&[("GENERATOR_DRAFT_TIMEOUT_SECS", "120")]).is_err());
    }
}

pub mod accept;
pub mod evidence;
pub mod findings;
pub mod guard;
pub mod operation;
pub mod package;
pub mod proposal;

pub mod drive;
