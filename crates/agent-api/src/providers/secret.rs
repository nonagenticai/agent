//! Provider and source secrets (plan §0 R2): LLM provider keys, slot keys and source credentials are sealed with the
//! ProviderSecret keyring (crate::crypto::keyring, `PROVIDER_SECRET_KEYS` / `PROVIDER_SECRET_KEY_CURRENT`) into one
//! `v2:` text column, bound to `<tenant>:<kind>:<row id>` so a value copied to another row or tenant never opens.
//!
//! - Keyring unset: every seal and open is 503 `provider_secret_key_unconfigured`.
//! - A stored value that no longer opens (unknown kid, wrong key, moved or corrupt) is 409 `secret_unreadable`;
//!   the keyring's error text never reaches a response.
//! - Responses, audit rows, ledger values and snapshots carry `{set, fingerprint}` only: [`fingerprint`] is the first
//!   8 hex of SHA-256 over `<tenant>:<kind>:` and the plaintext.
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::crypto::keyring::Keyring;
use crate::error::{ApiError, ApiResult};

/// What a sealed secret is; part of its AAD scope and its fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    /// `llm_provider.secret_sealed`.
    LlmKey,
    /// `provider_slot_secret.key_sealed`.
    SlotKey,
    /// `source.credentials_sealed`.
    SourceCred,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::LlmKey => "llm_key",
            SecretKind::SlotKey => "slot_key",
            SecretKind::SourceCred => "source_cred",
        }
    }
}

/// The AAD scope of one row's secret: `<tenant>:<kind>:<row id>`.
pub fn aad(tenant: Uuid, kind: SecretKind, row: Uuid) -> String {
    format!("{tenant}:{}:{row}", kind.as_str())
}

/// First 8 hex of SHA-256(`<tenant>:<kind>:` ‖ plaintext): stable for one tenant and kind, different across tenants.
pub fn fingerprint(tenant: Uuid, kind: SecretKind, plaintext: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("{tenant}:{}:", kind.as_str()).as_bytes());
    h.update(plaintext.as_bytes());
    let mut hex = hex::encode(h.finalize());
    hex.truncate(8);
    hex
}

/// The keyring, or 503 `provider_secret_key_unconfigured`.
pub fn keyring(kr: Option<&Keyring>) -> ApiResult<&Keyring> {
    kr.ok_or_else(|| {
        ApiError::Unavailable(
            "provider_secret_key_unconfigured",
            "provider secrets are not configured on this deployment (PROVIDER_SECRET_KEYS)".into(),
            None,
        )
    })
}

/// A sealed secret and its fingerprint. `Debug` shows the fingerprint only.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The `v2:<kid>:…` value to store.
    pub sealed: String,
    pub fingerprint: String,
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed")
            .field("sealed", &"[redacted]")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// An opened secret. `Debug` never prints it; read it with [`SecretText::expose`] at the one place it is sent.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretText(String);

impl SecretText {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretText([redacted])")
    }
}

/// `plaintext` sealed for row `row` of `tenant`.
pub fn seal(
    kr: Option<&Keyring>,
    tenant: Uuid,
    kind: SecretKind,
    row: Uuid,
    plaintext: &str,
) -> ApiResult<Sealed> {
    let kr = keyring(kr)?;
    let sealed = kr
        .seal(aad(tenant, kind, row).as_bytes(), plaintext.as_bytes())
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("could not seal a provider secret")))?;
    Ok(Sealed {
        sealed,
        fingerprint: fingerprint(tenant, kind, plaintext),
    })
}

/// The plaintext of row `row`'s stored `sealed` value, or 409 `secret_unreadable`.
pub fn open(
    kr: Option<&Keyring>,
    tenant: Uuid,
    kind: SecretKind,
    row: Uuid,
    sealed: &str,
) -> ApiResult<SecretText> {
    let kr = keyring(kr)?;
    let unreadable = || {
        ApiError::Conflict(
            "secret_unreadable",
            "the stored secret can no longer be read; enter it again".into(),
        )
    };
    let bytes = kr
        .open(aad(tenant, kind, row).as_bytes(), sealed)
        .map_err(|_| unreadable())?;
    String::from_utf8(bytes)
        .map(SecretText)
        .map_err(|_| unreadable())
}
