//! Principal & six-right action model (T11 — runtime enforcement).
//!
//! A [`Principal`] is the security identity threading through the whole Core
//! call chain (agent → planner → tool_executor → host). It carries the
//! single-tenant `tenant_id`, a `user_id`, and the six-bit [`PermissionSet`]
//! (Read/Generate/Modify/Execute/Commit/Audit) introduced in `permissions.rs`.
//!
//! The six audited actions are modeled by [`AuditAction`]; the action boundary
//! (e.g. before a `modify` tool runs) calls [`check_permission`] and, on
//! failure, the caller rejects the action and records a denied audit entry.

use crate::permissions::{Permission, PermissionDenied, PermissionSet};
use std::fmt;

/// A security principal carried through the entire Core call chain.
///
/// Constructed once (from config in single-tenant mode, or per-request once SSO
/// lands in T12) and threaded into the Planner / ChatEngine / CraftEngine / host
/// calls so every privileged action can be authorized against it.
#[derive(Debug, Clone)]
pub struct Principal {
    /// Single-tenant tenant identifier (v1.0 keeps one logical tenant).
    pub tenant_id: String,
    /// Identity of the acting user/service.
    pub user_id: String,
    /// The six-bit capability mask governing what this principal may do.
    pub perms: PermissionSet,
}

impl Principal {
    /// Build a principal from its parts.
    pub fn new(
        tenant_id: impl Into<String>,
        user_id: impl Into<String>,
        perms: PermissionSet,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            user_id: user_id.into(),
            perms,
        }
    }

    /// Build a principal holding the full six-bit mask (all rights granted).
    pub fn all(tenant_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self::new(tenant_id, user_id, PermissionSet::all())
    }

    /// Build a principal from just a raw mask; ids use the given sentinels and
    /// the mask is clamped to the six-bit domain (0..63).
    pub fn from_mask(
        tenant_id: impl Into<String>,
        user_id: impl Into<String>,
        mask: u32,
    ) -> Self {
        Self::new(tenant_id, user_id, PermissionSet(mask & 0b111111))
    }

    /// Expose the raw six-bit mask (mirrors the SQL `perm_mask` column).
    pub fn perm_mask(&self) -> u32 {
        self.perms.mask() & 0b111111
    }

    /// Whether this principal may read the audit log (the `Audit` bit).
    pub fn can_read_audit(&self) -> bool {
        self.perms.has(Permission::Audit)
    }
}

/// The six audited capability actions. These map 1:1 to the six permission bits
/// (see `permissions::Permission`) and to the `perm_bit` column of the `audit`
/// table in `migrations/0001_init.sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    /// Read code / context / documents.
    Read,
    /// Generate content (chat, NES completion, drafts) without persisting.
    Generate,
    /// Modify files / apply edits.
    Modify,
    /// Execute commands / tools.
    Execute,
    /// Commit to VCS.
    Commit,
    /// Read/inspect the audit log itself.
    Audit,
}

impl AuditAction {
    /// The permission bit that *governs performing* this action.
    pub fn required_permission(self) -> Permission {
        match self {
            AuditAction::Read => Permission::Read,
            AuditAction::Generate => Permission::Generate,
            AuditAction::Modify => Permission::Modify,
            AuditAction::Execute => Permission::Execute,
            AuditAction::Commit => Permission::Commit,
            AuditAction::Audit => Permission::Audit,
        }
    }

    /// Stable string form used as the `audit.action` text column.
    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::Read => "read",
            AuditAction::Generate => "generate",
            AuditAction::Modify => "modify",
            AuditAction::Execute => "execute",
            AuditAction::Commit => "commit",
            AuditAction::Audit => "audit",
        }
    }

    /// The `perm_bit` integer stored in the `audit` table (0..5).
    pub fn perm_bit(self) -> i32 {
        self.required_permission().bit() as i32
    }

    /// Map a permission bit back to its audit action (inverse of
    /// [`AuditAction::required_permission`]).
    pub fn from_permission(p: Permission) -> AuditAction {
        match p {
            Permission::Read => AuditAction::Read,
            Permission::Generate => AuditAction::Generate,
            Permission::Modify => AuditAction::Modify,
            Permission::Execute => AuditAction::Execute,
            Permission::Commit => AuditAction::Commit,
            Permission::Audit => AuditAction::Audit,
        }
    }
}

impl fmt::Display for AuditAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Map a free-form tool/action name to the six-bit action it performs.
///
/// Returns `None` for non-privileged actions (e.g. `inspect`, `finish`) that
/// need no capability check; the five privileged categories return their
/// governing [`Permission`]. Used at the action boundary in the Planner and the
/// host bridge so the right bit is enforced + audited.
pub fn action_permission(tool: &str) -> Option<Permission> {
    match tool.to_ascii_lowercase().as_str() {
        "read" | "read_document" | "readfile" | "read_file" => Some(Permission::Read),
        "generate" | "chat" | "complete" | "nes" | "ghost" | "draft" => Some(Permission::Generate),
        "modify" | "edit" | "write" | "apply" | "apply_edit" => Some(Permission::Modify),
        "execute" | "run" | "shell" | "command" | "exec" => Some(Permission::Execute),
        "commit" => Some(Permission::Commit),
        _ => None,
    }
}

/// Enforce that `principal` holds `required`, returning a clear error otherwise.
///
/// This is the single choke-point for runtime authorization (T11): every
/// privileged action calls it before executing, and on failure the caller
/// records a *denied* audit entry. It reuses the existing
/// [`crate::permissions::require`] authority so the Rust bit math stays
/// identical to the SQL `perm_has` helper in `0001_init.sql`.
pub fn check_permission(
    principal: &Principal,
    required: Permission,
) -> Result<(), PermissionDenied> {
    crate::permissions::require(principal.perms, required)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_mask_clamped_to_six_bits() {
        // 7 bits in -> 6 bits out (matches the SQL `perm_mask` domain 0..63).
        let p = Principal::from_mask("t", "u", 0b1111111);
        assert_eq!(p.perm_mask(), 0b111111);
    }

    #[test]
    fn audit_action_roundtrips_permission() {
        assert_eq!(
            AuditAction::from_permission(Permission::Modify).required_permission(),
            Permission::Modify
        );
        assert_eq!(AuditAction::Modify.perm_bit(), Permission::Modify.bit() as i32);
        assert_eq!(AuditAction::Audit.as_str(), "audit");
    }

    #[test]
    fn action_permission_maps_privileged_tools() {
        assert_eq!(action_permission("modify"), Some(Permission::Modify));
        assert_eq!(action_permission("commit"), Some(Permission::Commit));
        assert_eq!(action_permission("run"), Some(Permission::Execute));
        assert_eq!(action_permission("chat"), Some(Permission::Generate));
        assert_eq!(action_permission("read_document"), Some(Permission::Read));
        // Non-privileged actions need no capability check.
        assert_eq!(action_permission("inspect"), None);
        assert_eq!(action_permission("finish"), None);
    }

    #[test]
    fn check_permission_enforces_and_reports() {
        let full = Principal::all("t", "u");
        assert!(check_permission(&full, Permission::Commit).is_ok());

        let reader = Principal::new("t", "u", PermissionSet::empty().grant(Permission::Read));
        let err = check_permission(&reader, Permission::Modify).unwrap_err();
        assert!(err.to_string().contains("permission denied"));
        assert!(!reader.can_read_audit());
    }

    #[test]
    fn mask_matches_sql_perm_has_contract() {
        // The Rust mask must be byte-identical to the SQL `perm_mask` domain
        // (0..63). Validate the canonical "all six bits" value == 63.
        assert_eq!(PermissionSet::all().mask() & 0b111111, 63);
        // Each single bit equals 1<<bit, mirroring perm_has(mask, bit).
        for (i, p) in Permission::ALL.iter().enumerate() {
            let single = PermissionSet::empty().grant(*p);
            assert_eq!(single.mask(), 1u32 << i);
        }
    }
}
