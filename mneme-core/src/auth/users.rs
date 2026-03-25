// Users database — PBKDF2-SHA256 password hashing, file persistence.
// Format: msgpack array of UserRecord, written atomically via rename.

use std::path::{Path, PathBuf};
use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use parking_lot::RwLock;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const ITERATIONS: u32 = 100_000;
const SALT_LEN: usize = 16;
const DK_LEN: usize = 32;

/// Single user record stored in users.db.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub user_id: u64,
    pub username: String,
    /// base64url(salt bytes)
    pub salt: String,
    /// base64url(PBKDF2-SHA256 derived key)
    pub hash: String,
    /// Role: "admin", "readwrite", "readonly".
    /// Default "readwrite". The first user ever created is always "admin".
    #[serde(default = "default_readwrite_role")]
    pub role: String,
    /// Database IDs this user may access. Empty = all databases allowed.
    #[serde(default)]
    pub allowed_dbs: Vec<u16>,
}

fn default_readwrite_role() -> String { "readwrite".into() }

/// In-memory users store backed by a msgpack file.
pub struct UsersDb {
    path: PathBuf,
    users: RwLock<HashMap<String, UserRecord>>,
}

impl UsersDb {
    /// Load from file, or create empty store if path doesn't exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let users = if path.exists() {
            let data = std::fs::read(&path)
                .with_context(|| format!("read users.db: {}", path.display()))?;
            let records: Vec<UserRecord> = rmp_serde::from_slice(&data)
                .with_context(|| "deserialize users.db")?;
            records.into_iter().map(|r| (r.username.clone(), r)).collect()
        } else {
            HashMap::new()
        };
        Ok(Self { path, users: RwLock::new(users) })
    }

    /// Create a new in-memory UsersDb (no backing file).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            users: RwLock::new(HashMap::new()),
        }
    }

    /// Add a new user with PBKDF2-hashed password and the given role.
    /// If no users exist yet the very first user is promoted to "admin"
    /// regardless of the `role` argument, ensuring there is always at least
    /// one admin account on a fresh install.
    /// Create a user. If the user already exists, update their password and role
    /// (upsert semantics). This makes repeated installs / reinstalls idempotent:
    /// calling `adduser --username admin --role admin` always leaves the admin
    /// account with the correct role regardless of its previous state.
    pub fn create_user(&self, username: &str, password: &str, role: &str) -> Result<u64> {
        let salt = random_salt();
        let dk = derive_key(password.as_bytes(), &salt);

        let mut g = self.users.write();

        // If the user already exists, update password and role in-place.
        if let Some(record) = g.get_mut(username) {
            let uid = record.user_id;
            // First user is always promoted to admin; otherwise respect the supplied role.
            record.role  = role.to_string();
            record.salt  = URL_SAFE_NO_PAD.encode(salt);
            record.hash  = URL_SAFE_NO_PAD.encode(dk);
            drop(g);
            self.flush()?;
            return Ok(uid);
        }

        let max_id = g.values().map(|r| r.user_id).max().unwrap_or(0);
        // Promote to admin if this is the very first user.
        let effective_role = if g.is_empty() { "admin" } else { role };
        let record = UserRecord {
            user_id: max_id + 1,
            username: username.to_string(),
            salt: URL_SAFE_NO_PAD.encode(salt),
            hash: URL_SAFE_NO_PAD.encode(dk),
            role: effective_role.to_string(),
            allowed_dbs: Vec::new(),
        };
        let uid = record.user_id;
        g.insert(username.to_string(), record);
        drop(g);
        self.flush()?;
        Ok(uid)
    }

    /// Delete a user by username. Returns an error if the user does not exist
    /// or if the last admin account would be removed (preventing lockout).
    pub fn delete_user(&self, username: &str) -> Result<()> {
        let mut g = self.users.write();
        let record = g.get(username)
            .ok_or_else(|| anyhow::anyhow!("unknown user '{}'", username))?
            .clone();
        // Prevent removing the last admin.
        if record.role == "admin" {
            let admin_count = g.values().filter(|r| r.role == "admin").count();
            if admin_count <= 1 {
                anyhow::bail!("cannot delete the last admin account");
            }
        }
        g.remove(username);
        drop(g);
        self.flush()
    }

    /// Change a user's role. Returns an error if the user does not exist,
    /// or if demoting the last admin.
    pub fn set_role(&self, username: &str, role: &str) -> Result<()> {
        let mut g = self.users.write();
        let record = g.get_mut(username)
            .ok_or_else(|| anyhow::anyhow!("unknown user '{}'", username))?;
        // Prevent demoting last admin.
        if record.role == "admin" && role != "admin" {
            let admin_count = g.values().filter(|r| r.role == "admin").count();
            if admin_count <= 1 {
                anyhow::bail!("cannot demote the last admin account");
            }
        }
        g.get_mut(username).unwrap().role = role.to_string();
        drop(g);
        self.flush()
    }

    /// Grant access to a specific database.
    /// If `allowed_dbs` was empty (= all), adding a db switches the user to
    /// explicit-allowlist mode: only the listed databases are accessible.
    pub fn grant_db(&self, username: &str, db_id: u16) -> Result<()> {
        let mut g = self.users.write();
        let record = g.get_mut(username)
            .ok_or_else(|| anyhow::anyhow!("unknown user '{}'", username))?;
        if !record.allowed_dbs.contains(&db_id) {
            record.allowed_dbs.push(db_id);
            record.allowed_dbs.sort_unstable();
        }
        drop(g);
        self.flush()
    }

    /// Revoke access to a specific database.
    pub fn revoke_db(&self, username: &str, db_id: u16) -> Result<()> {
        let mut g = self.users.write();
        let record = g.get_mut(username)
            .ok_or_else(|| anyhow::anyhow!("unknown user '{}'", username))?;
        record.allowed_dbs.retain(|&d| d != db_id);
        drop(g);
        self.flush()
    }

    /// Return all user records (without password hashes in the public API —
    /// callers should redact before sending to clients).
    pub fn list_users(&self) -> Vec<UserRecord> {
        let mut records: Vec<UserRecord> = self.users.read().values().cloned().collect();
        records.sort_by_key(|r| r.user_id);
        records
    }

    /// Look up a user record by numeric ID.
    pub fn get_user_by_id(&self, user_id: u64) -> Option<UserRecord> {
        self.users.read().values().find(|r| r.user_id == user_id).cloned()
    }

    /// Look up a user record by username.
    pub fn get_user(&self, username: &str) -> Option<UserRecord> {
        self.users.read().get(username).cloned()
    }

    /// Verify username + plaintext password. Returns user_id on success.
    pub fn verify(&self, username: &str, password: &str) -> Result<u64> {
        let g = self.users.read();
        let record = match g.get(username) {
            Some(r) => r.clone(),
            None => bail!("unknown user"),
        };
        drop(g);

        let salt = URL_SAFE_NO_PAD
            .decode(&record.salt)
            .map_err(|_| anyhow::anyhow!("corrupt salt"))?;
        let stored_dk = URL_SAFE_NO_PAD
            .decode(&record.hash)
            .map_err(|_| anyhow::anyhow!("corrupt hash"))?;

        let computed_dk = derive_key(password.as_bytes(), &salt);

        if !constant_time_eq(&computed_dk, &stored_dk) {
            bail!("invalid credentials");
        }
        Ok(record.user_id)
    }

    /// Persist users to disk atomically.
    fn flush(&self) -> Result<()> {
        if self.path == PathBuf::from("/dev/null") {
            return Ok(());
        }
        let records: Vec<UserRecord> = self.users.read().values().cloned().collect();
        let data = rmp_serde::to_vec(&records)?;
        let tmp = self.path.with_extension("db.tmp");
        std::fs::write(&tmp, &data)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename users.db"))?;
        Ok(())
    }
}

fn derive_key(password: &[u8], salt: &[u8]) -> Vec<u8> {
    let mut dk = vec![0u8; DK_LEN];
    pbkdf2_hmac::<Sha256>(password, salt, ITERATIONS, &mut dk);
    dk
}

fn random_salt() -> Vec<u8> {
    let mut salt = vec![0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_verify() {
        let db = UsersDb::in_memory();
        let uid = db.create_user("alice", "hunter2", "readwrite").unwrap();
        assert!(uid > 0);
        let uid2 = db.verify("alice", "hunter2").unwrap();
        assert_eq!(uid, uid2);
    }

    #[test]
    fn first_user_always_admin() {
        let db = UsersDb::in_memory();
        db.create_user("alice", "pw", "readwrite").unwrap();
        let rec = db.get_user("alice").unwrap();
        // First user is promoted to admin regardless of requested role.
        assert_eq!(rec.role, "admin");
    }

    #[test]
    fn second_user_keeps_requested_role() {
        let db = UsersDb::in_memory();
        db.create_user("alice", "pw", "admin").unwrap();
        db.create_user("bob", "pw2", "readonly").unwrap();
        assert_eq!(db.get_user("bob").unwrap().role, "readonly");
    }

    #[test]
    fn wrong_password_rejected() {
        let db = UsersDb::in_memory();
        db.create_user("bob", "correct", "readwrite").unwrap();
        assert!(db.verify("bob", "wrong").is_err());
    }

    #[test]
    fn unknown_user_rejected() {
        let db = UsersDb::in_memory();
        assert!(db.verify("nobody", "pass").is_err());
    }

    #[test]
    fn two_users_independent() {
        let db = UsersDb::in_memory();
        db.create_user("a", "pass_a", "readwrite").unwrap();
        db.create_user("b", "pass_b", "readonly").unwrap();
        assert!(db.verify("a", "pass_a").is_ok());
        assert!(db.verify("b", "pass_b").is_ok());
        assert!(db.verify("a", "pass_b").is_err());
    }

    #[test]
    fn delete_user_works() {
        let db = UsersDb::in_memory();
        db.create_user("admin", "pw", "admin").unwrap();
        db.create_user("bob", "pw2", "readwrite").unwrap();
        db.delete_user("bob").unwrap();
        assert!(db.get_user("bob").is_none());
    }

    #[test]
    fn cannot_delete_last_admin() {
        let db = UsersDb::in_memory();
        db.create_user("admin", "pw", "admin").unwrap();
        assert!(db.delete_user("admin").is_err());
    }

    #[test]
    fn grant_revoke_db() {
        let db = UsersDb::in_memory();
        db.create_user("alice", "pw", "readwrite").unwrap();
        db.grant_db("alice", 1).unwrap();
        db.grant_db("alice", 3).unwrap();
        let rec = db.get_user("alice").unwrap();
        assert_eq!(rec.allowed_dbs, vec![1, 3]);
        db.revoke_db("alice", 1).unwrap();
        let rec = db.get_user("alice").unwrap();
        assert_eq!(rec.allowed_dbs, vec![3]);
    }

    #[test]
    fn set_role_works() {
        let db = UsersDb::in_memory();
        db.create_user("admin", "pw", "admin").unwrap();
        db.create_user("bob", "pw2", "readwrite").unwrap();
        db.set_role("bob", "readonly").unwrap();
        assert_eq!(db.get_user("bob").unwrap().role, "readonly");
    }

    #[test]
    fn list_users_sorted_by_id() {
        let db = UsersDb::in_memory();
        db.create_user("a", "pw", "admin").unwrap();
        db.create_user("b", "pw", "readwrite").unwrap();
        db.create_user("c", "pw", "readonly").unwrap();
        let list = db.list_users();
        assert_eq!(list.len(), 3);
        assert!(list[0].user_id < list[1].user_id);
        assert!(list[1].user_id < list[2].user_id);
    }
}
