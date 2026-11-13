//! The `graphql` transport: POST `{query, variables}` through the `rest` engine. An HTTP 2xx answer carrying a
//! non-empty `errors[]` is an error (by the first error's `extensions.code`); a non-empty `data.*.userErrors[]`
//! is `invalid_input`. Other statuses come back as the [`RawResponse`] for the mapping engine.
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::auth::AuthSpec;
use super::{CallTarget, Transport, misconfigured, rest};
use crate::connectors::wire::{CallSecrets, OpError, RawResponse, RenderedRequest};

pub async fn call(
    t: &Transport,
    target: &CallTarget,
    auth: &AuthSpec,
    secrets: &CallSecrets,
    req: &RenderedRequest,
    timeout: Duration,
    write: bool,
) -> Result<RawResponse, OpError> {
    let mut post = req.clone();
    post.method = "POST".into();
    post.body = Some(document(req.body.as_ref())?);
    let resp = rest::call(t, target, auth, secrets, &post, timeout, write).await?;
    check(&resp)?;
    Ok(resp)
}

/// The request document: a rendered body `{query, variables?, operationName?}` or a bare query string.
pub fn document(body: Option<&Value>) -> Result<Value, OpError> {
    match body {
        Some(Value::String(q)) if !q.trim().is_empty() => Ok(json!({"query": q, "variables": {}})),
        Some(Value::Object(o))
            if o.get("query")
                .and_then(Value::as_str)
                .is_some_and(|q| !q.trim().is_empty()) =>
        {
            let mut doc = Map::new();
            doc.insert("query".into(), o["query"].clone());
            doc.insert(
                "variables".into(),
                o.get("variables")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
            if let Some(name) = o.get("operationName") {
                doc.insert("operationName".into(), name.clone());
            }
            Ok(Value::Object(doc))
        }
        _ => Err(misconfigured(
            "a graphql operation must render to {query, variables} or a query string",
        )),
    }
}

/// A GraphQL `extensions.code` safe to echo: `[A-Z0-9_]{1,64}` (never free upstream text).
fn safe_code(code: &str) -> Option<&str> {
    (!code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
    .then_some(code)
}

/// The GraphQL-level error of a 2xx answer, if any.
pub fn check(resp: &RawResponse) -> Result<(), OpError> {
    if !(200..300).contains(&resp.status) {
        return Ok(());
    }
    if let Some(first) = resp
        .body
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        let code = first
            .pointer("/extensions/code")
            .and_then(Value::as_str)
            .map(str::to_ascii_uppercase)
            .unwrap_or_default();
        let canonical = match code.as_str() {
            "UNAUTHENTICATED" => "connector_auth_failed",
            "THROTTLED" => "rate_limited",
            _ => "upstream_unavailable",
        };
        let detail = match safe_code(&code) {
            Some(c) => format!("the GraphQL endpoint answered with errors (code {c})"),
            None => "the GraphQL endpoint answered with errors".to_string(),
        };
        return Err(OpError::new(canonical, detail));
    }
    if let Some(data) = resp.body.get("data").and_then(Value::as_object) {
        let user_errors: usize = data
            .values()
            .filter_map(|v| v.get("userErrors").and_then(Value::as_array))
            .map(Vec::len)
            .sum();
        if user_errors > 0 {
            return Err(OpError::new(
                "invalid_input",
                format!("the GraphQL mutation answered with {user_errors} user error(s)"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(body: Value) -> RawResponse {
        RawResponse {
            status: 200,
            body,
            retry_after_secs: None,
        }
    }

    #[test]
    fn errors_and_user_errors() {
        assert!(check(&ok(json!({"data": {"shop": {"name": "x"}}}))).is_ok());
        assert!(check(&ok(json!({"data": {"shop": {}}, "errors": []}))).is_ok());
        let e = check(&ok(
            json!({"errors": [{"message": "secret text", "extensions": {"code": "UNAUTHENTICATED"}}]}),
        ))
        .unwrap_err();
        assert_eq!(e.code, "connector_auth_failed");
        assert!(!e.detail.contains("secret text"));
        assert_eq!(
            check(&ok(json!({"errors": [{"message": "boom"}]})))
                .unwrap_err()
                .code,
            "upstream_unavailable"
        );
        assert_eq!(
            check(&ok(
                json!({"data": {"productCreate": {"userErrors": [{"field": ["title"], "message": "blank"}]}}})
            ))
            .unwrap_err()
            .code,
            "invalid_input"
        );
        let mut not_2xx = ok(json!({"errors": [{"message": "x"}]}));
        not_2xx.status = 500;
        assert!(check(&not_2xx).is_ok(), "non-2xx is the mapping engine's");
    }

    #[test]
    fn document_shapes() {
        assert_eq!(
            document(Some(&json!("{ shop { name } }"))).unwrap(),
            json!({"query": "{ shop { name } }", "variables": {}})
        );
        assert_eq!(
            document(Some(&json!({"query": "q", "variables": {"a": 1}}))).unwrap(),
            json!({"query": "q", "variables": {"a": 1}})
        );
        assert!(document(None).is_err());
        assert!(document(Some(&json!({"variables": {}}))).is_err());
    }
}
