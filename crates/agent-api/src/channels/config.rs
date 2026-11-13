//! `agent-edge` configuration. The edge needs no database, no Keycloak and no licence key in phase A: only its
//! binds and one optional group per connector. A connector is on only when all its required variables are set; any
//! missing → off, logged by name at boot, never a crash. A set but malformed value fails the boot by name, never
//! echoing the value.
//!
//! D109 (KH T6): the edge never answers a customer with a model of its own. Without core (phase A) its local backend
//! is `Unconfigured` ("not available"); with core every answer is agent-api's runtime.
//!
//! B12: `EDGE_CORE_URL` + `EDGE_INTERNAL_KEY` (both or neither; the key ≥ 32 characters) connect the edge to
//! agent-api's internal seam (crate::channels::core). Half a pair fails the boot.
//!
//! B7 R9: with core on, the Frappe Helpdesk worker connector (crate::channels::helpdesk) is registered (its bindings
//! come from core). It calls tenant Frappe sites through crate::netguard: `CONNECTOR_PRIVATE_TARGETS` (the same JSON
//! array as agent-api's) admits an in-cluster Frappe for the listed tenants; unset = public https hosts only.
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::bindings::{Binding, BindingStore};
use super::connector::ChannelKind;
use super::core::{CoreLink, InternalKey, MIN_KEY_LEN};
use super::edge::{EdgeState, Registry};
use super::{helpdesk, telegram, webchat};
use crate::config::number;
use crate::interaction::{InteractionBackend, Unconfigured};
use crate::netguard::Policy;
use crate::observability::{LogFormat, ObsConfig};

pub const DEFAULT_EDGE_BIND: &str = "0.0.0.0:8099";
pub const DEFAULT_EDGE_METRICS_BIND: &str = "0.0.0.0:9099";

pub struct EdgeConfig {
    /// `EDGE_BIND` (default `0.0.0.0:8099`).
    pub bind: String,
    /// `LOG_FORMAT`, `EDGE_METRICS_BIND` (default `0.0.0.0:9099`).
    pub obs: ObsConfig,
    /// `EDGE_ANSWER_TIMEOUT_S` (default 120): one answer's bound before the customer gets the fixed "try again".
    pub answer_timeout_s: u64,
    pub webchat: Option<webchat::WebChatConfig>,
    /// `TELEGRAM_CHANNEL_API_BASE` (B7 T15b). Telegram bindings are core-only (BYO bot), so the connector is
    /// registered exactly when `core` is set.
    pub telegram: telegram::TelegramConfig,
    /// `EDGE_CORE_URL` + `EDGE_INTERNAL_KEY` (B12); `None` = phase A.
    pub core: Option<(String, InternalKey)>,
    /// `WEBCHAT_PUBLIC_URL` (default [`webchat::DEFAULT_PUBLIC_URL`]): the web chat's origin when it runs on core
    /// bindings alone.
    pub webchat_public_url: String,
    /// `CONNECTOR_PRIVATE_TARGETS`: the outbound policy of the Frappe Helpdesk connector (empty = public https only).
    pub connector_policy: Policy,
    /// Per group, the variables that kept it off (names only).
    pub missing: Vec<(&'static str, Vec<&'static str>)>,
}

impl EdgeConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let trimmed = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let log_format = match trimmed("LOG_FORMAT")
            .map(|v| v.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("json") => LogFormat::Json,
            Some("pretty") => LogFormat::Pretty,
            Some(_) => anyhow::bail!("LOG_FORMAT must be json or pretty"),
        };
        let answer_timeout_s: u64 = number(&trimmed, "EDGE_ANSWER_TIMEOUT_S", 120)?;
        if answer_timeout_s < 1 {
            anyhow::bail!("EDGE_ANSWER_TIMEOUT_S must be at least 1");
        }
        let (webchat, webchat_missing) = webchat::config_from(&trimmed)?;
        let telegram = telegram::config_from(&trimmed)?;
        let core = match (trimmed("EDGE_CORE_URL"), trimmed("EDGE_INTERNAL_KEY")) {
            (None, None) => None,
            (Some(url), Some(key)) => {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    anyhow::bail!("EDGE_CORE_URL must be an http(s) URL");
                }
                let key = InternalKey::new(key).ok_or_else(|| {
                    anyhow::anyhow!("EDGE_INTERNAL_KEY must be at least {MIN_KEY_LEN} characters")
                })?;
                Some((url, key))
            }
            _ => anyhow::bail!("EDGE_CORE_URL and EDGE_INTERNAL_KEY must be set together"),
        };
        let connector_policy = Policy::from_env_json(
            trimmed("CONNECTOR_PRIVATE_TARGETS")
                .as_deref()
                .unwrap_or(""),
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        let webchat_public_url = trimmed("WEBCHAT_PUBLIC_URL")
            .and_then(|v| webchat::normalize_origin(&v))
            .unwrap_or_else(|| webchat::DEFAULT_PUBLIC_URL.into());
        let missing = [("web_chat", webchat.is_none(), webchat_missing)]
            .into_iter()
            .filter(|(_, off, _)| *off)
            .map(|(g, _, m)| (g, m))
            .collect();
        Ok(Self {
            bind: trimmed("EDGE_BIND").unwrap_or_else(|| DEFAULT_EDGE_BIND.into()),
            obs: ObsConfig {
                log_format,
                metrics_bind: trimmed("EDGE_METRICS_BIND")
                    .unwrap_or_else(|| DEFAULT_EDGE_METRICS_BIND.into()),
            },
            answer_timeout_s,
            webchat,
            telegram,
            core,
            webchat_public_url,
            connector_policy,
            missing,
        })
    }

    /// The edge's state: the backend the config names and every configured connector.
    pub fn build(&self) -> Result<EdgeState> {
        // D109: no customer answer from a model of the edge's own (phase A answers "not available")
        let backend: Arc<dyn InteractionBackend> = Arc::new(Unconfigured);
        let mut registry = Registry::default();
        let mut env = Vec::new();
        let groups = [(
            ChannelKind::WebChat,
            self.webchat.as_ref().map(|c| &c.bindings),
        )];
        for (kind, map) in groups {
            for (ext, scope) in map.into_iter().flatten() {
                env.push(Binding::from_env(kind, ext.clone(), scope.clone()));
            }
        }
        let bindings = Arc::new(BindingStore::new(env));
        let core = match &self.core {
            Some((url, key)) => Some(Arc::new(CoreLink::new(url.clone(), key.clone())?)),
            None => None,
        };
        // Telegram: one BYO bot per core binding (B7 T15b); nothing to serve without core.
        if core.is_some() {
            registry.register(Arc::new(telegram::TelegramConnector::new(
                self.telegram.clone(),
            )?));
        }
        // Frappe Helpdesk (B7 R9): worker side, bindings from core only.
        if core.is_some() {
            registry.register(Arc::new(helpdesk::Helpdesk::new(
                self.connector_policy.clone(),
                bindings.clone(),
            )));
        }
        let mut st = EdgeState::new(registry, backend);
        st.answer_deadline = Duration::from_secs(self.answer_timeout_s);
        st.webchat = self
            .webchat
            .as_ref()
            .map(|c| Arc::new(webchat::WebChat::new(c.clone())));
        st.bindings = bindings;
        st.core = core;
        // Web chat bindings may live in core alone: the routes are on, the env allow-list empty.
        if self.core.is_some() && st.webchat.is_none() {
            st.webchat = Some(Arc::new(webchat::WebChat::new(webchat::WebChatConfig {
                bindings: Default::default(),
                allowed_origins: Default::default(),
                public_url: self.webchat_public_url.clone(),
            })));
        }
        Ok(st)
    }

    /// One boot line per group: on (with binding counts) or off (with the missing names).
    pub fn log_status(&self) {
        for (group, missing) in &self.missing {
            tracing::info!(group, missing = ?missing, "edge_group_off");
        }
        match &self.core {
            Some((url, _)) => tracing::info!(core_url = %url, "edge_core_on"),
            None => tracing::info!("edge_core_off"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::connector::Side;
    use uuid::Uuid;

    #[test]
    fn edge_boots_with_nothing_configured() {
        let c = EdgeConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.bind, DEFAULT_EDGE_BIND);
        assert_eq!(c.obs.metrics_bind, DEFAULT_EDGE_METRICS_BIND);
        assert!(c.webchat.is_none());
        assert_eq!(c.missing.len(), 1);
        let st = c.build().unwrap();
        assert!(st.registry.statuses().is_empty());
    }

    #[test]
    fn bad_values_fail_by_name() {
        let e = EdgeConfig::from_lookup(|k| (k == "EDGE_ANSWER_TIMEOUT_S").then(|| "0".into()));
        assert!(e.is_err());
        let e = EdgeConfig::from_lookup(|k| (k == "LOG_FORMAT").then(|| "xml".into()));
        assert!(e.is_err());
    }

    #[test]
    fn core_pair_is_both_or_neither_and_the_key_is_long() {
        let url = "http://agent-api.example.com:8098";
        let key = "k".repeat(32);
        let only_url = EdgeConfig::from_lookup(|k| (k == "EDGE_CORE_URL").then(|| url.into()));
        assert!(only_url.is_err());
        let only_key = EdgeConfig::from_lookup(|k| (k == "EDGE_INTERNAL_KEY").then(|| key.clone()));
        assert!(only_key.is_err());
        let short = EdgeConfig::from_lookup(|k| match k {
            "EDGE_CORE_URL" => Some(url.into()),
            "EDGE_INTERNAL_KEY" => Some("short-secret-value".into()),
            _ => None,
        });
        let e = short.err().unwrap().to_string();
        assert!(e.contains("EDGE_INTERNAL_KEY") && !e.contains("short-secret-value"));
        let c = EdgeConfig::from_lookup(|k| match k {
            "EDGE_CORE_URL" => Some(url.into()),
            "EDGE_INTERNAL_KEY" => Some(key.clone()),
            _ => None,
        })
        .unwrap();
        assert!(c.core.is_some());
        let st = c.build().unwrap();
        assert!(st.core.is_some());
        assert!(st.webchat.is_some(), "web chat runs on core bindings");
        assert!(
            st.registry
                .get_side(ChannelKind::Helpdesk, Side::Worker)
                .is_some(),
            "helpdesk runs on core bindings"
        );
        assert!(st.registry.get(ChannelKind::Helpdesk).is_none());
    }

    #[test]
    fn connector_private_targets_load_or_fail_by_name() {
        let t = Uuid::from_u128(5);
        let raw = format!(
            r#"[{{"host":"frappe.example.com","port":8000,"scheme":"http","tenants":["{t}"]}}]"#
        );
        let c =
            EdgeConfig::from_lookup(|k| (k == "CONNECTOR_PRIVATE_TARGETS").then(|| raw.clone()))
                .unwrap();
        assert_eq!(c.connector_policy.private_targets.len(), 1);
        assert!(!c.connector_policy.allow_loopback);
        let e =
            EdgeConfig::from_lookup(|k| (k == "CONNECTOR_PRIVATE_TARGETS").then(|| "{nope".into()));
        assert!(
            e.err()
                .unwrap()
                .to_string()
                .contains("CONNECTOR_PRIVATE_TARGETS")
        );
        assert!(
            EdgeConfig::from_lookup(|_| None)
                .unwrap()
                .connector_policy
                .private_targets
                .is_empty()
        );
    }
}
