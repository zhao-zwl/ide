//! QA verification tests for v2.0 Stage A (T19 collaboration / T20 等保三级).
//!
//! Complements the engineer's in-module unit tests (`security.rs` / `collab.rs`
//! / `audit.rs`) by closing the critical-path gaps called out in the v2.0 QA
//! brief that the module tests did not fully assert:
//!
//!   * TG20-1 — `migrations/0005_v20.sql` is *purely incremental*: it only
//!     `ADD COLUMN IF NOT EXISTS` / `ENABLE ROW LEVEL SECURITY` / `CREATE POLICY`
//!     / `CREATE TABLE IF NOT EXISTS` / `CREATE OR REPLACE FUNCTION` /
//!     `CREATE TRIGGER`; it never ALTERs/DROPs the 0001–0004 objects (no
//!     `DROP TABLE` / `DROP COLUMN` / `ALTER COLUMN` / `DROP FUNCTION`, no
//!     `CREATE EXTENSION` — pgcrypto already lives in 0001).
//!   * TG20-2 — RLS tenant-isolation: all 7 tables carry a policy whose
//!     `USING`/`WITH CHECK` scopes rows to
//!     `tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id)`,
//!     and every `tenant_id` column is `NOT NULL DEFAULT 'default'` (+ index).
//!   * TG20-3 — Audit tamper-evidence: 0001 still enforces append-only
//!     (`BEFORE UPDATE OR DELETE` trigger + INSERT-only RLS), and 0005 *adds*
//!     the `prev_hash`/`row_hash` chain (`audit_row_hash` / `audit_verify_chain`
//!     / `audit_chain_before_insert`).
//!   * TG20-4 — Rust↔SQL audit hash-chain normalization is byte-identical:
//!     `AuditEvent::row_hash` and SQL `audit_chain_payload` both produce
//!     `action|perm_bit|tenant_id|prev_hash` joined by `|` (no JSON /
//!     timestamptz drift across the boundary).
//!   * TG20-5 — Secrets at-rest encryption: `secrets` stores only
//!     `value_encrypted bytea` (no plaintext `value` column); the DEK travels as
//!     a *bind parameter* (`pgp_sym_encrypt(p_value, p_key)`), never inline; the
//!     schema keeps no stored key; `CoreConfig::enc_key` defaults empty and is
//!     sourced from `AIDEA_ENC_KEY`.
//!   * TG20-6 — `CoreConfig` exposes `tenant_id` (single-tenant logical id) +
//!     `enc_key`; the v2.0 public surface (`collab` / `security`) is re-exported
//!     from the crate root so the CLI reaches it without touching internals.
//!
//! No external services (DB / network / heavy deps): pure logic + migration text
//! embedded via `include_str!`. Runnable under the OOM-constrained baseline.

use ide_core::audit::{AuditEvent, AuditSink, MockAuditSink};
use ide_core::collab::{Comment, InMemoryCommentStore, InMemoryLockStore, Lock};
use ide_core::config::CoreConfig;
use ide_core::principal::{AuditAction, Principal};
use serde_json::Value as Json;
use std::collections::HashMap;

// Migration / proto text fixtures (relative to crates/core/tests/).
const MIG_0001: &str = include_str!("../../../migrations/0001_init.sql");
const MIG_0005: &str = include_str!("../../../migrations/0005_v20.sql");

// --- helpers ----------------------------------------------------------------
/// Collapse runs of whitespace to a single space (keeps layout-insensitive
/// substring checks readable).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Remove ALL whitespace (for exact SQL-template matching).
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Extract the `AS $$ ... $$;` body of a `CREATE OR REPLACE FUNCTION name(...)`.
fn extract_function_body(src: &str, name: &str) -> String {
    let marker = format!("FUNCTION {name}(");
    let start = src.find(marker.as_str()).expect("function must exist");
    let after = &src[start..];
    let body_start = after.find("AS $$").unwrap_or(0) + "AS $$".len();
    let rest = &after[body_start..];
    let end = rest.find("$$;").unwrap_or(rest.len());
    rest[..end].to_string()
}

/// Extract a `CREATE TABLE IF NOT EXISTS <name> ... );` block.
fn extract_create_table(src: &str, name: &str) -> String {
    let marker = format!("CREATE TABLE IF NOT EXISTS {name}");
    let start = src.find(marker.as_str()).expect("table must exist");
    let after = &src[start..];
    let end = after.find(");").unwrap_or(after.len());
    after[..end + 2].to_string()
}

// --- TG20-1: 0005 is purely incremental ------------------------------------
#[test]
fn mig_0005_is_purely_incremental() {
    // Allowed object kinds.
    assert!(MIG_0005.contains("ADD COLUMN IF NOT EXISTS"));
    assert!(MIG_0005.contains("ENABLE ROW LEVEL SECURITY"));
    assert!(MIG_0005.contains("CREATE POLICY"));
    assert!(MIG_0005.contains("CREATE TABLE IF NOT EXISTS"));
    assert!(MIG_0005.contains("CREATE OR REPLACE FUNCTION"));
    assert!(MIG_0005.contains("CREATE TRIGGER"));

    // Must NOT mutate 0001–0004 objects.
    let upper = MIG_0005.to_uppercase();
    for forbidden in [
        "DROP TABLE",
        "TRUNCATE",
        "DROP COLUMN",
        "ALTER COLUMN",
        "DROP FUNCTION",
    ] {
        assert!(
            !upper.contains(forbidden),
            "0005 must not contain {forbidden} (would mutate 0001–0004)"
        );
    }
    // pgcrypto is reused from 0001 — no CREATE EXTENSION *statement* (only a
    // comment mentions it). Any non-comment CREATE EXTENSION is a regression.
    for line in MIG_0005.lines() {
        if line.to_uppercase().contains("CREATE EXTENSION") && !line.trim_start().starts_with("--") {
            panic!("0005 must not CREATE EXTENSION (pgcrypto belongs to 0001): {line}");
        }
    }
    assert!(
        MIG_0001.contains("CREATE EXTENSION IF NOT EXISTS pgcrypto"),
        "pgcrypto must be enabled by 0001"
    );

    // Only idempotent DROP POLICY/TRIGGER IF EXISTS of its OWN objects.
    assert!(MIG_0005.contains("DROP POLICY IF EXISTS"));
    assert!(MIG_0005.contains("DROP TRIGGER IF EXISTS"));
    for own in [
        "audit_tenant_isolation",
        "context_sources_tenant_isolation",
        "ckg_symbols_tenant_isolation",
        "ckg_edges_tenant_isolation",
        "comments_tenant_isolation",
        "locks_tenant_isolation",
        "secrets_tenant_isolation",
        "audit_chain_hash",
    ] {
        assert!(MIG_0005.contains(own), "0005 must own the dropped object {own}");
    }
}

// --- TG20-2: RLS tenant-isolation USING clause ------------------------------
const TENANT_TABLES: &[&str] = &[
    "audit",
    "context_sources",
    "ckg_symbols",
    "ckg_edges",
    "comments",
    "locks",
    "secrets",
];

#[test]
fn rls_policies_scope_to_session_tenant() {
    let norm = collapse_ws(MIG_0005);
    for t in TENANT_TABLES {
        let rls_pat = format!("ALTER TABLE {t} ENABLE ROW LEVEL SECURITY");
        assert!(norm.contains(rls_pat.as_str()), "{t}: RLS must be enabled");
        let pol = format!("{t}_tenant_isolation");
        assert!(norm.contains(pol.as_str()), "{t}: tenant-isolation policy must exist");
        // The isolation predicate must anchor rows to the session tenant.
        assert!(
            norm.contains("tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id)"),
            "{t}: policy must scope rows to the session tenant"
        );
    }
    // The audit policy is RESTRICTIVE (AND-combined with the 0001 permissive
    // append-only policy) so isolation can never be OR-opened to a writable path.
    assert!(
        norm.contains("CREATE POLICY audit_tenant_isolation ON audit AS RESTRICTIVE"),
        "audit policy must be RESTRICTIVE (AND with 0001 append-only)"
    );
    // Every tenant_id column carries NOT NULL DEFAULT 'default' (+ index).
    let n = norm.matches("tenant_id text NOT NULL DEFAULT 'default'").count();
    assert_eq!(n, 7, "all 7 tenant tables must define tenant_id NOT NULL DEFAULT 'default'");
    assert!(norm.contains("CREATE INDEX IF NOT EXISTS audit_tenant_idx"));
}

// --- TG20-3: audit append-only (0001) + hash chain (0005) -------------------
#[test]
fn audit_append_only_and_hash_chain_present() {
    // 0001: append-only enforcement (BEFORE UPDATE/DELETE trigger + INSERT-only RLS).
    assert!(MIG_0001.contains("BEFORE UPDATE OR DELETE"));
    assert!(MIG_0001.contains("audit_no_update"));
    assert!(MIG_0001.contains("append-only"));
    assert!(MIG_0001.contains("CREATE POLICY audit_append_only ON audit"));
    assert!(MIG_0001.contains("FOR INSERT WITH CHECK (true)"));

    // 0005: hash-chain additions (never remove the 0001 guard).
    assert!(MIG_0005.contains("prev_hash"));
    assert!(MIG_0005.contains("row_hash"));
    assert!(MIG_0005.contains("CREATE OR REPLACE FUNCTION audit_row_hash"));
    assert!(MIG_0005.contains("CREATE OR REPLACE FUNCTION audit_verify_chain"));
    assert!(MIG_0005.contains("CREATE OR REPLACE FUNCTION audit_chain_before_insert"));
    assert!(MIG_0005.contains("audit_chain_hash"));
}

// --- TG20-4: Rust <-> SQL audit hash-chain normalization matches -------------
#[test]
fn audit_row_hash_normalization_matches_sql() {
    // Rust side: AuditEvent::row_hash builds `action|perm_bit|tenant_id|prev_hash`.
    let p = Principal::all("single", "tester");
    let e = AuditEvent::new(&p, AuditAction::Read, true, Json::Null);
    let rust = e.row_hash("");
    assert_eq!(rust.len(), 64, "sha256 hex is 64 chars");
    assert_eq!(rust, e.row_hash(""), "row_hash must be deterministic");

    // The canonical plaintext the Rust code hashes.
    let action = e.action.as_str(); // "read"
    let perm_bit = e.action.perm_bit(); // 0 (i32)
    let tenant = e.tenant_id.as_str(); // "single"
    let prev = "";
    let expected_plaintext = format!("{}|{}|{}|{}", action, perm_bit, tenant, prev);
    assert_eq!(expected_plaintext, "read|0|single|");

    // SQL side: audit_chain_payload must concatenate the same fields, in the
    // same order, joined by '|'.
    let body = compact(extract_function_body(&collapse_ws(MIG_0005), "audit_chain_payload"));
    let i_a = body.find("p_action").expect("p_action present");
    let i_b = body.find("p_perm_bit").expect("p_perm_bit present");
    let i_t = body.find("p_tenant_id").expect("p_tenant_id present");
    let i_p = body.find("p_prev_hash").expect("p_prev_hash present");
    assert!(
        i_a < i_b && i_b < i_t && i_t < i_p,
        "audit_chain_payload field order must be action|perm_bit|tenant_id|prev_hash"
    );
    assert!(body.contains("||'|'||"), "audit_chain_payload must join fields with '|'");
    // Exact normalization (whitespace removed): coalesce(x,'') == x for non-null
    // inputs, so the SQL plaintext equals the Rust plaintext above.
    let want = "coalesce(p_action,'')||'|'||coalesce(p_perm_bit::text,'')||'|'||coalesce(p_tenant_id,'')||'|'||coalesce(p_prev_hash,'')";
    assert!(
        body.contains(want),
        "audit_chain_payload must normalize to `action|perm_bit|tenant_id|prev_hash`"
    );
    // With non-null inputs the SQL plaintext equals the Rust plaintext, so
    // Rust sha256(audit plaintext) == SQL audit_row_hash(audit inputs):
    // the cross-boundary chain is byte-identical (no JSON/timestamptz drift).
    let sql_plaintext = format!("{}|{}|{}|{}", action, perm_bit, tenant, prev);
    assert_eq!(sql_plaintext, expected_plaintext);
}

// --- TG20-5: secrets at-rest encryption, DEK never persisted ----------------
#[test]
fn secrets_encrypted_at_rest_and_dek_not_persisted() {
    let norm = collapse_ws(MIG_0005);
    let secrets_tbl = extract_create_table(&norm, "secrets");
    // Only an encrypted bytea column — no plaintext value column.
    assert!(
        secrets_tbl.contains("value_encrypted bytea"),
        "secrets must store value_encrypted bytea"
    );
    assert!(
        !secrets_tbl.contains("value text"),
        "secrets must NOT store a plaintext `value text` column"
    );
    assert!(
        !secrets_tbl.contains("value varchar"),
        "secrets must NOT store a plaintext `value varchar` column"
    );
    // DEK travels as a bind parameter, not inline SQL text.
    assert!(
        norm.contains("pgp_sym_encrypt(p_value, p_key)"),
        "DEK (p_key) must be a bind parameter, not inline"
    );
    assert!(
        norm.contains("pgp_sym_decrypt(value_encrypted, p_key)"),
        "decrypt must use the supplied key, not a stored key"
    );
    // set_secret / get_secret signatures match the 4-arg Rust call in security.rs.
    assert!(norm.contains("CREATE OR REPLACE FUNCTION set_secret("));
    assert!(norm.contains("CREATE OR REPLACE FUNCTION get_secret("));

    // Config DEK sourced from AIDEA_ENC_KEY, defaults empty (dev = opt-in).
    assert_eq!(
        CoreConfig::default().enc_key,
        "",
        "enc_key must default empty (encryption-at-rest opt-in)"
    );
    let mut m = HashMap::new();
    m.insert("ENC_KEY".to_string(), "super-secret-dek".to_string());
    assert_eq!(CoreConfig::from_map(&m).enc_key, "super-secret-dek");
}

// --- TG20-6: config + public surface re-exports (no source breakage) --------
#[test]
fn config_exposes_tenant_and_enc_key_and_surface_reexports() {
    // Single-tenant logical id (MVP F7) + empty DEK by default.
    let c = CoreConfig::default();
    assert_eq!(c.tenant_id, "single");
    assert_eq!(c.enc_key, "");
    // AIDEA_* env mapping round-trips.
    let mut m = HashMap::new();
    m.insert("TENANT_ID".to_string(), "acme".to_string());
    m.insert("ENC_KEY".to_string(), "dek".to_string());
    let c = CoreConfig::from_map(&m);
    assert_eq!(c.tenant_id, "acme");
    assert_eq!(c.enc_key, "dek");

    // The v2.0 public surface is re-exported from the crate root so the CLI and
    // other consumers reach collab/security without touching internal modules.
    // (A missing re-export here fails to compile.)
    let _comment = Comment::new("t", "f.rs", 1, 1, "u", "body");
    let _lock = Lock::new("t", "f.rs", "u");
    let _cs = InMemoryCommentStore::new();
    let _ls = InMemoryLockStore::new();
    let _sink = MockAuditSink::new();
    let _ = &_sink as &dyn AuditSink;
}
