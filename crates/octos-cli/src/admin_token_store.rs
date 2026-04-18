//! Hashed admin auth token stored at `{data_dir}/admin_token.json`.
//! Replaces the static config/env bootstrap token once rotated.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminTokenRecord {
    /// 16 random bytes, base64 (URL-safe, no padding).
    pub salt: String,
    /// sha256(salt_bytes || token_bytes), base64 (URL-safe, no padding).
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

impl AdminTokenRecord {
    pub fn from_plaintext(token: &str) -> Self {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let mut salt_bytes = [0u8; 16];
        getrandom::getrandom(&mut salt_bytes).expect("getrandom failed");
        let salt = URL_SAFE_NO_PAD.encode(salt_bytes);
        let hash = hash_with_salt(&salt_bytes, token);
        Self {
            salt,
            hash,
            created_at: Utc::now(),
            created_by: "bootstrap-rotation".into(),
        }
    }

    pub fn verify(&self, token: &str) -> bool {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let Ok(salt_bytes) = URL_SAFE_NO_PAD.decode(&self.salt) else {
            return false;
        };
        let expected = hash_with_salt(&salt_bytes, token);
        constant_time_eq::constant_time_eq(expected.as_bytes(), self.hash.as_bytes())
    }
}

fn hash_with_salt(salt: &[u8], token: &str) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_verifies_a_token() {
        let record = AdminTokenRecord::from_plaintext("my-strong-token-1234567890-abcde");
        assert!(record.verify("my-strong-token-1234567890-abcde"));
        assert!(!record.verify("wrong-token"));
    }

    #[test]
    fn salts_are_unique_per_record() {
        let a = AdminTokenRecord::from_plaintext("same-token-value-xyz-1234567890");
        let b = AdminTokenRecord::from_plaintext("same-token-value-xyz-1234567890");
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.hash, b.hash);
    }
}
