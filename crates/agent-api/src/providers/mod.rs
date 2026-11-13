//! Knowledge + providers (B3). [`secret`]: provider/source secrets sealed under the ProviderSecret keyring (plan §0
//! R2). [`check_url`]: every tenant-entered URL (provider and slot `base_url`, source URLs) through
//! crate::netguard::check_url, refused with the UI's codes (`web/src/lib/api/areas/tenant.ts` `urlProblem` and its
//! callers; plan §0 R1). Internal (D70) URLs come from config and never pass here.
pub mod kinds;
pub mod llm;
pub mod secret;
pub mod slots;
pub mod targets;

use reqwest::Url;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::netguard::{self, Policy};

/// Which field a tenant-entered URL fills: the refusal codes differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlField {
    /// An LLM provider's or a provider slot's `base_url`: 422 `invalid_base_url` / `invalid_target`.
    BaseUrl,
    /// A knowledge source's URL: 422 `invalid_source` / `address_refused`.
    Source,
}

/// `raw` checked by crate::netguard::check_url (https only; no credentials, query or fragment; no loopback, private,
/// link-local, CGNAT, cluster, LAN or dotless host unless `policy` admits it), with netguard's `invalid_url` /
/// `invalid_target` mapped to `field`'s 422 codes.
pub fn check_url(raw: &str, policy: &Policy, tenant: Uuid, field: UrlField) -> ApiResult<Url> {
    netguard::check_url(raw, policy, tenant).map_err(|e| {
        let target = e.code == "invalid_target";
        let code = match (field, target) {
            (UrlField::BaseUrl, true) => "invalid_target",
            (UrlField::BaseUrl, false) => "invalid_base_url",
            (UrlField::Source, true) => "address_refused",
            (UrlField::Source, false) => "invalid_source",
        };
        let message = if target {
            e.detail
        } else {
            "Enter a full https:// URL; leave credentials, query and fragment out of it (keys go in the key field)."
                .to_string()
        };
        ApiError::Unprocessable(code, message)
    })
}
