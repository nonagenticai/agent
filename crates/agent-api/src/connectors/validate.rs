//! B7 T4: template validation. [`validate_tenant_template`] is a port of TS `validateTenantTemplate` of
//! `web/src/lib/connectors/ops.ts` (same codes, paths and messages; golden-tested). [`validate_template`] runs the
//! same rules on a platform template and adds the server-only checks: `secret_in_url`, `host_not_from_instance`,
//! `payment_requires_hosted_link` (D27). `idempotency_required` (W/X/P with `none`) is part of the shared rules.
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::jsonpath::js_space;
use super::model::{
    CSRF_FETCH_PATH, CSRF_HEADER, ConnectorTemplate, CredentialField, FieldMapEntry, Idempotency,
    MTLS_FIELDS, OpDef, OpKind, OpRef, SideEffect, TLS_VERIFY_OFF, TenantConnectorTemplate,
};
use super::opkey::{op_key, same_op};
use super::wire::AuthKind;

/// The closed transform list a mapping may use (TS `TRANSFORMS`). Anything else is refused at validation.
pub const TRANSFORMS: [&str; 11] = [
    "none",
    "trim",
    "lowercase",
    "uppercase",
    "to_int",
    "to_decimal",
    "iso_date",
    "enum_map",
    "split_first",
    "join",
    "mask_last4",
];

/// The one parameterised transform (TS `KV_LINE`): `kv_line:<KEY>` reads the value of the first `KEY:value` line of
/// a string, `KEY` matching `^[A-Z0-9_]+$` (e.g. a script gateway's `stdout`). Returns `KEY`.
pub fn kv_key(transform: &str) -> Option<&str> {
    let key = transform.strip_prefix("kv_line:")?;
    (!key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
    .then_some(key)
}

/// TS `transformAllowed`: in the closed list, or `kv_line:<KEY>`.
pub fn transform_allowed(transform: &str) -> bool {
    TRANSFORMS.contains(&transform) || kv_key(transform).is_some()
}

/// One validation finding: TS `ValidationError` (`path` like `ops[0] order.read.field_map[2].transform`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub path: String,
    pub code: &'static str,
    pub message: String,
}

impl Issue {
    fn new(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
        }
    }
}

/// A `{{ns.name}}` placeholder found in a template string, with its byte range. `{{ns.name ?? ns2.name2}}` (R10)
/// carries a fallback: when `ns.name` has no value (absent or null) the fallback is used instead — how an optional
/// canonical input (`canonical.customer_email`) falls back to a configured value (`env.requester_id`).
///
/// A trailing `?` before the closing braces (`{{canonical.from?}}`, `{{a ?? b?}}`) marks the placeholder OPTIONAL: when
/// it has no value (primary and fallback both absent or null) the renderer drops the thing it fills instead of
/// failing the call. Only two places can drop: a query pair whose value is exactly the placeholder, and a JSON object
/// member whose value is exactly the whole string. Anywhere else (a path segment, a query key, text embedded in a
/// longer string, an array element, a non-JSON request) the validator refuses it (`optional_not_droppable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder<'a> {
    pub start: usize,
    pub end: usize,
    pub ns: &'a str,
    pub name: &'a str,
    /// `(ns, name)` after `??`, if any.
    pub fallback: Option<(&'a str, &'a str)>,
    /// Trailing `?`: see the type docs.
    pub optional: bool,
}

impl<'a> Placeholder<'a> {
    /// Every `(ns, name)` the placeholder names: the primary, then the fallback.
    pub fn refs(&self) -> impl Iterator<Item = (&'a str, &'a str)> + '_ {
        std::iter::once((self.ns, self.name)).chain(self.fallback)
    }
}

/// `ns.name` at the start of `t` (TS `([a-z_]+)\.([a-z_][a-z0-9_]*)`) and the rest after it.
fn ns_name(t: &str) -> Option<(&str, &str, &str)> {
    let ns_len = t
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
        .count();
    if ns_len == 0 {
        return None;
    }
    let ns = t.get(..ns_len)?;
    let after_ns = t.get(ns_len..)?.strip_prefix('.')?;
    let first = *after_ns.as_bytes().first()?;
    if !(first.is_ascii_lowercase() || first == b'_') {
        return None;
    }
    let name_len = 1 + after_ns
        .bytes()
        .skip(1)
        .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        .count();
    Some((ns, after_ns.get(..name_len)?, after_ns.get(name_len..)?))
}

/// The placeholder at byte `i` (TS `PLACEHOLDER`,
/// `/\{\{\s*([a-z_]+)\.([a-z_][a-z0-9_]*)(?:\s*\?\?\s*([a-z_]+)\.([a-z_][a-z0-9_]*))?\s*(\?)?\s*\}\}/`), if any.
fn placeholder_at(s: &str, i: usize) -> Option<Placeholder<'_>> {
    let rest = s.get(i..)?.strip_prefix("{{")?;
    let (ns, name, tail) = ns_name(rest.trim_start_matches(js_space))?;
    let tail = tail.trim_start_matches(js_space);
    let (fallback, tail) = match tail
        .strip_prefix("??")
        .and_then(|t| ns_name(t.trim_start_matches(js_space)))
    {
        Some((fns, fname, t)) => (Some((fns, fname)), t.trim_start_matches(js_space)),
        None => (None, tail),
    };
    let (optional, tail) = match tail.strip_prefix('?') {
        Some(t) => (true, t.trim_start_matches(js_space)),
        None => (false, tail),
    };
    let after = tail.strip_prefix("}}")?;
    Some(Placeholder {
        start: i,
        end: s.len() - after.len(),
        ns,
        name,
        fallback,
        optional,
    })
}

/// Every placeholder of `s`, left to right, non-overlapping (the TS global regex scan).
pub fn placeholders(s: &str) -> Vec<Placeholder<'_>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = s.get(from..).and_then(|r| r.find("{{")) {
        let i = from + off;
        match placeholder_at(s, i) {
            Some(p) => {
                from = p.end;
                out.push(p);
            }
            None => from = i + 1,
        }
    }
    out
}

/// Number of leading chars of `t` in the class.
fn run(t: &str, class: impl Fn(char) -> bool) -> usize {
    t.chars().take_while(|c| class(*c)).count()
}

/// `\s+` then a run of at least `min` class chars.
fn spaced_run(t: &str, min: usize, class: impl Fn(char) -> bool) -> bool {
    let rest = t.trim_start_matches(js_space);
    rest.len() < t.len() && run(rest, class) >= min
}

/// TS `SECRETISH`: literal values that look like credentials.
fn secretish(s: &str) -> bool {
    let alnum = |c: char| c.is_ascii_alphanumeric();
    s.char_indices().any(|(i, _)| {
        let t = &s[i..];
        let after = |p: &str| t.strip_prefix(p);
        after("sk_live_")
            .or_else(|| after("sk_test_"))
            .is_some_and(|r| run(r, alnum) >= 1)
            || after("rk_live_").is_some_and(|r| run(r, alnum) >= 1)
            || after("shpat_").is_some_and(|r| run(r, alnum) >= 1)
            || after("Bearer").is_some_and(|r| {
                spaced_run(r, 12, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
                })
            })
            || after("Basic").is_some_and(|r| {
                spaced_run(r, 12, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')
                })
            })
            || after("apikey:").is_some_and(|r| run(r, alnum) >= 8)
    })
}

/// The op fields the shared rules read, from a platform or a tenant op.
struct OpView<'a> {
    op: OpRef,
    path: &'a str,
    request: &'a str,
    field_map: &'a [FieldMapEntry],
    idempotency: &'a Idempotency,
}

/// Platform op extras for the server-only checks.
struct PlatformExtras {
    side_effect: SideEffect,
    op_kind: OpKind,
}

fn find_def<'d>(defs: &'d [OpDef], op: &OpRef) -> Option<&'d OpDef> {
    let star = OpRef {
        variant: "*".to_owned(),
        ..op.clone()
    };
    defs.iter()
        .find(|d| same_op(&d.op_ref(), op))
        .or_else(|| defs.iter().find(|d| same_op(&d.op_ref(), &star)))
}

/// TS `validateTenantTemplate` over a neutral op view; `platform` carries per-op extras for the server checks
/// (`None` = tenant mode, which mirrors TS exactly, including a missing transform reading as `undefined`).
fn validate_ops(
    auth: Auth<'_>,
    schema: &[CredentialField],
    ops: &[OpView<'_>],
    defs: &[OpDef],
    platform: Option<&[PlatformExtras]>,
) -> Vec<Issue> {
    let mut errors = Vec::new();
    let secrets: BTreeSet<&str> = schema
        .iter()
        .filter(|f| f.secret)
        .map(|f| f.name.as_str())
        .collect();
    let config: BTreeSet<&str> = schema
        .iter()
        .filter(|f| !f.secret)
        .map(|f| f.name.as_str())
        .collect();
    if ops.is_empty() {
        errors.push(Issue::new(
            "ops",
            "no_operations",
            "Add at least one operation.",
        ));
    }
    for (i, op) in ops.iter().enumerate() {
        let key = op_key(&op.op);
        let at = format!("ops[{i}] {key}");
        validate_op(
            op,
            &at,
            &key,
            defs,
            &secrets,
            &config,
            platform.is_some(),
            &mut errors,
        );
        if let Some(extras) = platform.and_then(|p| p.get(i)) {
            server_checks(op, extras, &at, &mut errors);
        }
    }
    validate_auth(auth, schema, &mut errors);
    errors
}

/// A template's `auth_kind` + `auth_params`, as the auth rules read them.
#[derive(Clone, Copy)]
struct Auth<'a> {
    kind: AuthKind,
    params: &'a BTreeMap<String, String>,
}

/// RFC 9110 `token`: the characters of an HTTP header name.
fn header_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// A CSRF prefetch path: relative to the instance base URL, one leading `/`, no whitespace, `?`, `#` or `\`.
fn relative_path(p: &str) -> bool {
    p.starts_with('/')
        && !p.starts_with("//")
        && !p
            .chars()
            .any(|c| js_space(c) || matches!(c, '?' | '#' | '\\'))
}

/// TS `validateAuth`: an `mtls` template declares its two secret credential fields; certificate verification can
/// never be turned off; a CSRF prefetch (`auth_params.csrf_fetch_path` / `csrf_header`) is a relative path and a
/// header name.
fn validate_auth(auth: Auth<'_>, schema: &[CredentialField], errors: &mut Vec<Issue>) {
    if auth.kind == AuthKind::Mtls {
        for name in MTLS_FIELDS {
            if !schema.iter().any(|f| f.secret && f.name == name) {
                errors.push(Issue::new(
                    "credential_schema",
                    "mtls_credential_missing",
                    format!("mtls needs the secret credential field {name}."),
                ));
            }
        }
    }
    for key in TLS_VERIFY_OFF {
        if auth.params.contains_key(key) {
            errors.push(Issue::new(
                format!("auth_params.{key}"),
                "tls_verification_required",
                "Certificate verification cannot be turned off. Trust a private CA with the ca_pem credential field instead.",
            ));
        }
    }
    if let Some(p) = auth.params.get(CSRF_FETCH_PATH) {
        let p = p.trim_matches(js_space);
        if !p.is_empty() && !relative_path(p) {
            errors.push(Issue::new(
                format!("auth_params.{CSRF_FETCH_PATH}"),
                "invalid_csrf",
                "The CSRF fetch path is relative to the instance base URL and starts with a single / (no query, fragment or spaces).",
            ));
        }
    }
    if let Some(h) = auth.params.get(CSRF_HEADER) {
        let h = h.trim_matches(js_space);
        if !h.is_empty() && !header_name(h) {
            errors.push(Issue::new(
                format!("auth_params.{CSRF_HEADER}"),
                "invalid_csrf",
                "The CSRF header must be an HTTP header name such as x-csrf-token.",
            ));
        }
    }
}

/// An optional placeholder in a path is droppable only as the whole value of a query pair (`?k={{x?}}`, `&k={{x?}}`).
fn optional_droppable_in_path(path: &str, p: &Placeholder<'_>) -> bool {
    let b = path.as_bytes();
    super::mapping::query_start(path).is_some_and(|q| q < p.start)
        && p.start > 0
        && b.get(p.start - 1) == Some(&b'=')
        && b.get(p.end).is_none_or(|c| *c == b'&')
}

/// An optional placeholder in a request is droppable only as a whole-string JSON object member value
/// (`"k": "{{x?}}"` followed by `,` or `}`); in an array, a longer string, or a non-JSON request it is not.
fn optional_droppable_in_request(request: &str, p: &Placeholder<'_>) -> bool {
    let before = request.get(..p.start).unwrap_or_default();
    let after = request.get(p.end..).unwrap_or_default();
    let (Some(head), Some(tail)) = (before.strip_suffix('"'), after.strip_prefix('"')) else {
        return false;
    };
    head.trim_end_matches(js_space).ends_with(':')
        && tail.trim_start_matches(js_space).starts_with([',', '}'])
}

#[allow(clippy::too_many_arguments)]
fn validate_op(
    op: &OpView<'_>,
    at: &str,
    key: &str,
    defs: &[OpDef],
    secrets: &BTreeSet<&str>,
    config: &BTreeSet<&str>,
    platform: bool,
    errors: &mut Vec<Issue>,
) {
    let Some(def) = find_def(defs, &op.op) else {
        errors.push(Issue::new(
            at,
            "unknown_operation",
            format!("{key} is not in the operation catalogue."),
        ));
        return;
    };
    let def_key = op_key(&def.op_ref());
    for p in placeholders(op.request) {
        for (i, (ns, name)) in p.refs().enumerate() {
            // A canonical name on the LEFT of `??` need not be a declared input: it is an optional future input
            // (as `canonical.customer_email` was before platform seed v2 declared it on `case.create`) that resolves
            // to absent today, so
            // the fallback — which is still checked below — is what runs. Only the primary gets this leniency, and
            // only when a fallback follows; a bare `{{canonical.x}}` must still name a declared input.
            let optional_primary = i == 0 && p.fallback.is_some() && ns == "canonical";
            let ok = (ns == "canonical"
                && (optional_primary || def.input_fields.iter().any(|f| f == name)))
                || (ns == "secret" && secrets.contains(name))
                || (ns == "config" && config.contains(name))
                || ns == "env"
                || (ns == "step" && name == "idempotency_key");
            if !ok {
                errors.push(Issue::new(
                    format!("{at}.request"),
                    "unknown_placeholder",
                    format!(
                        "{{{{{ns}.{name}}}}} is not an input of {def_key}, a declared secret, a config field or an env ID."
                    ),
                ));
            }
        }
    }
    for (where_, text, droppable) in [
        (
            "path",
            op.path,
            optional_droppable_in_path as fn(&str, &Placeholder<'_>) -> bool,
        ),
        ("request", op.request, optional_droppable_in_request),
    ] {
        for p in placeholders(text).iter().filter(|p| p.optional) {
            if !droppable(text, p) {
                errors.push(Issue::new(
                    format!("{at}.{where_}"),
                    "optional_not_droppable",
                    format!(
                        "{} is optional but sits where it cannot be dropped: an optional placeholder must be a whole query value (?k={{{{canonical.x?}}}}) or a whole JSON object member value (\"k\": \"{{{{canonical.x?}}}}\").",
                        &text[p.start..p.end]
                    ),
                ));
            }
        }
    }
    if secretish(op.request) || secretish(op.path) {
        errors.push(Issue::new(
            format!("{at}.request"),
            "literal_secret",
            "A credential is written into the template. Declare it as a secret field and use {{secret.<name>}}.",
        ));
    }
    for (j, f) in op.field_map.iter().enumerate() {
        if !def.output_fields.contains(&f.field) {
            errors.push(Issue::new(
                format!("{at}.field_map[{j}]"),
                "unknown_field",
                format!(
                    "{} is not an output field of {def_key} ({}).",
                    f.field,
                    def.output_fields.join(", ")
                ),
            ));
        }
        let transform = match (&f.transform, platform) {
            (Some(t), _) => t.as_str(),
            (None, true) => "none",
            (None, false) => "undefined",
        };
        if !transform_allowed(transform) {
            errors.push(Issue::new(
                format!("{at}.field_map[{j}].transform"),
                "transform_not_allowed",
                format!(
                    "“{transform}” is not in the closed transform list ({}, kv_line:<KEY>).",
                    TRANSFORMS.join(", ")
                ),
            ));
        }
        if !f.path.starts_with('$') {
            errors.push(Issue::new(
                format!("{at}.field_map[{j}]"),
                "invalid_path",
                "A response path starts with $.",
            ));
        }
    }
    if def.side_effect.needs_key() && *op.idempotency == Idempotency::None {
        errors.push(Issue::new(
            format!("{at}.idempotency"),
            "idempotency_required",
            format!(
                "{def_key} is {}: declare an Idempotency header or a dup_search; none is refused.",
                def.side_effect.as_str()
            ),
        ));
    }
}

/// True when `path` leaves the instance base URL: a scheme (`https://…`, `http:…`) or a protocol-relative `//`.
fn leaves_instance(path: &str) -> bool {
    let p = path.trim_matches(js_space);
    let lower = p.to_ascii_lowercase();
    p.starts_with("//")
        || p.starts_with("\\\\")
        || p.contains("://")
        || lower.starts_with("http:")
        || lower.starts_with("https:")
}

fn server_checks(op: &OpView<'_>, extras: &PlatformExtras, at: &str, errors: &mut Vec<Issue>) {
    if placeholders(op.path)
        .iter()
        .any(|p| p.refs().any(|(ns, _)| ns == "secret"))
    {
        errors.push(Issue::new(
            format!("{at}.path"),
            "secret_in_url",
            "A secret may not appear in the URL path or query; send it in a header or the request body.",
        ));
    }
    if leaves_instance(op.path) {
        errors.push(Issue::new(
            format!("{at}.path"),
            "host_not_from_instance",
            "The path must be relative to the instance base URL (no scheme, host or leading //).",
        ));
    }
    if extras.side_effect == SideEffect::P && extras.op_kind != OpKind::HostedPaymentLink {
        errors.push(Issue::new(
            format!("{at}.op_kind"),
            "payment_requires_hosted_link",
            "A payment (P) operation must be a hosted_payment_link: the customer pays on the provider's page (D27).",
        ));
    }
    if extras.side_effect.needs_key() && *op.idempotency == Idempotency::None {
        let idem_path = format!("{at}.idempotency");
        let already = errors
            .iter()
            .any(|e| e.code == "idempotency_required" && e.path == idem_path);
        if !already {
            errors.push(Issue::new(
                idem_path,
                "idempotency_required",
                format!(
                    "This operation is {}: declare an Idempotency header or a dup_search; none is refused.",
                    extras.side_effect.as_str()
                ),
            ));
        }
    }
}

/// The declarative base every tenant template builds on (D31): it declares no ops of its own.
pub const GENERIC_REST: &str = "generic_rest";

/// Validates a platform template against the operation catalogue: the TS rules plus the server-only checks.
/// `generic_rest` may have no ops (TS P16 `publish_blockers` exempts it too).
pub fn validate_template(t: &ConnectorTemplate, defs: &[OpDef]) -> Vec<Issue> {
    if t.ops.is_empty() && t.key == GENERIC_REST {
        return Vec::new();
    }
    let views: Vec<OpView<'_>> = t
        .ops
        .iter()
        .map(|o| OpView {
            op: o.op_ref(),
            path: &o.path,
            request: &o.request,
            field_map: &o.field_map,
            idempotency: &o.idempotency,
        })
        .collect();
    let extras: Vec<PlatformExtras> = t
        .ops
        .iter()
        .map(|o| PlatformExtras {
            side_effect: o.side_effect,
            op_kind: o.op_kind,
        })
        .collect();
    let auth = Auth {
        kind: t.auth_kind,
        params: &t.auth_params,
    };
    validate_ops(auth, &t.credential_schema, &views, defs, Some(&extras))
}

/// TS `validateTenantTemplate`, exactly (golden-tested): no server-only checks.
pub fn validate_tenant_template(t: &TenantConnectorTemplate, defs: &[OpDef]) -> Vec<Issue> {
    let views: Vec<OpView<'_>> = t
        .ops
        .iter()
        .map(|o| OpView {
            op: o.op_ref(),
            path: &o.path,
            request: &o.request,
            field_map: &o.field_map,
            idempotency: &o.idempotency,
        })
        .collect();
    let auth = Auth {
        kind: t.auth_kind,
        params: &t.auth_params,
    };
    validate_ops(auth, &t.credential_schema, &views, defs, None)
}

/// The server-only checks a tenant template also gets before it may be pinned (secret in URL, host escape).
pub fn validate_tenant_template_server(t: &TenantConnectorTemplate, defs: &[OpDef]) -> Vec<Issue> {
    let mut issues = validate_tenant_template(t, defs);
    for (i, o) in t.ops.iter().enumerate() {
        let view = OpView {
            op: o.op_ref(),
            path: &o.path,
            request: &o.request,
            field_map: &o.field_map,
            idempotency: &o.idempotency,
        };
        let at = format!("ops[{i}] {}", op_key(&view.op));
        let side_effect = find_def(defs, &view.op).map_or(SideEffect::R, |d| d.side_effect);
        let extras = PlatformExtras {
            side_effect,
            op_kind: OpKind::Standard,
        };
        server_checks(&view, &extras, &at, &mut issues);
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::model::{ConnectorTemplateOp, TemplateStatus, Transport};
    use crate::connectors::wire::AuthKind;
    use agent_model::kinds::catalog::IdentityLevel;

    fn def(object: &str, verb: &str, se: SideEffect, inputs: &[&str], outputs: &[&str]) -> OpDef {
        OpDef {
            object: object.into(),
            verb: verb.into(),
            variant: "*".into(),
            side_effect: se,
            identity_level: IdentityLevel::Verified,
            input_fields: inputs.iter().map(|s| s.to_string()).collect(),
            output_fields: outputs.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn op(
        object: &str,
        verb: &str,
        se: SideEffect,
        path: &str,
        request: &str,
    ) -> ConnectorTemplateOp {
        ConnectorTemplateOp {
            input_types: Default::default(),
            object: object.into(),
            verb: verb.into(),
            variant: "*".into(),
            side_effect: se,
            op_kind: OpKind::Standard,
            method: "GET".into(),
            path: path.into(),
            request: request.into(),
            field_map: vec![],
            status_map: vec![],
            fault_map: vec![],
            pagination: None,
            idempotency: if se == SideEffect::R {
                Idempotency::Nat
            } else {
                Idempotency::Header {
                    header: "Idempotency-Key".into(),
                }
            },
            verify_read: None,
            timeout_ms: 5000,
            fixtures: vec![],
        }
    }

    fn template(ops: Vec<ConnectorTemplateOp>) -> ConnectorTemplate {
        ConnectorTemplate {
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
            rate_limit_model: serde_json::Value::Null,
            vendor_api_pin: String::new(),
            webhooks: serde_json::Value::Null,
            status: TemplateStatus::Active,
            t1: serde_json::Value::Null,
            t2: serde_json::Value::Null,
            published_by: None,
            published_at: None,
            drift: serde_json::Value::Null,
            ops,
            task_tools: Vec::new(),
        }
    }

    fn codes(issues: &[Issue]) -> Vec<&'static str> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn placeholder_scan_matches_ts_regex() {
        let found: Vec<(&str, &str)> =
            placeholders("{{{secret.x}} {{ canonical.a_1 }} {{Bad.x}} {{env.}}")
                .iter()
                .map(|p| (p.ns, p.name))
                .collect();
        assert_eq!(found, vec![("secret", "x"), ("canonical", "a_1")]);
    }

    /// (namespace, name, fallback (namespace, name)) of one scanned placeholder.
    type Scanned<'a> = (&'a str, &'a str, Option<(&'a str, &'a str)>);

    #[test]
    fn placeholder_fallback_like_ts() {
        let found: Vec<Scanned> = placeholders(
            "{{canonical.customer_email ?? env.requester_id}} {{ canonical.a??config.b }} {{canonical.c ?? }} {{canonical.d ?? Bad.x}}",
        )
        .iter()
        .map(|p| (p.ns, p.name, p.fallback))
        .collect();
        assert_eq!(
            found,
            vec![
                ("canonical", "customer_email", Some(("env", "requester_id"))),
                ("canonical", "a", Some(("config", "b"))),
            ]
        );
    }

    #[test]
    fn secretish_like_ts() {
        assert!(secretish("Authorization: Bearer abcdefghijkl"));
        assert!(!secretish("Bearer {{secret.token}}"));
        assert!(secretish("x sk_live_1"));
        assert!(secretish("apikey:12345678"));
        assert!(!secretish("apikey:1234567"));
        assert!(!secretish("Basic abc"));
    }

    #[test]
    fn server_checks_refuse_secret_url_host_and_payment() {
        let defs = [
            def("order", "read", SideEffect::R, &["order_id"], &["status"]),
            def("payment", "create", SideEffect::P, &["amount"], &["url"]),
            def("order", "cancel", SideEffect::X, &["order_id"], &["status"]),
        ];
        let mut cancel = op(
            "order",
            "cancel",
            SideEffect::X,
            "/orders/{{canonical.order_id}}",
            "",
        );
        cancel.idempotency = Idempotency::None;
        let t = template(vec![
            op(
                "order",
                "read",
                SideEffect::R,
                "/orders?key={{secret.token}}",
                "",
            ),
            op("order", "read", SideEffect::R, "https://evil.example/x", ""),
            op("order", "read", SideEffect::R, "//evil.example/x", ""),
            op(
                "payment",
                "create",
                SideEffect::P,
                "/pay",
                "{\"a\":\"{{canonical.amount}}\"}",
            ),
            cancel,
        ]);
        let issues = validate_template(&t, &defs);
        assert_eq!(
            codes(&issues),
            vec![
                "secret_in_url",
                "host_not_from_instance",
                "host_not_from_instance",
                "payment_requires_hosted_link",
                "idempotency_required"
            ]
        );
        assert_eq!(issues[0].path, "ops[0] order.read.path");
    }

    #[test]
    fn closed_transform_list_and_unknowns() {
        let defs = [def(
            "order",
            "read",
            SideEffect::R,
            &["order_id"],
            &["status"],
        )];
        let mut o = op(
            "order",
            "read",
            SideEffect::R,
            "/o",
            "{\"x\":\"{{canonical.nope}}\"}",
        );
        o.field_map = vec![
            FieldMapEntry {
                path: "$.status".into(),
                field: "status".into(),
                transform: Some("llm_summarise".into()),
            },
            FieldMapEntry {
                path: "status".into(),
                field: "colour".into(),
                transform: None,
            },
        ];
        let t = template(vec![o, op("order", "zap", SideEffect::R, "/z", "")]);
        let issues = validate_template(&t, &defs);
        assert_eq!(
            codes(&issues),
            vec![
                "unknown_placeholder",
                "transform_not_allowed",
                "unknown_field",
                "invalid_path",
                "unknown_operation"
            ]
        );
        assert!(validate_template(&template(vec![]), &defs)[0].code == "no_operations");
    }

    #[test]
    fn undeclared_canonical_is_allowed_only_left_of_a_fallback() {
        let defs = [def(
            "case",
            "create",
            SideEffect::W,
            &["subject"],
            &["case_id"],
        )];
        let issues_for = |request: &str| {
            let t = template(vec![op("case", "create", SideEffect::W, "/c", request)]);
            codes(&validate_template(&t, &defs))
                .into_iter()
                .filter(|c| *c == "unknown_placeholder")
                .count()
        };
        // Optional future input on the left of `??`, a valid fallback on the right: accepted.
        assert_eq!(
            issues_for(
                r#"{"s":"{{canonical.subject}}","by":"{{canonical.customer_email ?? env.requester_id}}"}"#
            ),
            0
        );
        // Bare undeclared canonical: rejected.
        assert_eq!(issues_for(r#"{"by":"{{canonical.customer_email}}"}"#), 1);
        // An undeclared canonical as the FALLBACK is still rejected.
        assert_eq!(
            issues_for(r#"{"by":"{{env.requester_id ?? canonical.customer_email}}"}"#),
            1
        );
        // The fallback itself is still checked.
        assert_eq!(
            issues_for(r#"{"by":"{{canonical.customer_email ?? secret.nope}}"}"#),
            1
        );
    }

    /// Platform seed v2 declares `customer_email` on `case.create` and every variant: against the real catalogue
    /// (`opdefs.json`) a bare `{{canonical.customer_email}}` is accepted, and the `?? env.*` fallback form still is.
    #[test]
    fn customer_email_is_a_declared_case_create_input() {
        let defs = crate::connectors::tenant_templates::op_defs();
        let issues_for = |variant: &str, request: &str| {
            let mut o = op("case", "create", SideEffect::W, "/c", request);
            o.variant = variant.into();
            let t = template(vec![o]);
            codes(&validate_template(&t, defs))
                .into_iter()
                .filter(|c| *c == "unknown_placeholder" || *c == "unknown_operation")
                .count()
        };
        for variant in [
            "*",
            "complaint",
            "dispute",
            "handover",
            "incident",
            "return",
            "service_request",
            "warranty_claim",
        ] {
            assert_eq!(
                issues_for(variant, r#"{"by":"{{canonical.customer_email}}"}"#),
                0,
                "{variant}: bare"
            );
            assert_eq!(
                issues_for(
                    variant,
                    r#"{"by":"{{canonical.customer_email ?? env.requester_id}}"}"#
                ),
                0,
                "{variant}: with a fallback"
            );
        }
        // still undeclared on other operations
        let t = template(vec![op(
            "case",
            "read",
            SideEffect::R,
            "/c",
            r#"{"by":"{{canonical.customer_email}}"}"#,
        )]);
        assert!(codes(&validate_template(&t, defs)).contains(&"unknown_placeholder"));
    }

    /// (namespace, name, fallback, optional) as the scan reads a placeholder.
    type ScannedPlaceholder<'a> = (&'a str, &'a str, Option<(&'a str, &'a str)>, bool);

    #[test]
    fn optional_marker_parses_and_is_not_the_fallback() {
        let found: Vec<ScannedPlaceholder> = placeholders(
            "{{canonical.a?}} {{ canonical.b ? }} {{canonical.c ?? env.d?}} {{canonical.e ?? env.f}} {{canonical.g ??}} {{canonical.h??}}",
        )
        .iter()
        .map(|p| (p.ns, p.name, p.fallback, p.optional))
        .collect();
        assert_eq!(
            found,
            vec![
                ("canonical", "a", None, true),
                ("canonical", "b", None, true),
                ("canonical", "c", Some(("env", "d")), true),
                ("canonical", "e", Some(("env", "f")), false),
            ]
        );
    }

    #[test]
    fn optional_placeholder_is_accepted_where_it_can_drop_and_refused_elsewhere() {
        let defs = [def(
            "slot",
            "list",
            SideEffect::R,
            &["from", "practitioner"],
            &["slots"],
        )];
        let issues_for = |path: &str, request: &str| {
            let t = template(vec![op("slot", "list", SideEffect::R, path, request)]);
            validate_template(&t, &defs)
                .into_iter()
                .filter(|i| i.code == "optional_not_droppable")
                .map(|i| i.path)
                .collect::<Vec<_>>()
        };
        let none: Vec<String> = Vec::new();
        // whole query value (first, middle, last) and whole JSON object member value
        assert_eq!(issues_for("/s?from={{canonical.from?}}", ""), none);
        assert_eq!(issues_for("/s?a=1&from={{canonical.from?}}&b=2", ""), none);
        assert_eq!(issues_for("/s?a=1&p={{canonical.practitioner?}}", ""), none);
        assert_eq!(
            issues_for("/s", r#"{"a":1, "from" : "{{canonical.from?}}" }"#),
            none
        );
        assert_eq!(
            issues_for("/s", r#"{"from":"{{canonical.from ?? env.team?}}","b":2}"#),
            none
        );
        // path segment, query key, embedded in a query value, embedded in a string, array element, bare, no query
        assert_eq!(
            issues_for("/s/{{canonical.from?}}", ""),
            vec!["ops[0] slot.list.path"]
        );
        assert_eq!(
            issues_for("/s/{{canonical.from?}}?a=1", ""),
            vec!["ops[0] slot.list.path"]
        );
        assert_eq!(
            issues_for("/s?{{canonical.from?}}=1", ""),
            vec!["ops[0] slot.list.path"]
        );
        assert_eq!(
            issues_for("/s?from=x{{canonical.from?}}", ""),
            vec!["ops[0] slot.list.path"]
        );
        assert_eq!(
            issues_for("/s", r#"{"from":"at {{canonical.from?}}"}"#),
            vec!["ops[0] slot.list.request"]
        );
        assert_eq!(
            issues_for("/s", r#"{"from":["{{canonical.from?}}"]}"#),
            vec!["ops[0] slot.list.request"]
        );
        assert_eq!(
            issues_for("/s", r#"{"from":{{canonical.from?}}}"#),
            vec!["ops[0] slot.list.request"]
        );
    }

    #[test]
    fn kv_line_needs_an_uppercase_key() {
        assert_eq!(kv_key("kv_line:STATUS"), Some("STATUS"));
        assert_eq!(kv_key("kv_line:MANAGER_EMAIL2"), Some("MANAGER_EMAIL2"));
        for bad in [
            "kv_line",
            "kv_line:",
            "kv_line:status",
            "kv_line:A B",
            "kv_line:A-B",
            "kv_line :A",
        ] {
            assert_eq!(kv_key(bad), None, "{bad}");
            assert!(!transform_allowed(bad), "{bad}");
        }
        assert!(transform_allowed("kv_line:EMAIL"));
        assert!(transform_allowed("mask_last4"));
        let defs = [def(
            "party",
            "read",
            SideEffect::R,
            &["party_id"],
            &["email"],
        )];
        let mut o = op("party", "read", SideEffect::R, "/v1/ad/execute", "");
        o.field_map = vec![FieldMapEntry {
            path: "$.stdout".into(),
            field: "email".into(),
            transform: Some("kv_line:EMAIL".into()),
        }];
        assert!(validate_template(&template(vec![o.clone()]), &defs).is_empty());
        if let Some(f) = o.field_map.first_mut() {
            f.transform = Some("kv_line:email".into());
        }
        assert_eq!(
            codes(&validate_template(&template(vec![o]), &defs)),
            vec!["transform_not_allowed"]
        );
    }
}
