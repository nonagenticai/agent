//! Encryption at rest (B7). [`keyring`]: the purpose-separated `v2:` keyring (B7 T8 uses
//! [`keyring::Purpose::TenantData`] to wrap the per-tenant data keys of crate::connectors::custody); [`licence`]: the
//! tenant / member licence tokens under [`keyring::Purpose::LicenceToken`] (B7 T1).
pub mod keyring;
pub mod licence;
