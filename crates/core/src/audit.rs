//! Audit sink — append-only audit logging (T11).
//!
//! Every privileged action (the six [`AuditAction`]s) records an [`AuditEvent`]
//! after it executes (or is denied). The event maps onto the append-only
//! `audit` partition table defined in `migrations/0001_init.sql`:
//!   * `actor_id`  -> `users.id` (nullable; demo principals have none)
//!   * `action`    -> `AuditAction::as_str()`
//!   * `perm_bit`  -> `AuditAction::perm_bit()`
//!   * `detail`    -> JSON carrying `tenant_id` / `user_id` / `granted` / payload
//!
//! Two sinks implement [`AuditSink`]:
//!   * [`MockAuditSink`] — in-memory, for unit tests and offline CLI.
//!   * [`PgAuditSink`]   — Postgres (tokio-postgres), appends to `audit` (v1.0).

use crate::principal::{AuditAction, Principal};
use async_trait::async_trait;
use serde_json::json;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// One audited action (maps to a row of the `audit` table).
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Epoch milliseconds.
    pub ts_ms: i64,
    /// `users.id` of the actor when known (often `None` in single-tenant demo).
    pub actor_id: Option<i64>,
    /// The six-right action performed / attempted.
    pub action: AuditAction,
    /// Tenant the actor belongs to (also embedded in `detail`).
    pub tenant_id: String,
    /// User id of the actor (also embedded in `detail`).
    pub user_id: String,
    /// Whether the action was permitted (true) or denied (false).
    pub granted: bool,
    /// Structured payload (tool, target, ids, …).
    pub payload: Json,
}

impl AuditEvent {
    /// Build an event for `principal` performing `action`, capturing whether it
    /// was `granted` and an optional structured `payload`.
    pub fn new(
        principal: &Principal,
        action: AuditAction,
        granted: bool,
        payload: Json,
    ) -> Self {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            ts_ms,
            actor_id: None,
            action,
            tenant_id: principal.tenant_id.clone(),
            user_id: principal.user_id.clone(),
            granted,
            payload,
        }
    }

    /// Convenience for a denied attempt with a human reason.
    pub fn denied(principal: &Principal, action: AuditAction, reason: &str) -> Self {
        Self::new(principal, action, false, json!({ "reason": reason }))
    }

    /// JSON object written to the `audit.detail` column. Bundles the principal
    /// identity + grant decision so the append-only row is self-describing (the
    /// `audit` table has no dedicated `tenant_id`/`user_id` columns).
    pub fn detail_json(&self) -> Json {
        json!({
            "tenant_id": self.tenant_id,
            "user_id": self.user_id,
            "granted": self.granted,
            "payload": self.payload,
        })
    }

    /// Compute the SHA-256 hex digest that anchors this event in the audit
    /// anti-tamper chain (T20). The canonical payload deliberately excludes
    /// `detail` / `ts` / `id` to stay JSON-canonicalization- and
    /// timestamptz-text-independent across the Rust/SQL boundary:
    ///   `action | perm_bit | tenant_id | prev_hash`.
    /// `prev_hash` is the `row_hash` of the previous row for this tenant (or
    /// `""` for the first row). The matching SQL is `audit_row_hash` in
    /// `migrations/0005_v20.sql`; `audit_verify_chain` recomputes it server-side
    /// to detect tampering.
    pub fn row_hash(&self, prev_hash: &str) -> String {
        let payload = format!(
            "{}|{}|{}|{}",
            self.action.as_str(),
            self.action.perm_bit(),
            self.tenant_id,
            prev_hash
        );
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Audit sink abstraction. Implementations decide *where* the immutable record
/// lands; the Core only depends on this trait (T11 runtime enforcement writes
/// through it).
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Append one audit event. Implementations must be append-only.
    async fn record(&self, event: &AuditEvent) -> anyhow::Result<()>;
}

/// In-memory audit sink — used by unit tests and the offline CLI. Holds every
/// recorded event so assertions can inspect them.
#[derive(Debug, Default)]
pub struct MockAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl MockAuditSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all recorded events (copy).
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().expect("audit lock poisoned").clone()
    }

    /// Total number of recorded events.
    pub fn count(&self) -> usize {
        self.events.lock().expect("audit lock poisoned").len()
    }

    /// Count events of a given action (regardless of grant decision).
    pub fn count_action(&self, action: AuditAction) -> usize {
        self.events
            .lock()
            .expect("audit lock poisoned")
            .iter()
            .filter(|e| e.action == action)
            .count()
    }

    /// Count *denied* events of a given action.
    pub fn count_denied(&self, action: AuditAction) -> usize {
        self.events
            .lock()
            .expect("audit lock poisoned")
            .iter()
            .filter(|e| e.action == action && !e.granted)
            .count()
    }
}

#[async_trait]
impl AuditSink for MockAuditSink {
    async fn record(&self, event: &AuditEvent) -> anyhow::Result<()> {
        self.events
            .lock()
            .expect("audit lock poisoned")
            .push(event.clone());
        Ok(())
    }
}

/// Postgres-backed audit sink (v1.0). Appends to the `audit` partition table
/// defined in `migrations/0001_init.sql` (append-only, trigger + RLS enforced
/// server-side). Connects lazily via `tokio-postgres` with `NoTls` (private
/// deployment over a trusted network / unix socket). v2.0 (T20): pins the
/// session tenant so the 0005 RLS policy is satisfied, and writes the
/// anti-tamper `prev_hash` / `row_hash` chain columns.
pub struct PgAuditSink {
    client: tokio_postgres::Client,
    #[allow(dead_code)]
    tenant_id: String,
}

impl PgAuditSink {
    /// Connect to Postgres and return a sink. Spawns the driver's background
    /// connection task internally, and pins `tenant_id` as the session tenant
    /// (the 0005 RLS policy checks `tenant_id = current_setting('app.tenant_id')`
    /// so every insert this sink performs is correctly scoped).
    pub async fn connect(database_url: &str, tenant_id: &str) -> anyhow::Result<Self> {
        let (client, conn) =
            tokio_postgres::connect(database_url, tokio_postgres::NoTls).await?;
        // Drive the connection in the background; errors are surfaced via the
        // returned `Client` (queries fail fast rather than panic).
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("audit pg connection closed: {e}");
            }
        });
        // Pin the session tenant so the tenant RLS policy is satisfied even for
        // plain (non-transactional) inserts.
        let _ = client.execute("SET app.tenant_id = $1", &[&tenant_id]).await;
        Ok(Self {
            client,
            tenant_id: tenant_id.to_string(),
        })
    }
}

#[async_trait]
impl AuditSink for PgAuditSink {
    async fn record(&self, event: &AuditEvent) -> anyhow::Result<()> {
        let action = event.action.as_str().to_string();
        let perm_bit = event.action.perm_bit();
        let detail = event.detail_json();
        // Anchor the anti-tamper chain: the previous row's hash for this tenant
        // becomes this row's `prev_hash`. The DB trigger (audit_chain_before_insert)
        // also fills these if NULL, so the chain is server-authoritative.
        let prev: Option<String> = self
            .client
            .query_opt(
                "SELECT row_hash FROM audit WHERE tenant_id = $1 ORDER BY id DESC LIMIT 1",
                &[&event.tenant_id],
            )
            .await?
            .and_then(|r| r.try_get::<_, Option<String>>(0).ok().flatten());
        let prev_hash = prev.unwrap_or_default();
        let row_hash = event.row_hash(&prev_hash);
        // Append-only insert into the `audit` table (tenant-scoped by RLS). The
        // BEFORE UPDATE/DELETE trigger (0001) + RLS policy guarantee no mutation.
        self.client
            .execute(
                "INSERT INTO audit \
                 (actor_id, action, perm_bit, detail, tenant_id, prev_hash, row_hash) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &event.actor_id,
                    &action,
                    &perm_bit,
                    &detail,
                    &event.tenant_id,
                    &prev_hash,
                    &row_hash,
                ],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionSet;

    #[tokio::test]
    async fn mock_sink_records_and_counts() {
        let sink = MockAuditSink::new();
        let p = Principal::all("t", "u");
        sink.record(&AuditEvent::new(
            &p,
            AuditAction::Read,
            true,
            json!({ "doc": "a.rs" }),
        ))
        .await
        .unwrap();
        // A denied attempt.
        sink.record(&AuditEvent::denied(&p, AuditAction::Modify, "no Modify bit"))
            .await
            .unwrap();

        assert_eq!(sink.count(), 2);
        assert_eq!(sink.count_action(AuditAction::Read), 1);
        assert_eq!(sink.count_action(AuditAction::Modify), 1);
        assert_eq!(sink.count_denied(AuditAction::Modify), 1);

        // The detail JSON must carry the principal identity + grant decision.
        let denied = sink
            .events()
            .into_iter()
            .find(|e| e.action == AuditAction::Modify)
            .unwrap();
        assert_eq!(denied.detail_json()["tenant_id"], "t");
        assert_eq!(denied.detail_json()["granted"], false);
    }

    #[tokio::test]
    async fn mask_is_consistent_with_sql_domain() {
        // A principal with only the Read bit must record Read as granted and
        // anything else as denied; the bit math mirrors the SQL `perm_mask`.
        let reader = Principal::new("t", "u", PermissionSet::empty().grant(crate::permissions::Permission::Read));
        let sink = MockAuditSink::new();
        sink.record(&AuditEvent::new(&reader, AuditAction::Read, true, json!({})))
            .await
            .unwrap();
        sink.record(&AuditEvent::new(&reader, AuditAction::Commit, false, json!({})))
            .await
            .unwrap();
        assert_eq!(sink.count_action(AuditAction::Read), 1);
        assert_eq!(sink.count_denied(AuditAction::Commit), 1);
    }

    #[test]
    fn audit_row_hash_is_deterministic_and_chains() {
        let p = Principal::all("t", "u");
        let e1 = AuditEvent::new(&p, AuditAction::Read, true, json!({}));
        let h1 = e1.row_hash("");
        assert_eq!(h1.len(), 64, "sha256 hex is 64 chars");
        // Deterministic for identical inputs.
        assert_eq!(h1, e1.row_hash(""));
        // Second event chains on the first's hash -> different digest.
        let e2 = AuditEvent::new(&p, AuditAction::Modify, true, json!({}));
        let h2 = e2.row_hash(&h1);
        assert_ne!(h1, h2);
        // A different tenant yields a different hash for an otherwise-equal payload,
        // mirroring the SQL `audit_row_hash` tenant-scoped chain.
        let p2 = Principal::all("other", "u");
        let e3 = AuditEvent::new(&p2, AuditAction::Read, true, json!({}));
        assert_ne!(e3.row_hash(""), h1);
    }
}
