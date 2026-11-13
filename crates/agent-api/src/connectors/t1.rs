//! T1 for a platform template (B7 T7 publish): replay every op's recorded fixtures through the mapping engine
//! (crate::connectors::mapping) and the stub transport (crate::connectors::transport::stub), with no network.
//!
//! Per op: the request renders with sample inputs (every placeholder resolves, as the tenant T1 does). Per fixture
//! (selected by name with the stub's fixture header):
//! - a 2xx fixture whose outcome is `ok` (or free text) must map: `extract` succeeds (transforms, `enum_map`
//!   through the status map) and every `field_map` path matches the recorded body (the tenant T1 rule,
//!   crate::connectors::tenant_templates::t1_op);
//! - any other fixture (also a 2xx one recording a code: a `BODY` fault rule) must fail with the code it records:
//!   status 0 → `upstream_unavailable`; an outcome `"401 → connector_auth_failed"` → the code after the arrow;
//!   an outcome that is itself a code (`not_found`) → that code; free text → any error.
//!
//! An op without fixtures has nothing to replay and passes, as the TS fixture world's publish does (it records
//! `t1 = green` without looking at fixtures).
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use super::jsonpath::json_path;
use super::mapping::{self, RenderCtx};
use super::model::{ConnectorTemplate, ConnectorTemplateOp, TemplateFixture};
use super::opkey::op_key;
use super::transport::stub::{self, FIXTURE_HEADER};
use super::validate::placeholders;
use super::wire::RenderedRequest;

/// One fixture (or render) that did not replay as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct T1Failure {
    /// `object.verb{variant}`.
    pub op: String,
    /// The fixture name, or `render` when the request itself does not render.
    pub fixture: String,
    pub reason: String,
}

/// One T1 run over a template.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct T1Report {
    /// Fixtures replayed.
    pub replayed: usize,
    pub failures: Vec<T1Failure>,
}

impl T1Report {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// A typed sample for a canonical input (names only in the catalogue: numbers and flags by name).
fn sample_input(name: &str) -> Value {
    const NUMERIC: [&str; 6] = ["amount", "total", "price", "quantity", "count", "score"];
    if NUMERIC.iter().any(|n| name.contains(n)) {
        Value::from(1)
    } else if matches!(name, "granted" | "verified") {
        Value::Bool(true)
    } else {
        Value::String(format!("t1-{name}"))
    }
}

/// The op rendered with sample values for every placeholder of its path and request.
fn render_sample(op: &ConnectorTemplateOp) -> Result<RenderedRequest, String> {
    let mut canonical = Map::new();
    let mut config = Map::new();
    let mut secret = Map::new();
    for text in [op.path.as_str(), op.request.as_str()] {
        for (ns, name) in placeholders(text).iter().flat_map(|p| p.refs()) {
            match ns {
                "canonical" => {
                    canonical.insert(name.to_owned(), sample_input(name));
                }
                "secret" => {
                    secret.insert(name.to_owned(), Value::String("t1-sample-secret".into()));
                }
                _ => {
                    config.insert(name.to_owned(), Value::String(format!("t1-{name}")));
                }
            }
        }
    }
    let ctx = RenderCtx {
        canonical: &canonical,
        config: &config,
        env: &config,
        secret: &secret,
        idempotency_key: "t1-idempotency-key",
    };
    mapping::render(op, &ctx).map_err(|e| format!("does not render: {}", e.detail))
}

/// The error code a non-2xx fixture records, `None` when any error is the expected outcome.
fn expected_code(f: &TemplateFixture) -> Option<String> {
    if f.status == 0 {
        return Some("upstream_unavailable".into());
    }
    let outcome = f.outcome.trim();
    if let Some((_, code)) = outcome
        .rsplit_once('\u{2192}')
        .or_else(|| outcome.rsplit_once("->"))
    {
        return Some(code.trim().to_owned());
    }
    let is_code = !outcome.is_empty()
        && outcome
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    is_code.then(|| outcome.to_owned())
}

fn replay_fixture(
    op: &ConnectorTemplateOp,
    req: &RenderedRequest,
    all: &[Value],
    f: &TemplateFixture,
) -> Result<(), String> {
    let mut req = req.clone();
    req.headers
        .push((FIXTURE_HEADER.to_owned(), f.name.clone()));
    let mapped = match stub::replay(&req, all) {
        Ok(resp) => {
            // A 2xx fixture recording an error code (a `BODY` fault rule, e.g. a gateway's `STATUS:FAILED`)
            // must fail with that code like a non-2xx one; every other 2xx fixture must map.
            let ok = (200..300).contains(&resp.status)
                && expected_code(f).is_none_or(|code| code == "ok");
            let out = mapping::extract(op, &BTreeMap::new(), &resp);
            if ok {
                out.map_err(|e| format!("does not map: {} ({})", e.code, e.detail))?;
                for m in &op.field_map {
                    json_path(&resp.body, &m.path)
                        .map_err(|e| format!("{} \u{2192} {}: {e}", m.path, m.field))?;
                }
                return Ok(());
            }
            out
        }
        Err(e) => Err(e),
    };
    let want = expected_code(f);
    match (mapped, want) {
        (Ok(_), _) => Err(format!(
            "HTTP {} mapped as a success; the fixture records \u{201c}{}\u{201d}",
            f.status, f.outcome
        )),
        (Err(e), Some(code)) if e.code != code => {
            Err(format!("mapped to {}, the fixture records {code}", e.code))
        }
        (Err(_), _) => Ok(()),
    }
}

/// Replays every fixture of every op of `t`.
pub fn replay(t: &ConnectorTemplate) -> T1Report {
    let mut report = T1Report::default();
    for op in &t.ops {
        let key = op_key(&op.op_ref());
        if op.fixtures.is_empty() {
            continue;
        }
        let req = match render_sample(op) {
            Ok(r) => r,
            Err(reason) => {
                report.failures.push(T1Failure {
                    op: key,
                    fixture: "render".into(),
                    reason,
                });
                continue;
            }
        };
        let all: Vec<Value> = op
            .fixtures
            .iter()
            .filter_map(|f| serde_json::to_value(f).ok())
            .collect();
        for f in &op.fixtures {
            report.replayed += 1;
            if let Err(reason) = replay_fixture(op, &req, &all, f) {
                report.failures.push(T1Failure {
                    op: key.clone(),
                    fixture: f.name.clone(),
                    reason,
                });
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expected_codes_follow_the_recorded_outcome() {
        let f = |status: u16, outcome: &str| TemplateFixture {
            name: "x".into(),
            status,
            outcome: outcome.into(),
            body: Value::Null,
            match_on: None,
        };
        assert_eq!(
            expected_code(&f(401, "401 \u{2192} connector_auth_failed")).as_deref(),
            Some("connector_auth_failed")
        );
        assert_eq!(
            expected_code(&f(404, "not_found")).as_deref(),
            Some("not_found")
        );
        assert_eq!(
            expected_code(&f(0, "timeout (fault injection)")).as_deref(),
            Some("upstream_unavailable")
        );
        assert_eq!(expected_code(&f(500, "vendor outage, retried")), None);
    }
}
