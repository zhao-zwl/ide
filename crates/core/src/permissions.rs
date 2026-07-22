//! Six-bit permission mask (T03 / RBAC).
//!
//! Permissions are modeled as a bitmask so a single `i32` column stores all
//! grants compactly and is cheap to check. The exact same encoding is mirrored
//! in `migrations/0001_init.sql` (`perm_mask` domain + SQL helpers), keeping
//! the Rust authority and the database constraint in lock-step.

/// The six capability bits of the Agentic IDE RBAC model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Permission {
    /// Read code, docs and context.
    Read = 1 << 0,
    /// Generate (e.g. NES completion, chat draft) without persisting.
    Generate = 1 << 1,
    /// Modify files / apply edits.
    Modify = 1 << 2,
    /// Execute commands / tools.
    Execute = 1 << 3,
    /// Commit to VCS.
    Commit = 1 << 4,
    /// Write to the audit log.
    Audit = 1 << 5,
}

impl Permission {
    /// All six permissions in canonical bit order.
    pub const ALL: [Permission; 6] = [
        Permission::Read,
        Permission::Generate,
        Permission::Modify,
        Permission::Execute,
        Permission::Commit,
        Permission::Audit,
    ];

    /// The raw bit value of this permission.
    pub fn bit(self) -> u8 {
        self as u8
    }

    /// Human-readable label (used by the enterprise console, T13).
    pub fn label(self) -> &'static str {
        match self {
            Permission::Read => "Read",
            Permission::Generate => "Generate",
            Permission::Modify => "Modify",
            Permission::Execute => "Execute",
            Permission::Commit => "Commit",
            Permission::Audit => "Audit",
        }
    }
}

/// A permission set stored as a `u32` bitmask (max 32 bits; we use 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PermissionSet(pub u32);

impl PermissionSet {
    /// Empty set (no grants).
    pub fn empty() -> Self {
        Self(0)
    }

    /// Full set (all six bits).
    pub fn all() -> Self {
        let mut mask = 0u32;
        for p in Permission::ALL {
            mask |= p.bit() as u32;
        }
        Self(mask)
    }

    /// Grant a permission (idempotent).
    pub fn grant(mut self, p: Permission) -> Self {
        self.0 |= p.bit() as u32;
        self
    }

    /// Revoke a permission (idempotent).
    pub fn revoke(mut self, p: Permission) -> Self {
        self.0 &= !(p.bit() as u32);
        self
    }

    /// Whether `p` is granted.
    pub fn has(self, p: Permission) -> bool {
        self.0 & (p.bit() as u32) != 0
    }

    /// Whether every permission in `other` is granted (subset check).
    pub fn contains(self, other: PermissionSet) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Raw mask value (mirrors the SQL `perm_mask` column).
    pub fn mask(self) -> u32 {
        self.0
    }

    /// Build from a raw mask, clamped to the six-bit domain (0..63).
    pub fn from_mask(mask: u32) -> Self {
        Self(mask & 0b111111)
    }

    /// Human-readable names of the granted permissions, in canonical bit order
    /// (used by the enterprise console's `perm_mask` rendering, T13).
    pub fn labels(self) -> Vec<&'static str> {
        Permission::ALL
            .iter()
            .filter(|p| self.has(**p))
            .map(|p| p.label())
            .collect()
    }
}

/// Error returned when a required permission is missing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("permission denied: required {required:?} but mask is {mask}")]
pub struct PermissionDenied {
    pub required: Permission,
    pub mask: u32,
}

/// Check that `set` grants `required`; otherwise fail fast.
///
/// Use at the boundary of every privileged tool (Commit, Modify, Audit, …) so
/// the six-bit authority is enforced in the Core, not just in SQL.
pub fn require(set: PermissionSet, required: Permission) -> Result<(), PermissionDenied> {
    if set.has(required) {
        Ok(())
    } else {
        Err(PermissionDenied {
            required,
            mask: set.mask(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_and_check() {
        let set = PermissionSet::empty()
            .grant(Permission::Read)
            .grant(Permission::Modify);
        assert!(set.has(Permission::Read));
        assert!(set.has(Permission::Modify));
        assert!(!set.has(Permission::Commit));
    }

    #[test]
    fn revoke_clears_bit() {
        let set = PermissionSet::all().revoke(Permission::Audit);
        assert!(!set.has(Permission::Audit));
        assert!(set.has(Permission::Read));
    }

    #[test]
    fn require_ok_and_err() {
        let set = PermissionSet::empty().grant(Permission::Read);
        assert!(require(set, Permission::Read).is_ok());
        assert!(require(set, Permission::Execute).is_err());
    }

    #[test]
    fn contains_subsumes() {
        let full = PermissionSet::all();
        let sub = PermissionSet::empty()
            .grant(Permission::Read)
            .grant(Permission::Generate);
        assert!(full.contains(sub));
        assert!(!sub.contains(full));
    }

    #[test]
    fn all_has_six_bits() {
        let all = PermissionSet::all();
        assert_eq!(all.mask().count_ones(), 6);
        for p in Permission::ALL {
            assert!(all.has(p));
        }
    }
}
