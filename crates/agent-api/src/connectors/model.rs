//! B7 T4: serde mirrors of the connector template shapes of `web/src/lib/api/types/connectors.ts`
//! (`ConnectorTemplate`, `ConnectorTemplateOp`, `TenantConnectorTemplate`, `TenantTemplateOp`, …) plus `OpDef`, the
//! Rust twin of the TS `OpDefLite` the validator reads. Field names are the TS ones (snake_case, as the API sends);
//! unknown fields are ignored (no `deny_unknown_fields`) so the seed and the golden files can carry more than we read.
use std::collections::BTreeMap;

use agent_model::kinds::catalog::IdentityLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::wire::AuthKind;

/// `object.verb{variant}`; `variant = "*"` is the unqualified operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OpRef {
    pub object: String,
    pub verb: String,
    #[serde(default = "star")]
    pub variant: String,
}

fn star() -> String {
    "*".to_owned()
}

/// `SideEffect` of the TS types: read, write, irreversible, payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideEffect {
    R,
    W,
    X,
    P,
}

impl SideEffect {
    /// TS `needsKey`: every side effect other than a read needs an idempotency mapping.
    pub fn needs_key(self) -> bool {
        self != SideEffect::R
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SideEffect::R => "R",
            SideEffect::W => "W",
            SideEffect::X => "X",
            SideEffect::P => "P",
        }
    }
}

/// `Transport` of the TS types (same literals).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Rest,
    Graphql,
    Platform,
    Stub,
    Soap,
    Graph,
    Ldap,
    Mcp,
    Table,
}

/// `TemplateStatus` of the TS types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateStatus {
    Draft,
    Active,
    Retired,
}

/// `OpKind` of the TS types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    #[default]
    Standard,
    HostedPaymentLink,
    Builtin,
}

/// Per-op idempotency mapping (doc 10 §4.2, doc 12 §3.7). `none` is refused for W/X/P at validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Idempotency {
    Nat,
    Header { header: String },
    DupSearch { on: String },
    None,
}

/// One field of `credential_schema`: `secret` fields → sealed credential (`{{secret.*}}`), the rest → config
/// (`{{config.*}}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialField {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Response mapping entry: system JSONPath → canonical field, through an optional transform (absent = `none` on
/// platform templates; tenant templates always carry one in TS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldMapEntry {
    pub path: String,
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

/// System status → canonical status of the object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMapEntry {
    pub system: String,
    pub canonical: String,
}

/// Vendor fault → canonical error code. `system` is free text; `HTTP <code>[ suffix]` / `HTTP <n>xx` entries are
/// the ones the mapping engine can match on a status code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultMapEntry {
    pub system: String,
    pub code: String,
}

/// A recorded, redacted response of one op (`status` 0 = timeout/fault injection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateFixture {
    pub name: String,
    pub status: u16,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub body: Value,
    /// Input key of a stub fixture (`{"path"?, "query"?: {k: v}}`): the stub transport answers with it when the
    /// rendered request matches (crate::connectors::transport::stub).
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_on: Option<Value>,
}

/// The type and description a declared input carries into the model's tool spec (a template op's `input_types`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputType {
    /// A JSON-schema type (`string`, `integer`, `number`, `boolean`, `array`, `object`).
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub description: String,
}

/// A row of `connector_template_op` (doc 11 §6.1, doc 12 §8.1). The `OpRef` fields are inline as in TS
/// (`extends OpRef`); [`ConnectorTemplateOp::op_ref`] builds the reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorTemplateOp {
    pub object: String,
    pub verb: String,
    #[serde(default = "star")]
    pub variant: String,
    pub side_effect: SideEffect,
    #[serde(default)]
    pub op_kind: OpKind,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub field_map: Vec<FieldMapEntry>,
    #[serde(default)]
    pub status_map: Vec<StatusMapEntry>,
    #[serde(default)]
    pub fault_map: Vec<FaultMapEntry>,
    #[serde(default)]
    pub pagination: Option<String>,
    pub idempotency: Idempotency,
    #[serde(default)]
    pub verify_read: Option<String>,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub fixtures: Vec<TemplateFixture>,
    /// The canonical inputs' JSON-schema type and description (0243), e.g. from an MCP tool's `inputSchema`; the
    /// assistant's tool spec carries them to the model. An input without an entry is offered by name only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_types: BTreeMap<String, InputType>,
}

impl ConnectorTemplateOp {
    pub fn op_ref(&self) -> OpRef {
        OpRef {
            object: self.object.clone(),
            verb: self.verb.clone(),
            variant: self.variant.clone(),
        }
    }
}

/// `connector_template@version` (platform, C3) — P16/P17. Display-only blocks (`rate_limit_model`, `webhooks`,
/// tier results, `drift`) are kept as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorTemplate {
    pub key: String,
    pub version: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub system_class: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub wave: String,
    #[serde(default)]
    pub platform_connector: bool,
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_transport: Option<Transport>,
    pub auth_kind: AuthKind,
    #[serde(default)]
    pub auth_detail: String,
    /// Machine-readable auth parameters (header name, token URL template, …); server-side addition.
    #[serde(default)]
    pub auth_params: BTreeMap<String, String>,
    #[serde(default)]
    pub credential_schema: Vec<CredentialField>,
    #[serde(default)]
    pub rate_limit_model: Value,
    #[serde(default)]
    pub vendor_api_pin: String,
    #[serde(default)]
    pub webhooks: Value,
    pub status: TemplateStatus,
    #[serde(default)]
    pub t1: Value,
    #[serde(default)]
    pub t2: Value,
    #[serde(default)]
    pub published_by: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub drift: Value,
    #[serde(default)]
    pub ops: Vec<ConnectorTemplateOp>,
    /// The operations this template runs as durable MCP tasks (`object.verb` keys); a platform template leaves it
    /// empty — its task tools are the global `MCP_TASK_TOOLS`. Empty is omitted from the value, so a template with
    /// none serialises exactly as before the field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_tools: Vec<String>,
}

/// One op of a tenant-private template (D31): declarative mapping only (OQ-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantTemplateOp {
    pub object: String,
    pub verb: String,
    #[serde(default = "star")]
    pub variant: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub field_map: Vec<FieldMapEntry>,
    #[serde(default)]
    pub status_map: Vec<StatusMapEntry>,
    pub idempotency: Idempotency,
    /// The canonical inputs' JSON-schema type and description (0243), e.g. from an MCP tool's `inputSchema`; the
    /// assistant's tool spec carries them to the model. An input without an entry is offered by name only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_types: BTreeMap<String, InputType>,
    /// The tenant's own definition of an op no platform catalogue knows (0246): authored in the template editor, or
    /// filled by mcp-import from the MCP tool. `None` = the platform catalogue defines it (a platform op is never
    /// shadowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub def: Option<OwnOpDef>,
}

/// A tenant's own definition of an operation (0246): what the platform catalogue says of its own ops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnOpDef {
    pub side_effect: SideEffect,
    #[serde(default)]
    pub input_fields: Vec<String>,
    #[serde(default)]
    pub output_fields: Vec<String>,
    #[serde(default)]
    pub description: String,
}

impl TenantTemplateOp {
    pub fn op_ref(&self) -> OpRef {
        OpRef {
            object: self.object.clone(),
            verb: self.verb.clone(),
            variant: self.variant.clone(),
        }
    }
}

/// A tenant-private template (D31): generic (built on generic REST) or custom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantConnectorTemplate {
    #[serde(default)]
    pub tenant_id: String,
    pub key: String,
    pub version: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub base: String,
    pub transport: Transport,
    pub auth_kind: AuthKind,
    /// The server-side `auth_params` (same keys as [`ConnectorTemplate::auth_params`]); absent = none.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "text_pairs"
    )]
    pub auth_params: BTreeMap<String, String>,
    #[serde(default)]
    pub credential_schema: Vec<CredentialField>,
    pub status: TemplateStatus,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub ops: Vec<TenantTemplateOp>,
    /// The operations this template runs as durable MCP tasks (`object.verb` keys); each must name one of its own
    /// `ops` when they are known. Empty is omitted from the value (see [`ConnectorTemplate::task_tools`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_tools: Vec<String>,
    #[serde(default)]
    pub validation: Value,
    #[serde(default)]
    pub contract: Value,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub updated_at: String,
}

/// An object's text values (other values and a non-object are skipped, like `TemplateRow::auth_params`).
fn text_pairs<'de, D>(d: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Value::deserialize(d)?;
    Ok(v.as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default())
}

/// `auth_params` key of the CSRF prefetch path (see [`CsrfPrefetch`]).
pub const CSRF_FETCH_PATH: &str = "csrf_fetch_path";
/// `auth_params` key of the CSRF header name (see [`CsrfPrefetch`]).
pub const CSRF_HEADER: &str = "csrf_header";
/// The CSRF header when `auth_params.csrf_header` is absent (SAP Gateway / S/4HANA OData).
pub const CSRF_DEFAULT_HEADER: &str = "x-csrf-token";
/// The credential fields an `mtls` template must declare as secrets (TS `MTLS_FIELDS`); `ca_pem` is optional.
pub const MTLS_FIELDS: [&str; 2] = ["client_cert_pem", "client_key_pem"];
/// `auth_params` keys that would turn TLS certificate verification off: refused by the validator (TS
/// `TLS_VERIFY_OFF`) and by the transport. There is no such option; trust a private CA with `ca_pem` instead.
pub const TLS_VERIFY_OFF: [&str; 5] = [
    "danger_accept_invalid_certs",
    "insecure_skip_verify",
    "tls_insecure",
    "tls_verify",
    "verify_tls",
];

/// Template-level CSRF token prefetch (SAP S/4HANA OData writes), carried in the template's `auth_params` like
/// every other machine-readable auth parameter (no schema change; tenant and platform templates alike):
///
/// ```json
/// "auth_params": {"csrf_fetch_path": "/sap/opu/odata/sap/API_SALES_ORDER_SRV/", "csrf_header": "x-csrf-token"}
/// ```
///
/// `csrf_fetch_path` turns it on: a path relative to the instance base URL, starting with `/`. `csrf_header` is
/// optional (default `x-csrf-token`). Before every non-R (W/X/P) operation the transport sends
/// `GET {base}{csrf_fetch_path}` with the instance auth and `{csrf_header}: Fetch`, then sends the write with
/// `{csrf_header}: <the token answered>` and the prefetch's `Set-Cookie` session cookies as `Cookie` (that one
/// request only: there is no cookie jar). Reads are never prefetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrfPrefetch {
    pub fetch_path: String,
    pub header: String,
}

impl CsrfPrefetch {
    /// The prefetch of a template's `auth_params`, when `csrf_fetch_path` is set (non-blank).
    pub fn from_auth_params(params: &BTreeMap<String, String>) -> Option<Self> {
        let fetch_path = params
            .get(CSRF_FETCH_PATH)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())?;
        let header = params
            .get(CSRF_HEADER)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(CSRF_DEFAULT_HEADER);
        Some(Self {
            fetch_path: fetch_path.to_owned(),
            header: header.to_owned(),
        })
    }
}

/// What the operation catalogue says an op must accept and produce: TS `OpDefLite` (subset of `operation_def`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpDef {
    pub object: String,
    pub verb: String,
    #[serde(default = "star")]
    pub variant: String,
    pub side_effect: SideEffect,
    /// The identity level the operation needs (`0`, `1`, `2`, `3`, `S`). Required: a contract entry without one
    /// does not parse, so an operation can never be silently treated as level 2.
    pub identity_level: IdentityLevel,
    #[serde(default)]
    pub input_fields: Vec<String>,
    #[serde(default)]
    pub output_fields: Vec<String>,
}

impl OpDef {
    pub fn op_ref(&self) -> OpRef {
        OpRef {
            object: self.object.clone(),
            verb: self.verb.clone(),
            variant: self.variant.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_is_tagged_by_kind() {
        let i: Idempotency =
            serde_json::from_str(r#"{"kind":"dup_search","on":"x"}"#).expect("parse");
        assert_eq!(i, Idempotency::DupSearch { on: "x".into() });
        let n: Idempotency = serde_json::from_str(r#"{"kind":"none"}"#).expect("parse");
        assert_eq!(n, Idempotency::None);
    }
}
