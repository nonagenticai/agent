//! T2 for a platform template (B7 T7 `runTemplateSandbox`): run the template live against the vendor sandbox the
//! operator configured, on demand only (never scheduled).
//!
//! Sandboxes come from `CONNECTOR_SANDBOXES` (crate::config): JSON `{"<template key>": {"base_url", "env",
//! "probes"}}`. `env` NAMES the process environment variable that holds the sandbox credential, a JSON object of the
//! template's `credential_schema` fields (secret fields → `{{secret.*}}`, the rest → `{{config.*}}`); it is read at
//! run time, never logged, never stored. `probes` are the test identifiers the ops' `{{canonical.*}}` inputs take
//! (crate::connectors::contract::probe_inputs reads them as `probe.<field>`).
//!
//! Every R op runs live; W ops run with their subject/summary/title tagged `[agent-contract-test]` (the T3 rule);
//! X and P ops are not run (irreversible / payment). Each mapped response is compared with the op's recorded T1 ok
//! fixture: a `field_map` path that matches the fixture but not the live body is drift. The result is a
//! `TierResult`: `red` when any call fails, else `drift` when any shape differs, else `green`.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::contract::{CallFailure, live_call};
use super::instances::InstanceRow;
use super::jsonpath::json_path;
use super::mapping;
use super::model::{ConnectorTemplate, SideEffect};
use super::opkey::op_key;
use super::transport::Transport;

/// One configured vendor sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    pub base_url: String,
    /// The NAME of the environment variable holding the credential JSON (never the value).
    pub env: String,
    #[serde(default)]
    pub probes: BTreeMap<String, String>,
}

/// Template key → sandbox.
pub type Sandboxes = BTreeMap<String, Sandbox>;

fn env_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_uppercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// Parses `CONNECTOR_SANDBOXES` (empty = none). The error names the variable and the entry, never a value.
pub fn parse(raw: &str) -> Result<Sandboxes, String> {
    if raw.trim().is_empty() {
        return Ok(Sandboxes::new());
    }
    let all: Sandboxes = serde_json::from_str(raw).map_err(|e| {
        format!(
            "CONNECTOR_SANDBOXES must be a JSON object {{\"<key>\": {{\"base_url\", \"env\", \"probes\"}}}} ({e})"
        )
    })?;
    for (key, s) in &all {
        if s.base_url.trim().is_empty() {
            return Err(format!("CONNECTOR_SANDBOXES[{key}]: base_url is empty"));
        }
        if !env_name_ok(&s.env) {
            return Err(format!(
                "CONNECTOR_SANDBOXES[{key}]: env must name an environment variable (A-Z, 0-9, _)"
            ));
        }
    }
    Ok(all)
}

/// A cheap, stable fingerprint of the credential text: the OAuth token cache key changes when the credential does.
fn credential_version(text: &str) -> i32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in text.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    i32::from_ne_bytes(h.to_ne_bytes())
}

/// The sandbox as a connector instance (platform scope, test env), so the T3 call path (render → netguard-pinned
/// transport) runs unchanged. The nil tenant matches no private-target allow-list entry: a sandbox is a public
/// https host.
fn instance_of(
    tpl: &ConnectorTemplate,
    sb: &Sandbox,
    config: Map<String, Value>,
    cred_version: i32,
) -> InstanceRow {
    let mut env_ids = config;
    for (field, value) in &sb.probes {
        env_ids.insert(format!("probe.{field}"), Value::String(value.clone()));
    }
    InstanceRow {
        tenant_id: Uuid::nil(),
        id: format!("sandbox-{}", tpl.key),
        label: format!("{} sandbox", tpl.label),
        template_scope: "platform".into(),
        template_key: tpl.key.clone(),
        template_version: tpl.version.clone(),
        env: "test".into(),
        base_url: sb.base_url.clone(),
        env_ids: Value::Object(env_ids),
        max_rps: None,
        stub: false,
        platform: false,
        status: "ok".into(),
        last_error: None,
        created_by: "platform".into(),
        created_at: Utc::now(),
        cred_updated_at: None,
        cred_fingerprint: Some("sandbox0".into()),
        cred_version: Some(cred_version),
    }
}

/// `TierResult` JSON.
fn tier(status: &str, at: DateTime<Utc>, note: String) -> Value {
    const MAX_NOTE: usize = 600;
    let note: String = if note.chars().count() > MAX_NOTE {
        format!(
            "{}\u{2026}",
            note.chars().take(MAX_NOTE).collect::<String>()
        )
    } else {
        note
    };
    json!({"status": status, "at": at, "note": note})
}

/// Runs `tpl` against `sb`. `credential` is the text of the variable `sb.env` names (`None` = unset).
pub async fn run(
    connectors: &Transport,
    tpl: &ConnectorTemplate,
    sb: &Sandbox,
    credential: Option<String>,
) -> Value {
    let at = Utc::now();
    let Some(text) = credential.filter(|t| !t.trim().is_empty()) else {
        return tier(
            "red",
            at,
            format!("The sandbox credential variable {} is not set.", sb.env),
        );
    };
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&text) else {
        return tier(
            "red",
            at,
            format!(
                "The sandbox credential variable {} is not a JSON object.",
                sb.env
            ),
        );
    };
    let mut secret = Map::new();
    let mut config = Map::new();
    for (name, value) in fields {
        let is_config = tpl
            .credential_schema
            .iter()
            .any(|f| f.name == name && !f.secret);
        if is_config {
            config.insert(name, value);
        } else {
            secret.insert(name, value);
        }
    }
    let inst = instance_of(tpl, sb, config, credential_version(&text));
    let mut ran = 0usize;
    let mut writes = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut drift: Vec<String> = Vec::new();
    for op in &tpl.ops {
        if matches!(op.side_effect, SideEffect::X | SideEffect::P) {
            continue;
        }
        let key = op_key(&op.op_ref());
        ran += 1;
        if op.side_effect == SideEffect::W {
            writes += 1;
        }
        let resp = match live_call(connectors, &inst, tpl, op, Some(&secret)).await {
            Ok(r) => r,
            Err(CallFailure::ProbeMissing(probe)) => {
                failures.push(format!("{key}: no probe value for {probe}"));
                continue;
            }
            Err(CallFailure::Op(e)) => {
                failures.push(format!("{key}: {}", e.code));
                continue;
            }
        };
        if let Err(e) = mapping::extract(op, &BTreeMap::new(), &resp) {
            failures.push(format!("{key}: {} (HTTP {})", e.code, resp.status));
            continue;
        }
        let recorded = op
            .fixtures
            .iter()
            .find(|f| (200..300).contains(&f.status))
            .map(|f| &f.body);
        if let Some(body) = recorded {
            for m in &op.field_map {
                if json_path(body, &m.path).is_ok() && json_path(&resp.body, &m.path).is_err() {
                    drift.push(format!("{key} {}", m.path));
                }
            }
        }
    }
    if ran == 0 {
        return tier(
            "red",
            at,
            "No R or W operation to run against the sandbox.".into(),
        );
    }
    if !failures.is_empty() {
        return tier(
            "red",
            at,
            format!(
                "{} of {ran} ops failed: {}",
                failures.len(),
                failures.join("; ")
            ),
        );
    }
    if !drift.is_empty() {
        return tier(
            "drift",
            at,
            format!(
                "live shape differs from the T1 fixtures: {}",
                drift.join("; ")
            ),
        );
    }
    tier(
        "green",
        at,
        format!("{ran} ops live against the sandbox ({writes} writes tagged agent-contract-test)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandboxes_parse_and_refuse_malformed_entries() {
        assert!(parse("").unwrap().is_empty());
        let ok = parse(
            r#"{"zendesk_support": {"base_url": "https://acme.zendesk.com", "env": "ZD_SANDBOX", "probes": {"case_id": "1"}}}"#,
        )
        .unwrap();
        assert_eq!(ok["zendesk_support"].probes["case_id"], "1");
        for bad in [
            "[",
            r#"{"k": {"base_url": "", "env": "X"}}"#,
            r#"{"k": {"base_url": "https://x.example", "env": "lower"}}"#,
            r#"{"k": {"base_url": "https://x.example", "env": "X", "token": "s"}}"#,
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains("CONNECTOR_SANDBOXES"), "{e}");
        }
    }

    #[test]
    fn credential_version_tracks_the_text() {
        assert_eq!(credential_version("a"), credential_version("a"));
        assert_ne!(credential_version("a"), credential_version("b"));
    }
}
