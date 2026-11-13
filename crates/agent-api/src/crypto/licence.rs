//! Tenant and member licence tokens at rest (B7 T1; replaces the retired `license_token.rs`).
//!
//! Tokens are sealed under the [`Purpose::LicenceToken`] keyring (crate::crypto::keyring): `v2:<kid>:…`,
//! AES-256-GCM, associated data = the tenant id's 16 bytes ‖ kid, so a token copied onto another tenant's row (or
//! relabelled with another kid) does not open. A token that does not open is an error, never "no licence".
//!
//! Keys (`LicenceKeys::from_env`):
//! - `LICENSE_TOKEN_ENC_KEYS` (JSON `{kid: base64 32 bytes}`) + `LICENSE_TOKEN_ENC_KEY_CURRENT`: the key set.
//! - **Transitional rule:** when both are unset but the legacy `LICENSE_TOKEN_ENC_KEY` (base64 of 32 bytes) is set,
//!   the key set is derived from it: one key, kid [`LEGACY_KID`] (`v1`), current. Production still boots this way
//!   (its tokens are `v2:v1:…` under those bytes). Neither set fails the boot naming `LICENSE_TOKEN_ENC_KEYS`. Remove
//!   the fallback once every environment carries the new variables (the legacy bytes then live on under kid `v1`).
//!
//! The legacy `v1:` token format is gone (0151: production resealed every one; the tables admit `v2:` only).
use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use uuid::Uuid;

use crate::crypto::keyring::{KEY_LEN, KeySet, Keyring, KeyringError, Purpose};

/// The legacy single-key variable (transitional; see the module doc).
pub const LEGACY_KEY_VAR: &str = "LICENSE_TOKEN_ENC_KEY";
/// The kid of a key set derived from [`LEGACY_KEY_VAR`].
pub const LEGACY_KID: &str = "v1";

/// From base64 (standard alphabet, padded): exactly 32 bytes, else an error that never echoes the value.
fn key_from_base64(s: &str) -> Result<[u8; KEY_LEN], String> {
    let raw = STANDARD
        .decode(s.trim())
        .map_err(|_| "is not valid base64".to_string())?;
    raw.as_slice()
        .try_into()
        .map_err(|_| format!("must decode to {KEY_LEN} bytes, not {}", raw.len()))
}

/// The licence-token key set from the environment.
#[derive(Clone, Debug)]
pub struct LicenceKeys {
    pub set: KeySet,
    /// The set was derived from [`LEGACY_KEY_VAR`] (the transitional rule), not read from `LICENSE_TOKEN_ENC_KEYS`.
    pub derived: bool,
}

impl LicenceKeys {
    /// The raw values of `LICENSE_TOKEN_ENC_KEYS`, `LICENSE_TOKEN_ENC_KEY_CURRENT` and `LICENSE_TOKEN_ENC_KEY`
    /// (`None` or blank = unset). Errors name the variable, never a key. The legacy key is used only when the set is
    /// absent; a malformed one fails the boot either way.
    pub fn from_env(
        keys_json: Option<&str>,
        current: Option<&str>,
        legacy_b64: Option<&str>,
    ) -> Result<Self, String> {
        let legacy = match legacy_b64.map(str::trim).filter(|s| !s.is_empty()) {
            Some(b) => Some(key_from_base64(b).map_err(|why| format!("{LEGACY_KEY_VAR} {why}"))?),
            None => None,
        };
        let p = Purpose::LicenceToken;
        if let Some(set) = KeySet::parse(p, keys_json, current)? {
            return Ok(Self {
                set,
                derived: false,
            });
        }
        let Some(k) = legacy else {
            return Err(format!(
                "{} is not set (nor {}, nor the legacy {LEGACY_KEY_VAR})",
                p.keys_var(),
                p.current_var()
            ));
        };
        Ok(Self {
            set: KeySet {
                current: LEGACY_KID.to_string(),
                keys: BTreeMap::from([(LEGACY_KID.to_string(), k)]),
            },
            derived: true,
        })
    }
}

/// Seals and opens `v2:` licence tokens. `Debug` shows no key.
#[derive(Debug)]
pub struct LicenceCipher {
    ring: Keyring,
}

impl LicenceCipher {
    pub fn new(keys: &LicenceKeys) -> Self {
        Self {
            ring: Keyring::new(Purpose::LicenceToken, &keys.set),
        }
    }

    /// The kid new tokens are sealed under.
    pub fn current_kid(&self) -> &str {
        self.ring.current_kid()
    }

    /// `token` sealed for `tenant` under the current kid (`v2:<kid>:…`), with a fresh nonce.
    pub fn seal(&self, tenant: Uuid, token: &str) -> Result<String, KeyringError> {
        self.ring.seal(tenant.as_bytes(), token.as_bytes())
    }

    /// The token `stored` holds for `tenant`: a `v2:` value under the kid it names.
    pub fn open(&self, tenant: Uuid, stored: &str) -> Result<String, KeyringError> {
        let plain = self.ring.open(tenant.as_bytes(), stored)?;
        String::from_utf8(plain).map_err(|_| KeyringError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(byte: u8) -> String {
        STANDARD.encode([byte; KEY_LEN])
    }

    fn tenant() -> Uuid {
        Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap()
    }

    #[test]
    fn legacy_key_alone_derives_a_v1_kid_set() {
        let k = LicenceKeys::from_env(None, None, Some(&b64(7))).unwrap();
        assert!(k.derived);
        assert_eq!(k.set.current, LEGACY_KID);
        assert_eq!(k.set.keys[LEGACY_KID], [7u8; KEY_LEN]);
        let c = LicenceCipher::new(&k);
        let sealed = c.seal(tenant(), "lic-tok").unwrap();
        assert!(sealed.starts_with("v2:v1:"), "{sealed}");
        assert_eq!(c.open(tenant(), &sealed).unwrap(), "lic-tok");
    }

    #[test]
    fn nothing_set_names_the_new_variable() {
        let e = LicenceKeys::from_env(None, Some(" "), None).unwrap_err();
        assert!(e.contains("LICENSE_TOKEN_ENC_KEYS"), "{e}");
        let e = LicenceKeys::from_env(None, None, Some("%%%")).unwrap_err();
        assert!(e.contains(LEGACY_KEY_VAR) && e.contains("base64"), "{e}");
    }

    #[test]
    fn the_new_set_wins_over_the_legacy_key() {
        let keys = format!(r#"{{"k2":"{}"}}"#, b64(2));
        let k = LicenceKeys::from_env(Some(&keys), Some("k2"), Some(&b64(7))).unwrap();
        assert!(!k.derived);
        assert_eq!(k.set.current, "k2");
        let c = LicenceCipher::new(&k);
        let new = c.seal(tenant(), "lic").unwrap();
        assert!(new.starts_with("v2:k2:"), "{new}");
        assert_eq!(c.current_kid(), "k2");
        // a token sealed under the derived set (kid `v1`) does not open once the set lacks kid `v1`
        let legacy = LicenceCipher::new(&LicenceKeys::from_env(None, None, Some(&b64(7))).unwrap());
        let old = legacy.seal(tenant(), "lic").unwrap();
        assert_eq!(c.open(tenant(), &old), Err(KeyringError::UnknownKid));
    }

    #[test]
    fn the_legacy_bytes_under_kid_v1_keep_opening_derived_tokens() {
        let legacy = LicenceCipher::new(&LicenceKeys::from_env(None, None, Some(&b64(7))).unwrap());
        let old = legacy.seal(tenant(), "t").unwrap();
        let keys = format!(r#"{{"v1":"{}","k2":"{}"}}"#, b64(7), b64(2));
        let c = LicenceCipher::new(&LicenceKeys::from_env(Some(&keys), Some("k2"), None).unwrap());
        assert_eq!(c.open(tenant(), &old).unwrap(), "t");
    }

    #[test]
    fn a_legacy_v1_value_or_garbage_does_not_open() {
        let c = LicenceCipher::new(&LicenceKeys::from_env(None, None, Some(&b64(7))).unwrap());
        for bad in ["v1:AAAA", "plaintext"] {
            assert!(c.open(tenant(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_v2_token_moved_to_another_tenant_does_not_open() {
        let c = LicenceCipher::new(&LicenceKeys::from_env(None, None, Some(&b64(7))).unwrap());
        let sealed = c.seal(tenant(), "t").unwrap();
        assert_eq!(
            c.open(Uuid::new_v4(), &sealed),
            Err(KeyringError::Undecryptable)
        );
    }

    #[test]
    fn debug_never_prints_key_bytes() {
        let k = LicenceKeys::from_env(None, None, Some(&b64(0xAB))).unwrap();
        let c = LicenceCipher::new(&k);
        for shown in [format!("{k:?}"), format!("{c:?}")] {
            assert!(
                !shown.contains("171") && !shown.contains(&b64(0xAB)),
                "{shown}"
            );
        }
    }
}
