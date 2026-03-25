// Argus — Session token manager.
// HMAC-SHA256 tokens, TTL, in-memory blacklist.
// Delegates credential verification to UsersDb (PBKDF2-SHA256).

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::auth::users::UsersDb;

type HmacSha256 = Hmac<Sha256>;

/// Token claims embedded in the signed payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub user_id: u64,
    /// Absolute expiry — seconds since Unix epoch.
    pub exp: u64,
    /// Issued-at — seconds since Unix epoch.
    pub iat: u64,
    /// Random jti for uniqueness (also used as blacklist key).
    pub jti: u64,
    /// Role at token-issue time: "admin", "readwrite", "readonly".
    /// Defaults to "admin" so tokens issued before D-02 (which lack this field)
    /// continue to have full access — backward-compatible.
    #[serde(default = "default_admin_role")]
    pub role: String,
    /// Database IDs permitted at token-issue time.
    /// Empty = all databases allowed (same as no restriction).
    #[serde(default)]
    pub allowed_dbs: Vec<u16>,
}

fn default_admin_role() -> String { "admin".into() }

impl Claims {
    pub fn is_expired(&self) -> bool {
        now_secs() >= self.exp
    }
}

pub struct Argus {
    secret: Vec<u8>,
    /// Revoked token jtis. Production would TTL-prune this; here it is
    /// bounded by `max_blacklist` entries (oldest removed when full).
    blacklist: Mutex<HashSet<u64>>,
    max_blacklist: usize,
    /// Session token TTL in seconds (default 86 400 = 24 h).
    token_ttl_secs: u64,
    /// User credentials store (PBKDF2-SHA256).
    users: Arc<UsersDb>,
}

impl Argus {
    /// Create with a cluster secret (raw bytes or a passphrase).
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self::with_users_db(secret, UsersDb::in_memory())
    }

    pub fn with_users_db(secret: impl AsRef<[u8]>, db: UsersDb) -> Self {
        Self::with_config(secret, db, 86_400)
    }

    pub fn with_config(secret: impl AsRef<[u8]>, db: UsersDb, token_ttl_secs: u64) -> Self {
        Self {
            secret: secret.as_ref().to_vec(),
            blacklist: Mutex::new(HashSet::new()),
            max_blacklist: 65_536,
            token_ttl_secs,
            users: Arc::new(db),
        }
    }

    /// Verify username + password via PBKDF2, then issue a signed token.
    /// Returns the token string and the embedded Claims (role, allowed_dbs).
    /// Called when a client sends AUTH <username> <password>.
    pub fn auth_user(&self, username: &str, password: &str) -> Result<(String, Claims)> {
        let user_id = self.users.verify(username, password)?;
        let user = self.users.get_user_by_id(user_id)
            .ok_or_else(|| anyhow::anyhow!("user record missing after verify"))?;
        self.issue_with_claims(user_id, self.token_ttl_secs, &user.role, &user.allowed_dbs)
    }

    /// Create a user (admin operation).
    /// `role` is one of "admin", "readwrite", "readonly".
    /// The very first user created is always promoted to "admin".
    pub fn create_user(&self, username: &str, password: &str, role: &str) -> Result<u64> {
        self.users.create_user(username, password, role)
    }

    /// Delete a user. Prevents removal of the last admin.
    pub fn delete_user(&self, username: &str) -> Result<()> {
        self.users.delete_user(username)
    }

    /// Change a user's role.
    pub fn set_user_role(&self, username: &str, role: &str) -> Result<()> {
        self.users.set_role(username, role)
    }

    /// Grant a user access to a specific database.
    pub fn grant_db_access(&self, username: &str, db_id: u16) -> Result<()> {
        self.users.grant_db(username, db_id)
    }

    /// Revoke a user's access to a specific database.
    pub fn revoke_db_access(&self, username: &str, db_id: u16) -> Result<()> {
        self.users.revoke_db(username, db_id)
    }

    /// List all users as (username, role, allowed_dbs) tuples.
    /// Password hashes are never included.
    pub fn list_users(&self) -> Vec<(String, String, Vec<u16>)> {
        self.users.list_users().into_iter()
            .map(|r| (r.username, r.role, r.allowed_dbs))
            .collect()
    }

    /// Get user info by username.
    pub fn get_user(&self, username: &str) -> Option<(u64, String, Vec<u16>)> {
        self.users.get_user(username).map(|r| (r.user_id, r.role, r.allowed_dbs))
    }

    /// Get user info by user_id (for embedding in tokens after re-login).
    pub fn get_user_by_id(&self, user_id: u64) -> Option<(String, Vec<u16>)> {
        self.users.get_user_by_id(user_id).map(|r| (r.role, r.allowed_dbs))
    }

    /// Issue a signed token valid for `ttl_secs` seconds.
    /// Format: `<base64url(msgpack(claims))>.<base64url(hmac)>`
    /// Role defaults to "admin" and allowed_dbs to [] for backward compat.
    pub fn issue(&self, user_id: u64, ttl_secs: u64) -> Result<String> {
        self.issue_with_claims(user_id, ttl_secs, "admin", &[])
            .map(|(tok, _)| tok)
    }

    /// Issue a token and return both the token string and the Claims.
    pub fn issue_with_claims(
        &self,
        user_id: u64,
        ttl_secs: u64,
        role: &str,
        allowed_dbs: &[u16],
    ) -> Result<(String, Claims)> {
        let now = now_secs();
        let claims = Claims {
            user_id,
            iat: now,
            exp: now + ttl_secs,
            jti: random_u64(),
            role: role.to_string(),
            allowed_dbs: allowed_dbs.to_vec(),
        };

        let payload = rmp_serde::to_vec(&claims)?;
        let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

        let sig = self.sign(payload_b64.as_bytes())?;
        let sig_b64 = URL_SAFE_NO_PAD.encode(&sig);

        let token = format!("{payload_b64}.{sig_b64}");
        Ok((token, claims))
    }

    /// Verify token signature and TTL. Returns Claims on success.
    pub fn verify(&self, token: &str) -> Result<Claims> {
        let (payload_b64, sig_b64) = token
            .rsplit_once('.')
            .ok_or_else(|| anyhow::anyhow!("malformed token"))?;

        // Verify signature
        let expected_sig = self.sign(payload_b64.as_bytes())?;
        let provided_sig = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| anyhow::anyhow!("bad sig encoding"))?;

        if !constant_time_eq(&expected_sig, &provided_sig) {
            bail!("invalid token signature");
        }

        // Decode claims
        let payload = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| anyhow::anyhow!("bad payload encoding"))?;
        let claims: Claims = rmp_serde::from_slice(&payload)
            .map_err(|_| anyhow::anyhow!("bad claims"))?;

        if claims.is_expired() {
            bail!("token expired");
        }

        if self.blacklist.lock().contains(&claims.jti) {
            bail!("token revoked");
        }

        Ok(claims)
    }

    /// Revoke a token so that future `verify` calls return an error.
    pub fn revoke(&self, token: &str) -> Result<()> {
        let claims = self.verify(token)?;
        let mut bl = self.blacklist.lock();
        // Evict oldest entries when limit reached (simple: clear half)
        if bl.len() >= self.max_blacklist {
            let half: Vec<u64> = bl.iter().copied().take(bl.len() / 2).collect();
            for jti in half {
                bl.remove(&jti);
            }
        }
        bl.insert(claims.jti);
        Ok(())
    }

    /// Verify only the HMAC of the cluster secret (for internal node auth).
    /// Returns true if the provided tag matches HMAC-SHA256 of `message`.
    pub fn verify_cluster_tag(&self, message: &[u8], tag: &[u8]) -> bool {
        self.sign(message)
            .map(|expected| constant_time_eq(&expected, tag))
            .unwrap_or(false)
    }

    /// Compute HMAC-SHA256 tag over `data`.
    pub fn sign(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .map_err(|_| anyhow::anyhow!("HMAC key error"))?;
        mac.update(data);
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_u64() -> u64 {
    use rand::Rng;
    rand::thread_rng().gen()
}

/// Constant-time byte slice comparison to prevent timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argus() -> Argus {
        Argus::new(b"test-secret-key-1234567890abcdef")
    }

    #[test]
    fn issue_and_verify_ok() {
        let a = argus();
        let token = a.issue(42, 3600).unwrap();
        let claims = a.verify(&token).unwrap();
        assert_eq!(claims.user_id, 42);
    }

    #[test]
    fn expired_token_rejected() {
        let a = argus();
        // TTL of 0 → exp == iat, so already expired
        let token = a.issue(1, 0).unwrap();
        assert!(a.verify(&token).is_err());
    }

    #[test]
    fn wrong_secret_rejected() {
        let a1 = Argus::new(b"secret-one");
        let a2 = Argus::new(b"secret-two");
        let token = a1.issue(1, 3600).unwrap();
        assert!(a2.verify(&token).is_err());
    }

    #[test]
    fn revoked_token_rejected() {
        let a = argus();
        let token = a.issue(7, 3600).unwrap();
        a.revoke(&token).unwrap();
        assert!(a.verify(&token).is_err());
    }

    #[test]
    fn tampered_payload_rejected() {
        let a = argus();
        let token = a.issue(1, 3600).unwrap();
        // Replace a byte in the payload section (before the last '.')
        let dot = token.rfind('.').unwrap();
        let mut chars: Vec<char> = token.chars().collect();
        // Flip case of a character inside the payload
        if dot > 2 {
            chars[2] = if chars[2].is_ascii_uppercase() { 'a' } else { 'A' };
        }
        let tampered: String = chars.into_iter().collect();
        if tampered != token {
            assert!(a.verify(&tampered).is_err());
        }
    }

    #[test]
    fn cluster_tag_verify() {
        let a = argus();
        let msg = b"hello-node-2";
        let sig = a.sign(msg).unwrap();
        assert!(a.verify_cluster_tag(msg, &sig));
        assert!(!a.verify_cluster_tag(b"wrong", &sig));
    }

    #[test]
    fn revoke_does_not_affect_other_tokens() {
        let a = argus();
        let t1 = a.issue(1, 3600).unwrap();
        let t2 = a.issue(2, 3600).unwrap();
        a.revoke(&t1).unwrap();
        assert!(a.verify(&t1).is_err());
        assert!(a.verify(&t2).is_ok());
    }

    #[test]
    fn malformed_token_rejected() {
        let a = argus();
        assert!(a.verify("not-a-token").is_err());
        assert!(a.verify("").is_err());
        assert!(a.verify("a.b.c.d").is_err());
    }

    /// Helper: create an Argus backed by a UsersDb with a pre-created user.
    fn argus_with_user(username: &str, password: &str, role: &str) -> Argus {
        let db = UsersDb::in_memory();
        db.create_user(username, password, role).unwrap();
        Argus::with_users_db(b"test-secret-key-1234567890abcdef", db)
    }

    #[test]
    fn auth_valid_user() {
        let a = argus_with_user("alice", "hunter2", "admin");
        let (token, claims) = a.auth_user("alice", "hunter2").unwrap();
        assert!(!token.is_empty());
        assert_eq!(claims.user_id, 1);
        // Token should also be verifiable.
        let verified = a.verify(&token).unwrap();
        assert_eq!(verified.user_id, claims.user_id);
    }

    #[test]
    fn auth_invalid_password() {
        let a = argus_with_user("alice", "hunter2", "admin");
        let result = a.auth_user("alice", "wrong-password");
        assert!(result.is_err());
    }

    #[test]
    fn auth_nonexistent_user() {
        let a = argus_with_user("alice", "hunter2", "admin");
        let result = a.auth_user("nobody", "whatever");
        assert!(result.is_err());
    }

    #[test]
    fn validate_expired_token() {
        // Use a TTL of 0 so the token is expired at issuance.
        let db = UsersDb::in_memory();
        db.create_user("alice", "pw", "admin").unwrap();
        let a = Argus::with_config(b"test-secret", db, 0);
        let (token, _) = a.auth_user("alice", "pw").unwrap();
        let err = a.verify(&token).unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn validate_revoked_token() {
        let a = argus_with_user("alice", "pw", "admin");
        let (token, _) = a.auth_user("alice", "pw").unwrap();
        // Verify it works before revocation.
        assert!(a.verify(&token).is_ok());
        // Revoke the token.
        a.revoke(&token).unwrap();
        // Verify it is rejected with "revoked".
        let err = a.verify(&token).unwrap_err();
        assert!(err.to_string().contains("revoked"));
    }

    #[test]
    fn validate_tampered_token() {
        let a = argus_with_user("alice", "pw", "admin");
        let (token, _) = a.auth_user("alice", "pw").unwrap();
        // Tamper with a byte in the payload (before the dot).
        let mut bytes = token.into_bytes();
        // Flip a bit in the 3rd byte of the payload section.
        bytes[2] ^= 0x01;
        let tampered = String::from_utf8(bytes).unwrap();
        let err = a.verify(&tampered).unwrap_err();
        // Should fail with invalid signature or bad payload.
        let msg = err.to_string();
        assert!(msg.contains("invalid") || msg.contains("bad"));
    }

    #[test]
    fn issue_token_csprng_jti() {
        let a = argus_with_user("alice", "pw", "admin");
        let (_, claims1) = a.auth_user("alice", "pw").unwrap();
        let (_, claims2) = a.auth_user("alice", "pw").unwrap();
        assert_ne!(claims1.jti, claims2.jti, "JTIs must differ (CSPRNG)");
    }

    #[test]
    fn role_in_claims() {
        let a = argus_with_user("alice", "pw", "admin");
        let (_, claims) = a.auth_user("alice", "pw").unwrap();
        assert_eq!(claims.role, "admin");
    }

    #[test]
    fn allowed_dbs_in_claims() {
        let db = UsersDb::in_memory();
        db.create_user("alice", "pw", "admin").unwrap();
        db.grant_db("alice", 1).unwrap();
        db.grant_db("alice", 5).unwrap();
        db.grant_db("alice", 10).unwrap();
        let a = Argus::with_users_db(b"test-secret-key-1234567890abcdef", db);
        let (_, claims) = a.auth_user("alice", "pw").unwrap();
        assert_eq!(claims.allowed_dbs, vec![1, 5, 10]);
    }
}
