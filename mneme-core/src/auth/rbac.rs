// rbac.rs — Role-based access control for MnemeCache.
//
// Three roles:
//   admin      — full access to all commands and all databases
//   readwrite  — read + write data commands on permitted databases
//   readonly   — read-only data commands on permitted databases
//
// Per-database restrictions:
//   allowed_dbs = [] (empty) → all databases are accessible
//   allowed_dbs = [0, 2]     → only databases 0 and 2 are accessible
//
// User management commands (UserCreate, UserDelete, etc.) are admin-only.
// The RBAC check is enforced in handle_connection before dispatch_command.

use mneme_common::CmdId;

/// Role assigned to a user account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    ReadWrite,
    ReadOnly,
}

impl Role {
    /// Parse role from wire string. Unknown values default to ReadWrite.
    pub fn from_str(s: &str) -> Self {
        match s {
            "admin"    => Self::Admin,
            "readonly" => Self::ReadOnly,
            _          => Self::ReadWrite,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin     => "admin",
            Self::ReadWrite => "readwrite",
            Self::ReadOnly  => "readonly",
        }
    }
}

/// Returns `true` if a connection with `role` and `allowed_dbs` may execute
/// `cmd` against `db_id`.
///
/// Rules:
/// 1. Admin bypasses all checks.
/// 2. If `allowed_dbs` is non-empty, `db_id` must appear in it.
/// 3. ReadOnly may only run read commands. ReadWrite may run read + write commands.
///    User-management commands require Admin (they are not in the read or write sets).
pub fn can_execute(role: Role, cmd: CmdId, db_id: u16, allowed_dbs: &[u16]) -> bool {
    if role == Role::Admin {
        return true;
    }

    // Database access check.
    if !allowed_dbs.is_empty() && !allowed_dbs.contains(&db_id) {
        return false;
    }

    match role {
        Role::Admin => true,
        Role::ReadOnly  => is_read_cmd(cmd),
        Role::ReadWrite => is_read_cmd(cmd) || is_write_cmd(cmd),
    }
}

/// Commands that read state but never mutate it.
fn is_read_cmd(cmd: CmdId) -> bool {
    matches!(cmd,
        // String / generic reads
        CmdId::Get | CmdId::Exists | CmdId::Ttl |
        // Hash reads
        CmdId::HGet | CmdId::HGetAll |
        // List reads
        CmdId::LRange |
        // ZSet reads
        CmdId::ZScore | CmdId::ZRank | CmdId::ZRange | CmdId::ZRangeByScore | CmdId::ZCard |
        // JSON reads
        CmdId::JsonGet | CmdId::JsonExists | CmdId::JsonType |
        // Observability (read-only view of server state)
        CmdId::SlowLog | CmdId::Metrics | CmdId::Stats | CmdId::MemoryUsage |
        CmdId::ClusterInfo | CmdId::ClusterSlots | CmdId::KeeperList | CmdId::PoolStats |
        // DB namespace (reads / connection management)
        CmdId::DbSize | CmdId::Select | CmdId::Scan | CmdId::Type | CmdId::MGet |
        // Auth
        CmdId::Auth | CmdId::RevokeToken |
        // Cluster
        CmdId::Wait |
        // User self-info (calling user only; admin check happens in handler)
        CmdId::UserInfo
    )
}

/// Commands that mutate data.
fn is_write_cmd(cmd: CmdId) -> bool {
    matches!(cmd,
        // String / generic writes
        CmdId::Set | CmdId::Del | CmdId::Expire |
        // Hash writes
        CmdId::HSet | CmdId::HDel |
        // List writes
        CmdId::LPush | CmdId::RPush | CmdId::LPop | CmdId::RPop |
        // ZSet writes
        CmdId::ZAdd | CmdId::ZRem |
        // Counters
        CmdId::Incr | CmdId::Decr | CmdId::IncrBy | CmdId::DecrBy |
        CmdId::IncrByFloat | CmdId::GetSet |
        // JSON writes
        CmdId::JsonSet | CmdId::JsonDel | CmdId::JsonArrAppend | CmdId::JsonNumIncrBy |
        // DB namespace writes
        CmdId::FlushDb | CmdId::MSet
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_always_allowed() {
        for cmd in [CmdId::Get, CmdId::Set, CmdId::UserCreate, CmdId::FlushDb] {
            assert!(can_execute(Role::Admin, cmd, 0, &[]));
            assert!(can_execute(Role::Admin, cmd, 5, &[0, 1]));
        }
    }

    #[test]
    fn readonly_blocks_writes() {
        assert!(can_execute(Role::ReadOnly, CmdId::Get, 0, &[]));
        assert!(!can_execute(Role::ReadOnly, CmdId::Set, 0, &[]));
        assert!(!can_execute(Role::ReadOnly, CmdId::Del, 0, &[]));
        assert!(!can_execute(Role::ReadOnly, CmdId::FlushDb, 0, &[]));
        assert!(!can_execute(Role::ReadOnly, CmdId::UserCreate, 0, &[]));
    }

    #[test]
    fn readwrite_allows_data_writes() {
        assert!(can_execute(Role::ReadWrite, CmdId::Set, 0, &[]));
        assert!(can_execute(Role::ReadWrite, CmdId::Del, 0, &[]));
        assert!(can_execute(Role::ReadWrite, CmdId::FlushDb, 0, &[]));
        // But not user management
        assert!(!can_execute(Role::ReadWrite, CmdId::UserCreate, 0, &[]));
        assert!(!can_execute(Role::ReadWrite, CmdId::UserDelete, 0, &[]));
    }

    #[test]
    fn db_restriction_enforced() {
        // allowed_dbs = [0] — can access db 0, not db 1
        assert!(can_execute(Role::ReadWrite, CmdId::Get, 0, &[0]));
        assert!(!can_execute(Role::ReadWrite, CmdId::Get, 1, &[0]));
        // admin ignores allowed_dbs
        assert!(can_execute(Role::Admin, CmdId::Get, 1, &[0]));
    }

    #[test]
    fn empty_allowed_dbs_means_all() {
        assert!(can_execute(Role::ReadWrite, CmdId::Get, 15, &[]));
        assert!(can_execute(Role::ReadOnly,  CmdId::Get, 15, &[]));
    }

    #[test]
    fn role_roundtrip() {
        for (s, r) in [("admin", Role::Admin), ("readwrite", Role::ReadWrite), ("readonly", Role::ReadOnly)] {
            assert_eq!(Role::from_str(s), r);
            assert_eq!(r.as_str(), s);
        }
        // Unknown → ReadWrite
        assert_eq!(Role::from_str("superuser"), Role::ReadWrite);
    }
}
