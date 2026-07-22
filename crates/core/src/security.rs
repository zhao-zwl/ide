//! Secrets & static encryption helpers (T20 — 等保三级 数据保密性).
//!
//! Wraps the `pgcrypto` server-side functions defined in
//! `migrations/0005_v20.sql` (`set_secret` / `get_secret` / `pgp_sym_encrypt`
//! / `pgp_sym_decrypt`) and exposes a pure Rust key-holder path so the
//! data-encryption key (DEK) sourced from `AIDEA_ENC_KEY` (see
//! [`crate::config::CoreConfig::enc_key`]) is passed straight to Postgres over
//! the trusted (private-network) connection and never persisted in plaintext.
//!
//! This is **code-level** encryption hardening only. A real 等保三级 certification
//! must be performed by an authorized assessor (see `docs/compliance-mapping.md`).

use tokio_postgres::NoTls;

/// SQL used to scope a transaction to a tenant (mirrors
/// [`crate::collab::SET_TENANT_LOCAL_SQL`]).
pub const SET_TENANT_LOCAL_SQL: &str = "SET LOCAL app.tenant_id = $1";

/// Postgres-backed secret store (T20). Each operation runs in a tenant-scoped
/// transaction and calls the `pgcrypto` `set_secret` / `get_secret` functions.
pub struct PgSecretStore {
    client: tokio::sync::Mutex<tokio_postgres::Client>,
    #[allow(dead_code)]
    default_tenant: String,
}

impl PgSecretStore {
    /// Connect to Postgres and pin the session tenant (see 0005 RLS rationale).
    pub async fn connect(database_url: &str, tenant_id: &str) -> anyhow::Result<Self> {
        let (client, conn) = tokio_postgres::connect(database_url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("secret pg connection closed: {e}");
            }
        });
        let _ = client.execute("SET app.tenant_id = $1", &[&tenant_id]).await;
        Ok(Self {
            client: tokio::sync::Mutex::new(client),
            default_tenant: tenant_id.to_string(),
        })
    }

    /// Encrypt-at-rest upsert of a named secret for `tenant`. `key` is the DEK
    /// from `AIDEA_ENC_KEY`; it is sent to Postgres (which performs the
    /// `pgp_sym_encrypt`) and is never stored in the DB in plaintext.
    pub async fn set(
        &self,
        tenant: &str,
        name: &str,
        value: &str,
        key: &str,
    ) -> anyhow::Result<()> {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await?;
        tx.execute(SET_TENANT_LOCAL_SQL, &[&tenant]).await?;
        tx.execute("SELECT set_secret($1, $2, $3, $4)", &[&tenant, &name, &value, &key])
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Read (decrypt) a named secret for `tenant`. Returns `None` if absent.
    pub async fn get(
        &self,
        tenant: &str,
        name: &str,
        key: &str,
    ) -> anyhow::Result<Option<String>> {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await?;
        tx.execute(SET_TENANT_LOCAL_SQL, &[&tenant]).await?;
        let row = tx
            .query_opt("SELECT get_secret($1, $2, $3)", &[&tenant, &name, &key])
            .await?;
        let out = row.and_then(|r| r.try_get::<_, Option<String>>(0).ok().flatten());
        tx.commit().await?;
        Ok(out)
    }
}

/// This module's contribution to the T20 "SQL self-consistency" tests: assert
/// that the 0005 migration defines the pgcrypto secret functions. We read the
/// migration file (relative to the crate root) and check for the expected SQL
/// symbols — no live Postgres required.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Read `migrations/0005_v20.sql` (relative to this crate) as a string.
    fn migration_sql() -> String {
        let base = env!("CARGO_MANIFEST_DIR");
        let path = Path::new(base).join("../../migrations/0005_v20.sql");
        std::fs::read_to_string(path).expect("0005_v20.sql should be readable in tests")
    }

    #[test]
    fn migration_defines_pgcrypto_secret_functions() {
        let sql = migration_sql();
        assert!(sql.contains("pgp_sym_encrypt"), "missing pgp_sym_encrypt");
        assert!(sql.contains("pgp_sym_decrypt"), "missing pgp_sym_decrypt");
        assert!(sql.contains("CREATE OR REPLACE FUNCTION set_secret"), "missing set_secret");
        assert!(sql.contains("CREATE OR REPLACE FUNCTION get_secret"), "missing get_secret");
    }

    #[test]
    fn migration_enables_rls_on_tenant_tables() {
        let sql = migration_sql();
        for t in ["audit", "context_sources", "ckg_symbols", "ckg_edges", "comments", "locks", "secrets"] {
            assert!(
                sql.contains(&format!("ALTER TABLE {t} ENABLE ROW LEVEL SECURITY")),
                "RLS not enabled on {t}"
            );
        }
    }

    #[test]
    fn migration_defines_audit_tamper_chain() {
        let sql = migration_sql();
        assert!(sql.contains("CREATE OR REPLACE FUNCTION audit_verify_chain"), "missing audit_verify_chain");
        assert!(sql.contains("CREATE OR REPLACE FUNCTION audit_row_hash"), "missing audit_row_hash");
        assert!(sql.contains("CREATE OR REPLACE FUNCTION audit_chain_before_insert"), "missing trigger fn");
        assert!(sql.contains("prev_hash"), "missing prev_hash column");
        assert!(sql.contains("row_hash"), "missing row_hash column");
    }

    #[test]
    fn secret_store_session_injection_constant_exists() {
        assert!(SET_TENANT_LOCAL_SQL.contains("SET LOCAL app.tenant_id"));
    }
}
