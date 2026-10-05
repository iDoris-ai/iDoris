//! Virtual-key secret material for B5 caller identity.
//!
//! Wire tokens are `idk_<64 lowercase hex>`. Persistence must store only
//! [`VirtualKeyHash`] plus scope metadata; never [`VirtualKeySecret`].

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub mod store;

pub const VIRTUAL_KEY_PREFIX: &str = "idk_";
const SECRET_HEX_LEN: usize = 64;
pub const VIRTUAL_KEY_LEN: usize = VIRTUAL_KEY_PREFIX.len() + SECRET_HEX_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKeyError {
    InvalidFormat,
}

impl std::fmt::Display for VirtualKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid iDoris virtual key")
    }
}

impl std::error::Error for VirtualKeyError {}

pub struct VirtualKeySecret(String);

impl VirtualKeySecret {
    pub fn parse(raw: &str) -> Result<Self, VirtualKeyError> {
        if raw.len() != VIRTUAL_KEY_LEN || !raw.starts_with(VIRTUAL_KEY_PREFIX) {
            return Err(VirtualKeyError::InvalidFormat);
        }
        let payload = &raw[VIRTUAL_KEY_PREFIX.len()..];
        if !payload
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(VirtualKeyError::InvalidFormat);
        }
        Ok(Self(raw.to_owned()))
    }

    /// Explicit secret exposure for the one-time issuance response or
    /// Authorization header construction. Never log this value.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn hash(&self) -> VirtualKeyHash {
        VirtualKeyHash::from_secret(self)
    }
}

impl std::fmt::Debug for VirtualKeySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VirtualKeySecret([REDACTED])")
    }
}

impl std::fmt::Display for VirtualKeySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct VirtualKeyHash([u8; 32]);

impl VirtualKeyHash {
    pub fn from_secret(secret: &VirtualKeySecret) -> Self {
        let digest = Sha256::digest(secret.expose_secret().as_bytes());
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn matches(&self, secret: &VirtualKeySecret) -> bool {
        let candidate = Self::from_secret(secret);
        bool::from(self.0.ct_eq(&candidate.0))
    }

    /// Short non-secret correlation value for diagnostics. The full hash is
    /// still the persistence/lookup key; this prefix is display-only.
    pub fn fingerprint(&self) -> String {
        hex(&self.0[..6])
    }

    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

impl std::fmt::Debug for VirtualKeyHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("VirtualKeyHash")
            .field(&self.fingerprint())
            .finish()
    }
}

#[derive(Debug)]
pub struct MintedVirtualKey {
    pub key_id: String,
    pub secret: VirtualKeySecret,
    pub hash: VirtualKeyHash,
}

impl MintedVirtualKey {
    pub fn mint() -> Self {
        // UUID v4 is backed by the platform RNG. Two independent UUIDs
        // retain about 244 random bits after version/variant bits.
        let payload = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let secret = VirtualKeySecret(format!("{VIRTUAL_KEY_PREFIX}{payload}"));
        let hash = secret.hash();
        Self {
            key_id: format!("vk_{}", Uuid::new_v4().simple()),
            secret,
            hash,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn minted_keys_are_distinct_canonical_and_stably_hashed() {
        let first = MintedVirtualKey::mint();
        let second = MintedVirtualKey::mint();
        assert_ne!(first.key_id, second.key_id);
        assert_ne!(first.secret.expose_secret(), second.secret.expose_secret());
        assert_eq!(first.secret.expose_secret().len(), VIRTUAL_KEY_LEN);
        let reparsed = VirtualKeySecret::parse(first.secret.expose_secret()).unwrap();
        assert_eq!(reparsed.hash(), first.hash);
        assert!(first.hash.matches(&reparsed));
        assert!(!first.hash.matches(&second.secret));
        assert_eq!(first.hash.to_hex().len(), 64);
        assert_eq!(first.hash.fingerprint().len(), 12);
    }

    #[test]
    fn malformed_or_noncanonical_keys_are_rejected() {
        let valid = MintedVirtualKey::mint();
        let raw = valid.secret.expose_secret();
        for invalid in [
            "",
            "idk_",
            "sk_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "idk_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
        ] {
            assert_eq!(
                VirtualKeySecret::parse(invalid).unwrap_err(),
                VirtualKeyError::InvalidFormat
            );
        }
        let uppercase = raw.to_ascii_uppercase();
        assert_eq!(
            VirtualKeySecret::parse(&uppercase).unwrap_err(),
            VirtualKeyError::InvalidFormat
        );
        let oversized = format!("{raw}0");
        assert_eq!(
            VirtualKeySecret::parse(&oversized).unwrap_err(),
            VirtualKeyError::InvalidFormat
        );
    }

    #[test]
    fn secret_never_enters_debug_display_or_parse_errors() {
        let minted = MintedVirtualKey::mint();
        let sentinel = minted.secret.expose_secret();
        let rendered = format!("{:?} {} {:?}", minted.secret, minted.secret, minted);
        assert!(!rendered.contains(sentinel));

        let invalid = format!("bad-{sentinel}");
        let error = VirtualKeySecret::parse(&invalid).unwrap_err();
        let error_text = format!("{error} {error:?}");
        assert!(!error_text.contains(sentinel));
        assert!(!error_text.contains(&invalid));
    }
}
