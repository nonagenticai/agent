//! The `stub` transport: replays the template op's recorded fixtures (`{name, status, outcome, body, match?}`),
//! never touching the network. The rendered request picks a fixture with the [`FIXTURE_HEADER`] header — by name,
//! or by the fault selectors `timeout` (status 0), `5xx`, `not_found`, or by outcome. Without the header the
//! request itself selects: (a) the first 2xx fixture whose `match` (`{"path"?, "query"?: {k: v}}`) the rendered
//! request satisfies, else (b) the first 2xx fixture with no `match`, else (c) — the op has input-keyed fixtures
//! and none matched — the `not_found` fixture (404). A status-0 fixture is a timeout: `upstream_unavailable`.
use serde_json::Value;

use super::misconfigured;
use crate::connectors::wire::{OpError, RawResponse, RenderedRequest};

/// Selects a stub fixture; stripped from every real (rest/graphql) request.
pub const FIXTURE_HEADER: &str = "x-stub-fixture";

pub fn replay(req: &RenderedRequest, fixtures: &[Value]) -> Result<RawResponse, OpError> {
    let selector = req
        .headers
        .iter()
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(FIXTURE_HEADER))
        .map(|(_, v)| v.trim());
    let chosen = match selector {
        Some(sel) => select(fixtures, sel)
            .ok_or_else(|| misconfigured(format!("no stub fixture matches \"{sel}\"")))?,
        None => by_request(req, fixtures)?,
    };
    respond(chosen)
}

/// The op's sample answer when there is no call to match (a stub contract test, a sample fetch): the first 2xx
/// fixture, input-keyed or not.
pub fn replay_sample(fixtures: &[Value]) -> Result<RawResponse, OpError> {
    let chosen = fixtures
        .iter()
        .find(|f| is_2xx(f))
        .ok_or_else(|| misconfigured("the operation has no 2xx stub fixture"))?;
    respond(chosen)
}

fn respond(chosen: &Value) -> Result<RawResponse, OpError> {
    let status = status_of(chosen)
        .ok_or_else(|| misconfigured("the selected stub fixture has no status"))?;
    if status == 0 {
        return Err(OpError::new(
            "upstream_unavailable",
            "the upstream timed out (stub fault fixture)",
        ));
    }
    Ok(RawResponse {
        status,
        body: chosen.get("body").cloned().unwrap_or(Value::Null),
        retry_after_secs: None,
    })
}

/// The fixture's `match`, when it has one (`null` counts as none).
fn match_of(f: &Value) -> Option<&Value> {
    f.get("match").filter(|m| !m.is_null())
}

fn is_2xx(f: &Value) -> bool {
    status_of(f).is_some_and(|s| (200..300).contains(&s))
}

/// Header-free selection: rules (a), (b), (c) of the module doc.
fn by_request<'a>(req: &RenderedRequest, fixtures: &'a [Value]) -> Result<&'a Value, OpError> {
    if let Some(f) = fixtures
        .iter()
        .find(|f| is_2xx(f) && match_of(f).is_some_and(|m| matches(m, req)))
    {
        return Ok(f);
    }
    if let Some(f) = fixtures.iter().find(|f| is_2xx(f) && match_of(f).is_none()) {
        return Ok(f);
    }
    if fixtures.iter().any(|f| match_of(f).is_some()) {
        return select(fixtures, "not_found")
            .ok_or_else(|| misconfigured("no stub fixture matches the request"));
    }
    Err(misconfigured("the operation has no 2xx stub fixture"))
}

/// Whether the rendered request satisfies a fixture's `match`: `path` equal, and every listed `query` pair present
/// with an equal value. A `match` that is not an object, or a non-string query value, never matches.
fn matches(m: &Value, req: &RenderedRequest) -> bool {
    let Some(m) = m.as_object() else {
        return false;
    };
    if let Some(path) = m.get("path")
        && path.as_str() != Some(req.path.as_str())
    {
        return false;
    }
    if let Some(query) = m.get("query") {
        let Some(query) = query.as_object() else {
            return false;
        };
        for (k, v) in query {
            let Some(v) = v.as_str() else {
                return false;
            };
            if !req.query.iter().any(|(rk, rv)| rk == k && rv == v) {
                return false;
            }
        }
    }
    true
}

fn select<'a>(fixtures: &'a [Value], sel: &str) -> Option<&'a Value> {
    if let Some(f) = fixtures.iter().find(|f| text(f, "name") == Some(sel)) {
        return Some(f);
    }
    match sel {
        "timeout" => fixtures.iter().find(|f| status_of(f) == Some(0)),
        "5xx" => fixtures
            .iter()
            .find(|f| status_of(f).is_some_and(|s| (500..600).contains(&s))),
        "not_found" => fixtures
            .iter()
            .find(|f| status_of(f) == Some(404) || text(f, "outcome") == Some("not_found")),
        _ => fixtures.iter().find(|f| text(f, "outcome") == Some(sel)),
    }
}

fn text<'a>(f: &'a Value, key: &str) -> Option<&'a str> {
    f.get(key).and_then(Value::as_str)
}

/// The fixture's status: a number, or the leading digits of a string such as `200 graphql`.
fn status_of(f: &Value) -> Option<u16> {
    match f.get("status")? {
        Value::Number(n) => n.as_u64().and_then(|s| u16::try_from(s).ok()),
        Value::String(s) => {
            let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn req(path: &str, query: &[(&str, &str)]) -> RenderedRequest {
        RenderedRequest {
            method: "GET".into(),
            path: path.into(),
            query: query
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..RenderedRequest::default()
        }
    }

    fn keyed() -> Vec<Value> {
        vec![
            json!({"name": "all", "status": 200, "outcome": "ok", "body": {"n": "all"}}),
            json!({"name": "emma", "status": 200, "outcome": "ok", "body": {"n": "emma"},
                   "match": {"query": {"email": "emma@example.com"}}}),
            json!({"name": "c2", "status": 200, "outcome": "ok", "body": {"n": "c2"},
                   "match": {"path": "/customers/c2"}}),
            json!({"name": "not found", "status": 404, "outcome": "not_found", "body": {}}),
        ]
    }

    fn body_n(r: Result<RawResponse, OpError>) -> String {
        let r = r.expect("a response");
        r.body
            .get("n")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn query_match_wins_over_the_unkeyed_fixture() {
        let f = keyed();
        let r = req("/customers", &[("email", "emma@example.com"), ("x", "1")]);
        assert_eq!(body_n(replay(&r, &f)), "emma");
    }

    #[test]
    fn path_match_is_exact() {
        let f = keyed();
        assert_eq!(body_n(replay(&req("/customers/c2", &[]), &f)), "c2");
        assert_eq!(body_n(replay(&req("/customers/c2/x", &[]), &f)), "all");
    }

    #[test]
    fn no_match_falls_back_to_the_unkeyed_fixture() {
        let f = keyed();
        let r = req("/customers", &[("email", "nobody@example.com")]);
        assert_eq!(body_n(replay(&r, &f)), "all");
    }

    #[test]
    fn keyed_only_op_answers_not_found_when_nothing_matches() {
        let f: Vec<Value> = keyed().into_iter().skip(1).collect();
        let r = replay(&req("/customers/c9", &[]), &f).expect("the 404 fixture");
        assert_eq!(r.status, 404);
        let no_404: Vec<Value> = keyed().into_iter().skip(1).take(2).collect();
        let e = replay(&req("/customers/c9", &[]), &no_404).expect_err("misconfigured");
        assert_eq!(e.code, "connector_misconfigured");
    }

    #[test]
    fn header_selection_ignores_match() {
        let f = keyed();
        let mut r = req("/customers/c2", &[]);
        r.headers
            .push((FIXTURE_HEADER.to_owned(), "emma".to_owned()));
        assert_eq!(body_n(replay(&r, &f)), "emma");
        r.headers = vec![(FIXTURE_HEADER.to_owned(), "not_found".to_owned())];
        assert_eq!(replay(&r, &f).expect("404").status, 404);
    }

    #[test]
    fn unkeyed_ops_keep_the_first_2xx_rule() {
        let f = vec![
            json!({"name": "t", "status": 0, "outcome": "timeout", "body": null}),
            json!({"name": "a", "status": 200, "outcome": "ok", "body": {"n": "a"}}),
            json!({"name": "b", "status": 201, "outcome": "ok", "body": {"n": "b"}, "match": null}),
        ];
        assert_eq!(body_n(replay(&RenderedRequest::default(), &f)), "a");
    }

    #[test]
    fn sample_is_the_first_2xx_even_when_keyed() {
        let f: Vec<Value> = keyed().into_iter().skip(1).collect();
        assert_eq!(body_n(replay_sample(&f)), "emma");
    }
}
