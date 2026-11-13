//! B7 T5: the declarative mapping engine. [`render`] fills a template op's `path` and `request` from the call's
//! canonical input, config, env IDs and secrets (`{{canonical.f}} {{config.f}} {{env.f}} {{secret.f}}
//! {{step.idempotency_key}}`, the UI namespaces); [`extract`] maps a raw response back (fault map first for
//! non-2xx; for 2xx the fault map's `BODY` rules, then field map + transforms + status map); [`reduce`] trims the
//! result to the op's output fields.
//! No logic beyond substitution, JSONPath and the closed transform list.
use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};

use super::jsonpath::{js_space, json_path};
use super::model::{ConnectorTemplateOp, Idempotency, StatusMapEntry};
use super::validate::{Placeholder, kv_key, placeholders};
use super::wire::{OpError, RawResponse, RenderedRequest};

/// Longest string a reduced result keeps (chars).
pub const MAX_STRING_CHARS: usize = 2000;
/// Longest list a reduced result keeps.
pub const MAX_LIST_ITEMS: usize = 20;

/// The values one call renders from. Deliberately not `Debug`: `secret` holds decrypted credentials.
pub struct RenderCtx<'a> {
    pub canonical: &'a Map<String, Value>,
    pub config: &'a Map<String, Value>,
    pub env: &'a Map<String, Value>,
    pub secret: &'a Map<String, Value>,
    pub idempotency_key: &'a str,
}

fn lookup(ctx: &RenderCtx<'_>, ns: &str, name: &str) -> Result<Value, OpError> {
    let found = match ns {
        "canonical" => ctx.canonical.get(name).cloned(),
        "config" => ctx.config.get(name).cloned(),
        "env" => ctx.env.get(name).cloned(),
        "secret" => ctx.secret.get(name).cloned(),
        "step" if name == "idempotency_key" => {
            (!ctx.idempotency_key.is_empty()).then(|| Value::String(ctx.idempotency_key.to_owned()))
        }
        _ => {
            return Err(OpError::new(
                "connector_misconfigured",
                format!("unknown_placeholder {{{{{ns}.{name}}}}}"),
            ));
        }
    };
    match found {
        Some(v) if !v.is_null() => Ok(v),
        _ => Err(OpError::new(
            "invalid_input",
            format!("missing value for {{{{{ns}.{name}}}}}"),
        )),
    }
}

/// The value of a placeholder: its primary `ns.name`, else (absent or null) its `?? fallback` (R10), else the
/// primary's `invalid_input`.
fn lookup_placeholder(ctx: &RenderCtx<'_>, p: &Placeholder<'_>) -> Result<Value, OpError> {
    match (lookup(ctx, p.ns, p.name), p.fallback) {
        (Err(e), Some((ns, name))) if e.code == "invalid_input" => lookup(ctx, ns, name),
        (found, _) => found,
    }
}

/// True when `p` is optional and has no value (primary and fallback both absent or null): the caller drops what it
/// fills. A placeholder with a value, or any other lookup failure, is `false` / an error as for a required one.
fn optional_missing(ctx: &RenderCtx<'_>, p: &Placeholder<'_>) -> Result<bool, OpError> {
    if !p.optional {
        return Ok(false);
    }
    match lookup_placeholder(ctx, p) {
        Ok(_) => Ok(false),
        Err(e) if e.code == "invalid_input" => Ok(true),
        Err(e) => Err(e),
    }
}

/// Marks a JSON object member dropped by [`render_json_text`] (an optional whole-string value with no value); removed by
/// [`prune_omitted`]. The NUL cannot occur in a template, so it cannot collide with real text.
const OMIT: &str = "\u{0}omit-optional";

/// Removes every object member marked [`OMIT`]; true when one is left (in an array or at the top: not droppable).
fn prune_omitted(v: &mut Value) -> bool {
    match v {
        Value::Object(m) => {
            m.retain(|_, x| x.as_str() != Some(OMIT));
            // every member is pruned (no short-circuit): any() would stop at the first survivor
            let mut left = false;
            for x in m.values_mut() {
                left |= prune_omitted(x);
            }
            left
        }
        Value::Array(a) => a
            .iter_mut()
            .any(|x| x.as_str() == Some(OMIT) || prune_omitted(x)),
        other => other.as_str() == Some(OMIT),
    }
}

/// A value as text: strings verbatim, everything else as compact JSON.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// RFC 3986 percent-encoding of everything but the unreserved set (so `/` in a value stays inside its segment).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Renders a URL part (path or a query key/value). A `{{secret.*}}` is refused (`secret_in_url`); path values are
/// percent-encoded, query values stay raw (the transport encodes them).
fn render_url_part(s: &str, ctx: &RenderCtx<'_>, encode: bool) -> Result<String, OpError> {
    let mut out = String::new();
    let mut from = 0;
    for p in placeholders(s) {
        out.push_str(s.get(from..p.start).unwrap_or_default());
        if p.refs().any(|(ns, _)| ns == "secret") {
            return Err(OpError::new("connector_misconfigured", "secret_in_url"));
        }
        let text = value_text(&lookup_placeholder(ctx, &p)?);
        if encode {
            out.push_str(&encode_segment(&text));
        } else {
            out.push_str(&text);
        }
        from = p.end;
    }
    out.push_str(s.get(from..).unwrap_or_default());
    Ok(out)
}

/// Renders a non-JSON request (a GraphQL document): placeholders become their text.
fn render_text(s: &str, ctx: &RenderCtx<'_>) -> Result<String, OpError> {
    let mut out = String::new();
    let mut from = 0;
    for p in placeholders(s) {
        out.push_str(s.get(from..p.start).unwrap_or_default());
        out.push_str(&value_text(&lookup_placeholder(ctx, &p)?));
        from = p.end;
    }
    out.push_str(s.get(from..).unwrap_or_default());
    Ok(out)
}

/// Renders a JSON request template to JSON text. A placeholder that is a whole JSON string value (`"{{x.y}}"`)
/// becomes the source value with its JSON type; inside a longer string it is spliced in escaped; outside a string
/// it is written as JSON.
fn render_json_text(s: &str, ctx: &RenderCtx<'_>) -> Result<String, OpError> {
    let starts: BTreeMap<usize, Placeholder<'_>> =
        placeholders(s).into_iter().map(|p| (p.start, p)).collect();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut open_at = 0;
    let mut i = 0;
    while i < s.len() {
        if let Some(p) = starts.get(&i) {
            let end = p.end;
            let whole =
                in_string && !escaped && out.len() == open_at + 1 && bytes.get(end) == Some(&b'"');
            // An optional whole-string value with no value is dropped from its object (see `OMIT`); any other
            // missing optional is a missing required value.
            let val = if whole && optional_missing(ctx, p)? {
                Value::String(OMIT.to_owned())
            } else {
                lookup_placeholder(ctx, p)?
            };
            if in_string && !escaped {
                if whole {
                    out.truncate(open_at);
                    out.push_str(&val.to_string());
                    in_string = false;
                    i = end + 1;
                } else {
                    let quoted = Value::String(value_text(&val)).to_string();
                    out.push_str(quoted.get(1..quoted.len() - 1).unwrap_or_default());
                    i = end;
                }
                continue;
            }
            if !in_string {
                out.push_str(&val.to_string());
                i = end;
                continue;
            }
        }
        let Some(c) = s.get(i..).and_then(|r| r.chars().next()) else {
            break;
        };
        out.push(c);
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            open_at = out.len() - 1;
        }
        i += c.len_utf8();
    }
    Ok(out)
}

/// The request body: `None` for an empty template, the rendered JSON when the template is JSON, else the rendered
/// document as a JSON string (a GraphQL query; the graphql transport wraps it with its variables).
fn render_body(request: &str, ctx: &RenderCtx<'_>) -> Result<Option<Value>, OpError> {
    let trimmed = request.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let text = render_json_text(trimmed, ctx)?;
        if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
            if prune_omitted(&mut v) {
                return Err(OpError::new(
                    "invalid_input",
                    "an optional placeholder with no value is not a JSON object member",
                ));
            }
            return Ok(Some(v));
        }
    }
    Ok(Some(Value::String(render_text(trimmed, ctx)?)))
}

/// Byte index of the `?` that starts the query: the first one outside a `{{…}}` placeholder (a `??` fallback is not
/// a query start).
pub(super) fn query_start(raw: &str) -> Option<usize> {
    let mut depth = 0usize;
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"{{") {
            depth += 1;
            i += 2;
        } else if depth > 0 && b[i..].starts_with(b"}}") {
            depth -= 1;
            i += 2;
        } else if depth == 0 && b[i] == b'?' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// Renders one template op into a request relative to the instance base URL. `path` may carry `?a=b&c=d`, which is
/// split into `query` pairs (values raw; the transport encodes them). Auth headers are the transport's job; the only
/// header added here is the op's idempotency header.
pub fn render(op: &ConnectorTemplateOp, ctx: &RenderCtx<'_>) -> Result<RenderedRequest, OpError> {
    let raw = op.path.trim();
    let (path_part, query_part) = match query_start(raw) {
        Some(i) => (&raw[..i], Some(&raw[i + 1..])),
        None => (raw, None),
    };
    let path = render_url_part(path_part, ctx, true)?;
    let mut query = Vec::new();
    for pair in query_part
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        // A pair whose value is exactly one optional placeholder with no value is dropped, key and all.
        if let [p] = placeholders(v).as_slice()
            && p.start == 0
            && p.end == v.len()
            && !p.refs().any(|(ns, _)| ns == "secret")
            && optional_missing(ctx, p)?
        {
            continue;
        }
        query.push((
            render_url_part(k, ctx, false)?,
            render_url_part(v, ctx, false)?,
        ));
    }
    let mut headers = Vec::new();
    if let Idempotency::Header { header } = &op.idempotency {
        let key = lookup(ctx, "step", "idempotency_key")?;
        headers.push((header.clone(), value_text(&key)));
    }
    Ok(RenderedRequest {
        method: op.method.trim().to_ascii_uppercase(),
        path,
        query,
        headers,
        body: render_body(&op.request, ctx)?,
    })
}

/// A `HTTP <code>` / `HTTP <n>xx` fault-map entry (any suffix after the code is descriptive and not matched).
enum HttpMatch {
    Exact(u16),
    Class(u16),
}

fn http_match(system: &str) -> Option<HttpMatch> {
    let rest = system.trim().strip_prefix("HTTP")?.trim_start();
    let code = rest.split_whitespace().next()?;
    let b = code.as_bytes();
    if b.len() != 3 {
        return None;
    }
    if b.iter().all(u8::is_ascii_digit) {
        return code.parse().ok().map(HttpMatch::Exact);
    }
    let first = *b.first()?;
    if (b'1'..=b'5').contains(&first) && b.get(1..)?.eq_ignore_ascii_case(b"xx") {
        return Some(HttpMatch::Class(u16::from(first - b'0')));
    }
    None
}

/// The canonical error codes a fault map may name (`error_state_def` + the connector-only ones).
const CANONICAL_CODES: [&str; 25] = [
    "ambiguous",
    "attempts_exceeded",
    "case_cap_reached",
    "conflict",
    "connector_auth_failed",
    "connector_misconfigured",
    "duplicate",
    "identity_required",
    "ineligible",
    "invalid_input",
    "invalid_state",
    "invalid_target",
    "invalid_url",
    "locked",
    "not_entitled",
    "not_found",
    "outside_hours",
    "postcondition_failed",
    "precondition_unmet",
    "rate_limited",
    "step_up_required",
    "success_criteria_unmet",
    "timeout",
    "unknown_outcome",
    "upstream_unavailable",
];

/// The default fault set: 401, 403, 404, 400/422, 429, 5xx and no response (0).
fn default_fault(status: u16) -> OpError {
    let code = match status {
        401 => "connector_auth_failed",
        403 => "connector_misconfigured",
        404 => "not_found",
        400 | 422 => "invalid_input",
        429 => "rate_limited",
        _ => "upstream_unavailable",
    };
    let detail = if status == 0 {
        "no_response".to_owned()
    } else {
        format!("http_{status}")
    };
    OpError::new(code, detail)
}

/// A fault-map code as an error: a canonical code verbatim, anything else `upstream_unavailable`.
fn mapped_fault(code: &str, detail: String) -> OpError {
    match CANONICAL_CODES.iter().copied().find(|c| *c == code) {
        Some(code) => OpError::new(code, detail),
        None => OpError::new(
            "upstream_unavailable",
            format!("{detail} (non-canonical fault code)"),
        ),
    }
}

/// TS `kvLine`: the value of the first `KEY:value` line of `text` (lines split on `\n`, leading whitespace of a
/// line and whitespace around the value ignored, so CRLF output reads the same), `None` when no line has the key.
pub fn kv_line(text: &str, key: &str) -> Option<String> {
    text.split('\n').find_map(|line| {
        line.trim_start_matches(js_space)
            .strip_prefix(key)?
            .strip_prefix(':')
            .map(|v| v.trim_matches(js_space).to_owned())
    })
}

/// TS `matchBodyFault`: does a `BODY <jsonpath> [<transform>] = <value>` fault-map entry match a 2xx body? The
/// path must match one scalar; `<transform>` is `none` or `kv_line:<KEY>`; the (transformed) string, number or
/// boolean must equal `<value>` as text (trimmed). Any other `system` text never matches. This is how a gateway that
/// answers HTTP 2xx for a failed action (a script gateway's `STATUS:FAILED`) is classified as a fault:
/// `{"system": "BODY $.stdout kv_line:STATUS = FAILED", "code": "upstream_unavailable"}`.
pub fn body_fault_matches(system: &str, body: &Value) -> bool {
    let s = system.trim_matches(js_space);
    let Some(rest) = s.strip_prefix("BODY") else {
        return false;
    };
    if !rest.starts_with(js_space) {
        return false;
    }
    let Some((lhs, want)) = rest.split_once('=') else {
        return false;
    };
    let parts: Vec<&str> = lhs.split(js_space).filter(|p| !p.is_empty()).collect();
    let (path, transform) = match parts.as_slice() {
        [path] => (*path, None),
        [path, t] => (*path, Some(*t)),
        _ => return false,
    };
    let Ok(mut v) = json_path(body, path) else {
        return false;
    };
    if let Some(t) = transform.filter(|t| *t != "none") {
        let Some(key) = kv_key(t) else {
            return false;
        };
        let Some(found) = v.as_str().and_then(|text| kv_line(text, key)) else {
            return false;
        };
        v = Value::String(found);
    }
    let text = match &v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return false,
    };
    text == want.trim_matches(js_space)
}

/// Maps a non-2xx status: the first template fault-map entry naming that status wins, else the default set.
fn fault(op: &ConnectorTemplateOp, status: u16) -> OpError {
    for e in &op.fault_map {
        let hit = match http_match(&e.system) {
            Some(HttpMatch::Exact(c)) => c == status,
            Some(HttpMatch::Class(c)) => status / 100 == c,
            None => false,
        };
        if hit {
            return mapped_fault(&e.code, format!("http_{status}"));
        }
    }
    default_fault(status)
}

fn transform_failed(field: &str, transform: &str) -> OpError {
    OpError::new(
        "upstream_unavailable",
        format!("transform_failed {field} ({transform})"),
    )
}

fn is_decimal(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = match digits.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (digits, None),
    };
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

fn iso_date(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(
                    dt.with_timezone(&Utc)
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                );
            }
            for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
                if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
                    return Some(n.and_utc().to_rfc3339_opts(SecondsFormat::Secs, true));
                }
            }
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }
        Value::Number(n) => n
            .as_i64()
            .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
            .map(|d| d.to_rfc3339_opts(SecondsFormat::Secs, true)),
        _ => None,
    }
}

/// One scalar through a transform (arrays are mapped element-wise by [`apply`]).
fn apply_scalar(
    t: &str,
    v: Value,
    status_map: &[StatusMapEntry],
    field: &str,
) -> Result<Value, OpError> {
    let fail = || transform_failed(field, t);
    Ok(match t {
        "trim" | "lowercase" | "uppercase" => match v {
            Value::String(s) => Value::String(match t {
                "trim" => s.trim().to_owned(),
                "lowercase" => s.to_lowercase(),
                _ => s.to_uppercase(),
            }),
            other => other,
        },
        "to_int" => {
            let n = match &v {
                Value::Number(n) => n.as_i64().or_else(|| {
                    n.as_f64()
                        .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                        .map(|f| f as i64)
                }),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            };
            Value::from(n.ok_or_else(fail)?)
        }
        "to_decimal" => match &v {
            Value::Number(n) => Value::String(n.to_string()),
            Value::String(s) if is_decimal(s.trim()) => Value::String(s.trim().to_owned()),
            _ => return Err(fail()),
        },
        "iso_date" => Value::String(iso_date(&v).ok_or_else(fail)?),
        "enum_map" => {
            let key = match &v {
                Value::String(s) => s.clone(),
                Value::Number(_) | Value::Bool(_) => v.to_string(),
                _ => return Err(OpError::new("upstream_unavailable", "unmapped_status")),
            };
            match status_map.iter().find(|e| e.system == key) {
                Some(e) => Value::String(e.canonical.clone()),
                None => return Err(OpError::new("upstream_unavailable", "unmapped_status")),
            }
        }
        "mask_last4" => match v {
            Value::String(_) | Value::Number(_) => {
                let text = value_text(&v);
                let chars: Vec<char> = text.chars().collect();
                let tail: String = if chars.len() > 4 {
                    chars
                        .get(chars.len() - 4..)
                        .unwrap_or_default()
                        .iter()
                        .collect()
                } else {
                    String::new()
                };
                Value::String(format!("****{tail}"))
            }
            other => other,
        },
        _ => match kv_key(t) {
            Some(key) => match &v {
                Value::String(s) => kv_line(s, key).map_or(Value::Null, Value::String),
                _ => return Err(fail()),
            },
            None => v,
        },
    })
}

/// A matched value through a transform from the closed list.
fn apply(t: &str, v: Value, status_map: &[StatusMapEntry], field: &str) -> Result<Value, OpError> {
    match t {
        "none" => Ok(v),
        "join" => Ok(match v {
            Value::Array(items) => {
                Value::String(items.iter().map(value_text).collect::<Vec<_>>().join(", "))
            }
            other => other,
        }),
        "split_first" => Ok(match v {
            Value::Array(items) => items.into_iter().next().unwrap_or(Value::Null),
            Value::String(s) => match serde_json::from_str::<Value>(&s) {
                Ok(Value::Array(items)) => items.into_iter().next().unwrap_or(Value::Null),
                _ => Value::String(s.split(',').next().unwrap_or_default().trim().to_owned()),
            },
            other => other,
        }),
        "trim" | "lowercase" | "uppercase" | "to_int" | "to_decimal" | "iso_date" | "enum_map"
        | "mask_last4" => match v {
            Value::Array(items) => items
                .into_iter()
                .map(|x| apply_scalar(t, x, status_map, field))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            other => apply_scalar(t, other, status_map, field),
        },
        other if kv_key(other).is_some() => match v {
            Value::Array(items) => items
                .into_iter()
                .map(|x| apply_scalar(t, x, status_map, field))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            other => apply_scalar(t, other, status_map, field),
        },
        other => Err(OpError::new(
            "connector_misconfigured",
            format!("transform_not_allowed {other}"),
        )),
    }
}

/// Maps a raw response. Non-2xx → the fault map (first entry naming the status), else the default fault set.
/// 2xx → the first fault-map `BODY` rule that matches the body (see [`body_fault_matches`]) is the error; else every
/// field-map entry (a tenant override path, keyed by field, beats the template path) through JSONPath
/// and its transform; a path with no match leaves the field out. `enum_map` goes through the op's status map and an
/// unmapped system status is `upstream_unavailable` / `unmapped_status`.
pub fn extract(
    op: &ConnectorTemplateOp,
    overrides: &BTreeMap<String, String>,
    resp: &RawResponse,
) -> Result<Map<String, Value>, OpError> {
    if !(200..300).contains(&resp.status) {
        return Err(fault(op, resp.status));
    }
    if let Some(e) = op
        .fault_map
        .iter()
        .find(|e| body_fault_matches(&e.system, &resp.body))
    {
        return Err(mapped_fault(
            &e.code,
            format!("body_fault {}", e.system.trim()),
        ));
    }
    let mut out = Map::new();
    for e in &op.field_map {
        if out.contains_key(&e.field) {
            continue;
        }
        let path = overrides
            .get(&e.field)
            .map_or(e.path.as_str(), String::as_str);
        let Ok(v) = json_path(&resp.body, path) else {
            continue;
        };
        let t = e.transform.as_deref().unwrap_or("none");
        let mapped = apply(t, v, &op.status_map, &e.field)?;
        // `kv_line` with no such line leaves the field out, like a path with no match.
        if mapped.is_null() && kv_key(t).is_some() {
            continue;
        }
        out.insert(e.field.clone(), mapped);
    }
    Ok(out)
}

fn truncate_chars(s: &mut String, max: usize) -> bool {
    match s.char_indices().nth(max) {
        Some((i, _)) => {
            s.truncate(i);
            true
        }
        None => false,
    }
}

fn cap(v: &mut Value, truncated: &mut bool) {
    match v {
        Value::String(s) => *truncated |= truncate_chars(s, MAX_STRING_CHARS),
        Value::Array(items) => {
            if items.len() > MAX_LIST_ITEMS {
                items.truncate(MAX_LIST_ITEMS);
                *truncated = true;
            }
            for x in items.iter_mut() {
                cap(x, truncated);
            }
        }
        Value::Object(o) => {
            for x in o.values_mut() {
                cap(x, truncated);
            }
        }
        _ => {}
    }
}

/// Keeps only the op's output fields, caps strings at 2 000 chars and lists at 20 items; `truncated: true` is added
/// when anything was cut.
pub fn reduce(out: Map<String, Value>, outputs: &[String]) -> Map<String, Value> {
    let mut kept = Map::new();
    let mut truncated = false;
    for (k, mut v) in out {
        if outputs.contains(&k) {
            cap(&mut v, &mut truncated);
            kept.insert(k, v);
        }
    }
    if truncated {
        kept.insert("truncated".to_owned(), Value::Bool(true));
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::model::{
        ConnectorTemplate, CredentialField, FaultMapEntry, FieldMapEntry, OpDef, OpKind,
        SideEffect, TemplateStatus, Transport,
    };
    use crate::connectors::validate::validate_template;
    use crate::connectors::wire::AuthKind;
    use serde_json::json;

    fn op(path: &str, request: &str) -> ConnectorTemplateOp {
        ConnectorTemplateOp {
            input_types: Default::default(),
            object: "case".into(),
            verb: "read".into(),
            variant: "*".into(),
            side_effect: SideEffect::R,
            op_kind: OpKind::Standard,
            method: "get".into(),
            path: path.into(),
            request: request.into(),
            field_map: vec![],
            status_map: vec![],
            fault_map: vec![],
            pagination: None,
            idempotency: Idempotency::Nat,
            verify_read: None,
            timeout_ms: 5000,
            fixtures: vec![],
        }
    }

    fn fm(path: &str, field: &str, transform: &str) -> FieldMapEntry {
        FieldMapEntry {
            path: path.into(),
            field: field.into(),
            transform: Some(transform.into()),
        }
    }

    fn sm(pairs: &[(&str, &str)]) -> Vec<StatusMapEntry> {
        pairs
            .iter()
            .map(|(s, c)| StatusMapEntry {
                system: (*s).into(),
                canonical: (*c).into(),
            })
            .collect()
    }

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    fn ok(status: u16, body: Value) -> RawResponse {
        RawResponse {
            status,
            body,
            retry_after_secs: None,
        }
    }

    struct Ctx {
        canonical: Map<String, Value>,
        config: Map<String, Value>,
        env: Map<String, Value>,
        secret: Map<String, Value>,
    }

    impl Ctx {
        fn new(canonical: Value) -> Self {
            Self {
                canonical: obj(canonical),
                config: obj(json!({"limit": 50})),
                env: obj(json!({"team": "Billing"})),
                secret: obj(json!({"token": "s3cr3t-value"})),
            }
        }

        fn ctx(&self) -> RenderCtx<'_> {
            RenderCtx {
                canonical: &self.canonical,
                config: &self.config,
                env: &self.env,
                secret: &self.secret,
                idempotency_key: "step-key-1",
            }
        }
    }

    #[test]
    fn secret_never_appears_in_rendered_url() {
        let c = Ctx::new(json!({"case_id": "HD-1"}));
        let leaky = op(
            "/api/tickets/{{canonical.case_id}}?token={{secret.token}}",
            "",
        );
        let err = render(&leaky, &c.ctx()).expect_err("secret in url must be refused");
        assert_eq!(
            (err.code, err.detail.as_str()),
            ("connector_misconfigured", "secret_in_url")
        );
        assert!(!err.to_string().contains("s3cr3t"));
        let in_path = op("/api/{{secret.token}}/tickets", "");
        assert!(render(&in_path, &c.ctx()).is_err());

        let template = ConnectorTemplate {
            key: "t".into(),
            version: "1".into(),
            label: String::new(),
            vendor: String::new(),
            system_class: String::new(),
            description: String::new(),
            wave: "v1".into(),
            platform_connector: false,
            transport: Transport::Rest,
            alt_transport: None,
            auth_kind: AuthKind::BearerStatic,
            auth_detail: String::new(),
            auth_params: Default::default(),
            credential_schema: vec![CredentialField {
                name: "token".into(),
                label: "Token".into(),
                secret: true,
                hint: None,
            }],
            rate_limit_model: Value::Null,
            vendor_api_pin: String::new(),
            webhooks: Value::Null,
            status: TemplateStatus::Active,
            t1: Value::Null,
            t2: Value::Null,
            published_by: None,
            published_at: None,
            drift: Value::Null,
            ops: vec![leaky],
            task_tools: Vec::new(),
        };
        let defs = [OpDef {
            object: "case".into(),
            verb: "read".into(),
            variant: "*".into(),
            side_effect: SideEffect::R,
            identity_level: agent_model::kinds::catalog::IdentityLevel::Verified,
            input_fields: vec!["case_id".into()],
            output_fields: vec!["case_id".into()],
        }];
        let issues = validate_template(&template, &defs);
        assert!(
            issues.iter().any(|i| i.code == "secret_in_url"),
            "{issues:?}"
        );

        // A secret in the body is allowed and still never reaches the URL.
        let body_secret = op(
            "/api/tickets/{{canonical.case_id}}",
            r#"{"auth":"{{secret.token}}"}"#,
        );
        let r = render(&body_secret, &c.ctx()).expect("render");
        assert!(!r.path.contains("s3cr3t"));
        assert!(
            r.query
                .iter()
                .all(|(k, v)| !k.contains("s3cr3t") && !v.contains("s3cr3t"))
        );
    }

    #[test]
    fn whole_value_placeholder_keeps_type() {
        let c = Ctx::new(json!({"qty": 3, "flag": true, "tags": ["a", "b"], "name": "Ann \"A\""}));
        let o = op(
            "/x",
            r#"{"n":"{{canonical.qty}}","s":"x-{{canonical.qty}}","b":{{canonical.flag}},"t":"{{canonical.tags}}","q":"{{canonical.name}}","e":"say \"{{canonical.name}}\"","k":"{{step.idempotency_key}}","l":"{{config.limit}}","g":"{{env.team}}"}"#,
        );
        let r = render(&o, &c.ctx()).expect("render");
        assert_eq!(
            r.body,
            Some(json!({
                "n": 3, "s": "x-3", "b": true, "t": ["a", "b"], "q": "Ann \"A\"",
                "e": "say \"Ann \"A\"\"", "k": "step-key-1", "l": 50, "g": "Billing"
            }))
        );
        assert_eq!(r.method, "GET");
        let missing = op("/x", r#"{"n":"{{canonical.nope}}"}"#);
        assert_eq!(
            render(&missing, &c.ctx()).expect_err("missing").code,
            "invalid_input"
        );
        let gql = op("/graphql.json", "query($id:ID!){ order(id:$id){ id } }");
        assert_eq!(
            render(&gql, &c.ctx()).expect("render").body,
            Some(json!("query($id:ID!){ order(id:$id){ id } }"))
        );
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn optional_query_pair_is_dropped_when_absent_or_null() {
        let path = "/s?a=1&from={{canonical.from?}}&b=2";
        let present = Ctx::new(json!({"from": "2026-01-02"}));
        let r = render(&op(path, ""), &present.ctx()).expect("render");
        assert_eq!(
            r.query,
            q(&[("a", "1"), ("from", "2026-01-02"), ("b", "2")])
        );
        for canonical in [json!({}), json!({"from": null})] {
            let c = Ctx::new(canonical);
            let r = render(&op(path, ""), &c.ctx()).expect("render");
            assert_eq!(r.path, "/s");
            assert_eq!(r.query, q(&[("a", "1"), ("b", "2")]));
        }
        let c = Ctx::new(json!({}));
        let first = render(&op("/s?f={{canonical.f?}}&b=2", ""), &c.ctx()).expect("first");
        assert_eq!(first.query, q(&[("b", "2")]));
        let last = render(&op("/s?b=2&f={{canonical.f?}}", ""), &c.ctx()).expect("last");
        assert_eq!(last.query, q(&[("b", "2")]));
        let all = render(
            &op("/s?f={{canonical.f?}}&g={{canonical.g?}}", ""),
            &c.ctx(),
        )
        .expect("all");
        assert_eq!(all.path, "/s");
        assert!(all.query.is_empty());
        // a required placeholder in the same query still fails
        let req = render(&op("/s?f={{canonical.f?}}&g={{canonical.g}}", ""), &c.ctx());
        assert_eq!(req.expect_err("required").code, "invalid_input");
    }

    #[test]
    fn optional_body_member_is_dropped_and_other_misses_still_fail() {
        let o = op(
            "/x",
            r#"{"keep":"{{canonical.k}}","needed_by":"{{canonical.needed_by?}}","n":"{{canonical.n?}}"}"#,
        );
        let c = Ctx::new(json!({"k": "a", "n": 3}));
        assert_eq!(
            render(&o, &c.ctx()).expect("render").body,
            Some(json!({"keep": "a", "n": 3}))
        );
        let full = Ctx::new(json!({"k": "a", "n": 3, "needed_by": "2026-02-01"}));
        assert_eq!(
            render(&o, &full.ctx()).expect("render").body,
            Some(json!({"keep": "a", "needed_by": "2026-02-01", "n": 3}))
        );
        let nulled = Ctx::new(json!({"k": "a", "needed_by": null, "n": null}));
        assert_eq!(
            render(&o, &nulled.ctx()).expect("render").body,
            Some(json!({"keep": "a"}))
        );
        // embedded in a longer string, in an array, or in a path segment: a missing value is an error
        for (path, request) in [
            ("/x", r#"{"s":"by {{canonical.nope?}}"}"#),
            ("/x", r#"{"s":["{{canonical.nope?}}"]}"#),
            ("/x/{{canonical.nope?}}", ""),
        ] {
            let e = render(&op(path, request), &c.ctx()).expect_err("missing");
            assert_eq!(e.code, "invalid_input", "{path} {request}");
        }
    }

    #[test]
    fn optional_with_fallback_drops_only_when_both_are_absent() {
        let o = op(
            "/x?by={{canonical.who ?? env.team?}}&z={{canonical.who ?? env.nope?}}",
            r#"{"by":"{{canonical.who ?? env.nope?}}"}"#,
        );
        let c = Ctx::new(json!({}));
        let r = render(&o, &c.ctx()).expect("render");
        assert_eq!(r.query, q(&[("by", "Billing")]));
        assert_eq!(r.body, Some(json!({})));
        let given = Ctx::new(json!({"who": "ann"}));
        let r = render(&o, &given.ctx()).expect("render");
        assert_eq!(r.query, q(&[("by", "ann"), ("z", "ann")]));
        assert_eq!(r.body, Some(json!({"by": "ann"})));
    }

    #[test]
    fn fallback_placeholder_uses_the_fallback_when_absent_or_null() {
        let o = op(
            "/x",
            r#"{"raised_by":"{{canonical.customer_email ?? env.team}}","s":"by {{canonical.customer_email??env.team}}"}"#,
        );
        let given = Ctx::new(json!({"customer_email": "ann@example.com"}));
        assert_eq!(
            render(&o, &given.ctx()).expect("render").body,
            Some(json!({"raised_by": "ann@example.com", "s": "by ann@example.com"}))
        );
        for absent in [json!({}), json!({"customer_email": null})] {
            let c = Ctx::new(absent);
            assert_eq!(
                render(&o, &c.ctx()).expect("render").body,
                Some(json!({"raised_by": "Billing", "s": "by Billing"}))
            );
        }
        // both missing: the primary's invalid_input
        let both = op("/x", r#"{"a":"{{canonical.customer_email ?? env.nope}}"}"#);
        let e = render(&both, &Ctx::new(json!({})).ctx()).expect_err("missing");
        assert_eq!(e.code, "invalid_input");
        // a secret fallback in the URL is still refused
        let url = op("/x/{{canonical.case_id ?? secret.token}}", "");
        assert_eq!(
            render(&url, &Ctx::new(json!({"case_id": "1"})).ctx())
                .expect_err("secret")
                .code,
            "connector_misconfigured"
        );
    }

    #[test]
    fn query_split() {
        let c = Ctx::new(json!({"case_id": "a/b c"}));
        let o = op(
            "/tickets/{{canonical.case_id}}?a=b&c={{config.limit}}&&flag",
            "",
        );
        let r = render(&o, &c.ctx()).expect("render");
        assert_eq!(r.path, "/tickets/a%2Fb%20c");
        assert_eq!(
            r.query,
            vec![
                ("a".to_owned(), "b".to_owned()),
                ("c".to_owned(), "50".to_owned()),
                ("flag".to_owned(), String::new()),
            ]
        );
        assert_eq!(r.body, None);
        let mut w = op("/t", "");
        w.idempotency = Idempotency::Header {
            header: "Idempotency-Key".into(),
        };
        let r = render(&w, &c.ctx()).expect("render");
        assert_eq!(
            r.headers,
            vec![("Idempotency-Key".to_owned(), "step-key-1".to_owned())]
        );
    }

    #[test]
    fn star_path_maps_list() {
        let mut o = op("/graphql.json", "");
        o.field_map = vec![
            fm("$.data.orders.edges[*].node.id", "id", "none"),
            fm("$.data.orders.edges[*].node.state", "status", "enum_map"),
            fm("$.data.orders.edges[*].node.total", "price", "to_decimal"),
        ];
        o.status_map = sm(&[("OPEN", "processing"), ("DONE", "delivered")]);
        let body = json!({"data": {"orders": {"edges": [
            {"node": {"id": "1", "state": "OPEN", "total": "10.50"}},
            {"node": {"id": "2", "state": "DONE", "total": 7}}
        ]}}});
        let out = extract(&o, &BTreeMap::new(), &ok(200, body)).expect("extract");
        assert_eq!(out.get("id"), Some(&json!(["1", "2"])));
        assert_eq!(out.get("status"), Some(&json!(["processing", "delivered"])));
        assert_eq!(out.get("price"), Some(&json!(["10.50", "7"])));
    }

    #[test]
    fn enum_map_unmapped_value_is_error() {
        let mut o = op("/t", "");
        o.field_map = vec![fm("$.status", "status", "enum_map")];
        o.status_map = sm(&[("Open", "open")]);
        let err = extract(&o, &BTreeMap::new(), &ok(200, json!({"status": "Weird"})))
            .expect_err("unmapped");
        assert_eq!(
            (err.code, err.detail.as_str()),
            ("upstream_unavailable", "unmapped_status")
        );
        let mapped =
            extract(&o, &BTreeMap::new(), &ok(200, json!({"status": "Open"}))).expect("mapped");
        assert_eq!(mapped.get("status"), Some(&json!("open")));
    }

    #[test]
    fn fault_map_first_match_wins() {
        let mut o = op("/t", "");
        o.fault_map = [
            ("THROTTLED (extensions.cost)", "rate_limited"),
            ("HTTP 404 RecordNotFound", "invalid_state"),
            ("HTTP 404", "not_found"),
            ("HTTP 403 ACL", "not_found"),
            ("HTTP 5xx", "timeout"),
            ("HTTP 409 idempotency_error", "duplicate"),
        ]
        .iter()
        .map(|(s, c)| FaultMapEntry {
            system: (*s).into(),
            code: (*c).into(),
        })
        .collect();
        let code = |status| {
            extract(&o, &BTreeMap::new(), &ok(status, Value::Null))
                .expect_err("fault")
                .code
        };
        assert_eq!(code(404), "invalid_state");
        assert_eq!(code(403), "not_found");
        assert_eq!(code(503), "timeout");
        assert_eq!(code(409), "duplicate");
        assert_eq!(code(401), "connector_auth_failed");
        assert_eq!(code(0), "upstream_unavailable");
    }

    #[test]
    fn default_fault_set() {
        let o = op("/t", "");
        for (status, want) in [
            (401, "connector_auth_failed"),
            (403, "connector_misconfigured"),
            (404, "not_found"),
            (422, "invalid_input"),
            (400, "invalid_input"),
            (429, "rate_limited"),
            (500, "upstream_unavailable"),
            (503, "upstream_unavailable"),
            (0, "upstream_unavailable"),
        ] {
            let err = extract(&o, &BTreeMap::new(), &ok(status, json!({"error": "x"})))
                .expect_err("fault");
            assert_eq!(err.code, want, "status {status}");
        }
    }

    #[test]
    fn override_beats_template_default() {
        let mut o = op("/t", "");
        o.field_map = vec![
            fm("$.ticket.id", "case_id", "none"),
            fm("$.ticket.ref", "reference", "uppercase"),
        ];
        let body = json!({"ticket": {"id": 7, "ref": "abc", "custom": {"no": "x-9"}}});
        let overrides = BTreeMap::from([("reference".to_owned(), "$.ticket.custom.no".to_owned())]);
        let out = extract(&o, &overrides, &ok(200, body.clone())).expect("extract");
        assert_eq!(out.get("case_id"), Some(&json!(7)));
        assert_eq!(out.get("reference"), Some(&json!("X-9")));
        let plain = extract(&o, &BTreeMap::new(), &ok(200, body)).expect("extract");
        assert_eq!(plain.get("reference"), Some(&json!("ABC")));
    }

    #[test]
    fn reduce_drops_unknown_and_caps() {
        let long = "é".repeat(MAX_STRING_CHARS + 5);
        let items: Vec<i32> = (0..25).collect();
        let out = obj(json!({
            "id": "1",
            "secret_field": "drop me",
            "notes": long,
            "items": items
        }));
        let outputs = ["id".to_owned(), "notes".to_owned(), "items".to_owned()];
        let r = reduce(out, &outputs);
        assert!(!r.contains_key("secret_field"));
        assert_eq!(r.get("id"), Some(&json!("1")));
        assert_eq!(
            r.get("notes")
                .and_then(Value::as_str)
                .map(|s| s.chars().count()),
            Some(MAX_STRING_CHARS)
        );
        assert_eq!(
            r.get("items").and_then(Value::as_array).map(Vec::len),
            Some(MAX_LIST_ITEMS)
        );
        assert_eq!(r.get("truncated"), Some(&json!(true)));
        let small = reduce(obj(json!({"id": "1"})), &outputs);
        assert!(!small.contains_key("truncated"));
    }

    fn faults(pairs: &[(&str, &str)]) -> Vec<FaultMapEntry> {
        pairs
            .iter()
            .map(|(s, c)| FaultMapEntry {
                system: (*s).into(),
                code: (*c).into(),
            })
            .collect()
    }

    #[test]
    fn kv_line_reads_key_value_lines() {
        let text =
            "USERNAME:jdoe\r\n  EMAIL: j.doe@example.com \r\nSTATUS:SUCCESS\r\nNOTE:a:b\nEMPTY:\n";
        assert_eq!(kv_line(text, "USERNAME").as_deref(), Some("jdoe"));
        assert_eq!(kv_line(text, "EMAIL").as_deref(), Some("j.doe@example.com"));
        assert_eq!(kv_line(text, "STATUS").as_deref(), Some("SUCCESS"));
        assert_eq!(kv_line(text, "NOTE").as_deref(), Some("a:b"));
        assert_eq!(kv_line(text, "EMPTY").as_deref(), Some(""));
        assert_eq!(
            kv_line(text, "USER"),
            None,
            "a key is matched whole, up to its colon"
        );
        assert_eq!(kv_line(text, "MISSING"), None);
        assert_eq!(
            kv_line("A:1\nA:2", "A").as_deref(),
            Some("1"),
            "first line wins"
        );
    }

    #[test]
    fn kv_line_transform_maps_and_skips_missing_keys() {
        let mut o = op("/v1/ad/execute", "");
        o.field_map = vec![
            fm("$.stdout", "email", "kv_line:EMAIL"),
            fm("$.stdout", "name", "kv_line:DISPLAY_NAME"),
            fm("$.lines[*]", "status", "kv_line:STATUS"),
        ];
        let body = json!({"stdout": "EMAIL:a@example.com\nSTATUS:SUCCESS", "lines": ["STATUS:A", "STATUS:B"]});
        let out = extract(&o, &BTreeMap::new(), &ok(200, body)).expect("extract");
        assert_eq!(out.get("email"), Some(&json!("a@example.com")));
        assert!(
            !out.contains_key("name"),
            "no DISPLAY_NAME line leaves the field out"
        );
        assert_eq!(out.get("status"), Some(&json!(["A", "B"])));
        o.field_map = vec![fm("$.code", "email", "kv_line:EMAIL")];
        let err = extract(&o, &BTreeMap::new(), &ok(200, json!({"code": 3}))).expect_err("number");
        assert_eq!(err.code, "upstream_unavailable");
        assert!(err.detail.starts_with("transform_failed"), "{}", err.detail);
        o.field_map = vec![fm("$.stdout", "email", "kv_line:email")];
        let err = extract(&o, &BTreeMap::new(), &ok(200, json!({"stdout": "email:x"})))
            .expect_err("lowercase key is not a transform");
        assert_eq!(err.code, "connector_misconfigured");
    }

    #[test]
    fn body_fault_rule_classifies_a_2xx_failure() {
        let mut o = op("/v1/ad/execute", "");
        o.fault_map = faults(&[
            ("HTTP 401", "connector_auth_failed"),
            ("BODY $.stdout kv_line:EXISTS = false", "not_found"),
            (
                "BODY $.stdout kv_line:STATUS = FAILED",
                "upstream_unavailable",
            ),
            ("BODY $.ok = false", "invalid_state"),
            ("BODY $.stdout kv_line:STATUS = WEIRD", "no_such_code"),
        ]);
        o.field_map = vec![fm("$.stdout", "status", "kv_line:STATUS")];
        let failed = json!({"stdout": "STATUS:FAILED\r\nERROR:Cannot find an object with identity: 'x'\r\n"});
        let err = extract(&o, &BTreeMap::new(), &ok(200, failed)).expect_err("2xx failure");
        assert_eq!(err.code, "upstream_unavailable");
        assert_eq!(
            err.detail,
            "body_fault BODY $.stdout kv_line:STATUS = FAILED"
        );
        let absent = json!({"stdout": "EXISTS:false\nSTATUS:SUCCESS"});
        assert_eq!(
            extract(&o, &BTreeMap::new(), &ok(200, absent))
                .expect_err("absent")
                .code,
            "not_found"
        );
        assert_eq!(
            extract(&o, &BTreeMap::new(), &ok(200, json!({"ok": false})))
                .expect_err("bool")
                .code,
            "invalid_state"
        );
        let weird = extract(
            &o,
            &BTreeMap::new(),
            &ok(200, json!({"stdout": "STATUS:WEIRD"})),
        )
        .expect_err("non-canonical");
        assert_eq!(weird.code, "upstream_unavailable");
        assert!(weird.detail.ends_with("(non-canonical fault code)"));
        let fine = extract(
            &o,
            &BTreeMap::new(),
            &ok(200, json!({"stdout": "EXISTS:true\nSTATUS:SUCCESS"})),
        )
        .expect("success");
        assert_eq!(fine.get("status"), Some(&json!("SUCCESS")));
        // BODY rules never apply to a non-2xx answer, and HTTP rules never to a 2xx one.
        assert_eq!(
            extract(
                &o,
                &BTreeMap::new(),
                &ok(401, json!({"stdout": "STATUS:FAILED"}))
            )
            .expect_err("401")
            .code,
            "connector_auth_failed"
        );
        for (system, hit) in [
            ("BODY $.stdout kv_line:STATUS = FAILED", true),
            ("  BODY   $.stdout   kv_line:STATUS   =   FAILED  ", true),
            ("BODY $.stdout kv_line:STATUS = SUCCESS", false),
            ("BODY $.n = 3", true),
            ("BODY $.n none = 3", true),
            ("BODY $.missing = 3", false),
            ("BODY $.stdout kv_line:status = FAILED", false),
            ("BODY $.stdout trim = FAILED", false),
            ("BODY $.stdout kv_line:STATUS", false),
            ("BODY= 3", false),
            ("BODYX $.n = 3", false),
            ("HTTP 500", false),
            ("STATUS:FAILED", false),
            ("BODY $.obj = {}", false),
        ] {
            let body = json!({"stdout": "STATUS:FAILED", "n": 3, "obj": {}});
            assert_eq!(body_fault_matches(system, &body), hit, "{system}");
        }
    }

    /// A script gateway can answer a generated password as a `PASSWORD:` line of `stdout`: a `kv_line` map must never
    /// carry it into a mapped (and so never a reduced) outcome.
    #[test]
    fn kv_line_password_never_reaches_the_outcome() {
        let mut reset = op("/v1/execute", "");
        reset.field_map = vec![fm("$.stdout", "status", "kv_line:STATUS")];
        let password = "Xy7-not-real";
        let body = json!({"stdout": format!("USERNAME:jdoe\r\nPASSWORD:{password}\r\nSTATUS:SUCCESS\r\n")});
        let out = extract(&reset, &BTreeMap::new(), &ok(200, body)).expect("maps");
        let reduced = reduce(
            out.clone(),
            &["status".to_owned(), "temporary_password_sent".to_owned()],
        );
        for v in [Value::Object(out), Value::Object(reduced.clone())] {
            let text = v.to_string();
            assert!(!text.contains(password), "password leaked: {text}");
            assert!(!text.to_ascii_uppercase().contains("PASSWORD:"), "{text}");
        }
        assert_eq!(reduced.get("status"), Some(&json!("SUCCESS")));
    }

    #[test]
    fn frappe_ticket_maps() {
        let mut o = op("/api/resource/HD Ticket/{{canonical.case_id}}", "");
        o.field_map = vec![
            fm("$.data.name", "case_id", "none"),
            fm("$.data.status", "status", "enum_map"),
            fm("$.data.modified", "updated_at", "iso_date"),
            fm("$.data._assign", "assignee", "split_first"),
        ];
        o.status_map = sm(&[
            ("Open", "open"),
            ("Replied", "pending_customer"),
            ("Paused", "pending_provider"),
            ("Resolved", "resolved"),
            ("Closed", "closed"),
        ]);
        let body = json!({"data": {"name": "HD-0042", "status": "Replied", "modified": "2026-10-20 10:00:00"}});
        let out = extract(&o, &BTreeMap::new(), &ok(200, body)).expect("extract");
        assert_eq!(
            Value::Object(out),
            json!({"case_id": "HD-0042", "status": "pending_customer", "updated_at": "2026-10-20T10:00:00Z"})
        );
        let assigned = json!({"data": {"name": "HD-1", "status": "Open", "_assign": "[\"agent@example.com\"]"}});
        let out = extract(&o, &BTreeMap::new(), &ok(200, assigned)).expect("extract");
        assert_eq!(out.get("assignee"), Some(&json!("agent@example.com")));
        let c = Ctx::new(json!({"case_id": "HD-0042"}));
        assert_eq!(
            render(&o, &c.ctx()).expect("render").path,
            "/api/resource/HD Ticket/HD-0042"
        );
    }
}
