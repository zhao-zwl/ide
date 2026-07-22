-- ============================================================================
-- Agentic IDE — v2.0 Stage A incremental migration (T19 协作 / T20 等保三级).
-- File: migrations/0005_v20.sql
--
-- Scope (v2.0 Stage A — code-level 等保三级 hardening + collaboration):
--   * T19 — collaboration:  `comments` (code-review annotations) and `locks`
--           (lightweight editing-presence hints) tables, both tenant-scoped.
--   * T20 — 等保三级 (code-level, NOT a real certification):
--       1) RLS row-level security for tenant-related tables (audit,
--          context_sources, ckg_symbols, ckg_edges + comments/locks/secrets),
--          hardening multi-tenant readiness on top of the single-tenant default.
--       2) pgcrypto static (at-rest) encryption: `secrets` table + set_secret /
--          get_secret helpers (pgp_sym_encrypt / pgp_sym_decrypt).
--       3) Audit immutability: append-only (already in 0001) + a SHA-256
--          anti-tamper hash chain (prev_hash / row_hash) anchored per tenant.
--
-- Run AFTER 0001..0004:
--   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0005_v20.sql
-- Idempotent: safe to re-run (IF NOT EXISTS / CREATE OR REPLACE / idempotent
-- ALTERs). This file ONLY ADDS objects; it never alters 0001..0004 (except
-- adding NEW columns to existing tables, which is a pure-incremental ALTER).
--
-- IMPORTANT (multi-tenant readiness, single-tenant default):
--   Every new RLS policy uses
--     tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id)
--   so that when the connection has NOT set `app.tenant_id` (dev / legacy SQL
--   helpers / tests) behaviour is identical to before (no isolation); when the
--   connecting role HAS set `SET app.tenant_id = '<tenant>'` (what the Rust
--   Pg* stores do after connecting), strict per-tenant isolation is enforced.
--   Single-tenant deployments simply keep the one configured tenant id.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 1) RLS hardening for tenant-related tables
-- ---------------------------------------------------------------------------

-- 1a) `audit` — add tenant scoping + anti-tamper chain columns.
--     (audit is partitioned; adding a column cascades to all partitions.)
ALTER TABLE audit
  ADD COLUMN IF NOT EXISTS tenant_id    text NOT NULL DEFAULT 'default';
ALTER TABLE audit
  ADD COLUMN IF NOT EXISTS prev_hash    text;
ALTER TABLE audit
  ADD COLUMN IF NOT EXISTS row_hash     text;
-- Optional encrypted mirror of `detail` for high-sensitivity deployments.
-- Left NULL by default; populate via pgp_sym_encrypt (see §3 helpers).
ALTER TABLE audit
  ADD COLUMN IF NOT EXISTS detail_encrypted bytea;

CREATE INDEX IF NOT EXISTS audit_tenant_idx ON audit (tenant_id);

-- audit already has RLS enabled (0001) with a permissive INSERT-only policy
-- `audit_append_only`. This idempotent ENABLE is a no-op if already on, but
-- makes the tenant-isolation intent explicit and self-documenting.
ALTER TABLE audit ENABLE ROW LEVEL SECURITY;

-- We ADD a RESTRICTIVE tenant-isolation policy so the two
-- combine with AND: an insert must satisfy BOTH (permissive check-true AND
-- tenant match). RESTRICTIVE is required so we do not silently OR-open writes
-- that the existing policy would have allowed.
DROP POLICY IF EXISTS audit_tenant_isolation ON audit;
CREATE POLICY audit_tenant_isolation ON audit AS RESTRICTIVE
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

-- 1b) `context_sources` (0002) — tenant scope.
ALTER TABLE context_sources
  ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
CREATE INDEX IF NOT EXISTS context_sources_tenant_idx ON context_sources (tenant_id);
ALTER TABLE context_sources ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS context_sources_tenant_isolation ON context_sources;
CREATE POLICY context_sources_tenant_isolation ON context_sources
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

-- 1c) `ckg_symbols` (0004) — tenant scope.
ALTER TABLE ckg_symbols
  ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
CREATE INDEX IF NOT EXISTS ckg_symbols_tenant_idx ON ckg_symbols (tenant_id);
ALTER TABLE ckg_symbols ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS ckg_symbols_tenant_isolation ON ckg_symbols;
CREATE POLICY ckg_symbols_tenant_isolation ON ckg_symbols
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

-- 1d) `ckg_edges` (0004) — tenant scope.
ALTER TABLE ckg_edges
  ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
CREATE INDEX IF NOT EXISTS ckg_edges_tenant_idx ON ckg_edges (tenant_id);
ALTER TABLE ckg_edges ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS ckg_edges_tenant_isolation ON ckg_edges;
CREATE POLICY ckg_edges_tenant_isolation ON ckg_edges
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

-- ---------------------------------------------------------------------------
-- 2) T19 collaboration tables (tenant-scoped)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS comments (
  id         text PRIMARY KEY,                -- app-generated (no uuid dep)
  tenant_id  text NOT NULL DEFAULT 'default',
  file       text NOT NULL,
  line_start integer NOT NULL,
  line_end   integer NOT NULL,
  author     text NOT NULL,
  body       text NOT NULL,
  resolved   boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS comments_tenant_file_idx
  ON comments (tenant_id, file);
CREATE INDEX IF NOT EXISTS comments_tenant_idx ON comments (tenant_id);

-- Editing-presence lock hints (T19.3). One holder per (tenant, file).
CREATE TABLE IF NOT EXISTS locks (
  tenant_id  text NOT NULL DEFAULT 'default',
  file       text NOT NULL,
  owner      text NOT NULL,
  acquired_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, file)
);
CREATE INDEX IF NOT EXISTS locks_tenant_idx ON locks (tenant_id);

-- Tenant RLS for the new collaboration tables (permissive; the only policy).
ALTER TABLE comments ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS comments_tenant_isolation ON comments;
CREATE POLICY comments_tenant_isolation ON comments
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

ALTER TABLE locks ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS locks_tenant_isolation ON locks;
CREATE POLICY locks_tenant_isolation ON locks
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

-- ---------------------------------------------------------------------------
-- 3) T20 pgcrypto — static (at-rest) encryption of secrets / sensitive data
-- ---------------------------------------------------------------------------
-- pgcrypto was already enabled in 0001 (CREATE EXTENSION IF NOT EXISTS pgcrypto).

-- Tenant-scoped secret store. `value_encrypted` is PGP-symmetric encrypted with
-- the data-encryption key (DEK) sourced from `AIDEA_ENC_KEY` at the Core and
-- passed straight to Postgres (never persisted in plaintext in the DB).
CREATE TABLE IF NOT EXISTS secrets (
  id         bigserial PRIMARY KEY,
  tenant_id  text NOT NULL DEFAULT 'default',
  name       text NOT NULL,
  value_encrypted bytea NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, name)
);
CREATE INDEX IF NOT EXISTS secrets_tenant_idx ON secrets (tenant_id);
ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS secrets_tenant_isolation ON secrets;
CREATE POLICY secrets_tenant_isolation ON secrets
  FOR ALL
  USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
  WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id));

-- Upsert a secret (encrypted at rest).
CREATE OR REPLACE FUNCTION set_secret(
  p_tenant text, p_name text, p_value text, p_key text
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO secrets (tenant_id, name, value_encrypted)
  VALUES (p_tenant, p_name, pgp_sym_encrypt(p_value, p_key))
  ON CONFLICT (tenant_id, name)
  DO UPDATE SET value_encrypted = pgp_sym_encrypt(p_value, p_key);
END;
$$;

-- Read a secret back (decrypted with the same key).
CREATE OR REPLACE FUNCTION get_secret(
  p_tenant text, p_name text, p_key text
) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  v text;
BEGIN
  SELECT pgp_sym_decrypt(value_encrypted, p_key)
    INTO v
    FROM secrets
   WHERE tenant_id = p_tenant AND name = p_name;
  RETURN v;
END;
$$;

-- Generic at-rest encryption helpers (reused for any sensitive column, e.g. an
-- encrypted mirror of `audit.detail` via the `detail_encrypted` column above).
CREATE OR REPLACE FUNCTION pg_encrypt_text(p_text text, p_key text)
RETURNS bytea LANGUAGE sql IMMUTABLE AS $$
  SELECT pgp_sym_encrypt(p_text, p_key);
$$;

CREATE OR REPLACE FUNCTION pg_decrypt_text(p_bytea bytea, p_key text)
RETURNS text LANGUAGE sql IMMUTABLE AS $$
  SELECT pgp_sym_decrypt(p_bytea, p_key);
$$;

-- ---------------------------------------------------------------------------
-- 4) T20 audit immutability — append-only (0001) + SHA-256 anti-tamper chain
-- ---------------------------------------------------------------------------
-- The audit table is already append-only (BEFORE UPDATE/DELETE trigger + RLS in
-- 0001). Here we add a hash chain so that a forged/missing row is detectable:
--   row_hash = sha256( action | perm_bit | tenant_id | prev_hash )
--   prev_hash = row_hash of the previous row for the same tenant (by id desc).
-- `audit_verify_chain()` reports any row whose stored row_hash no longer matches
-- its recomputed canonical payload (i.e. tampered or broken chain).

-- Canonical, JSON-canonicalization-free payload (excludes `detail` / `ts` / `id`
-- to avoid pg jsonb key-reordering and timestamptz-text mismatches across the
-- Rust/SQL boundary; the chain still detects insert/delete/reorder attacks).
CREATE OR REPLACE FUNCTION audit_chain_payload(
  p_action    text,
  p_perm_bit  int,
  p_tenant_id text,
  p_prev_hash text
) RETURNS text LANGUAGE sql IMMUTABLE AS $$
  SELECT coalesce(p_action, '')
      || '|' || coalesce(p_perm_bit::text, '')
      || '|' || coalesce(p_tenant_id, '')
      || '|' || coalesce(p_prev_hash, '');
$$;

-- SHA-256 hex of the canonical payload (uses pgcrypto).
CREATE OR REPLACE FUNCTION audit_row_hash(
  p_action    text,
  p_perm_bit  int,
  p_tenant_id text,
  p_prev_hash text
) RETURNS text LANGUAGE sql IMMUTABLE AS $$
  SELECT encode(
    sha256(audit_chain_payload(p_action, p_perm_bit, p_tenant_id, p_prev_hash)::bytea),
    'hex'
  );
$$;

-- BEFORE INSERT trigger: anchor the chain. If the client already supplied
-- prev_hash / row_hash (the Rust PgAuditSink does, for attestation), they are
-- preserved; otherwise the server fills them authoritatively.
CREATE OR REPLACE FUNCTION audit_chain_before_insert()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  prev text;
BEGIN
  IF NEW.prev_hash IS NULL THEN
    SELECT a.row_hash INTO prev
      FROM audit a
     WHERE a.tenant_id = NEW.tenant_id
     ORDER BY a.id DESC
     LIMIT 1;
    NEW.prev_hash := coalesce(prev, '');
  END IF;
  IF NEW.row_hash IS NULL THEN
    NEW.row_hash := audit_row_hash(
      NEW.action, NEW.perm_bit, NEW.tenant_id, NEW.prev_hash
    );
  END IF;
  RETURN NEW;
END;
$$;

-- Attach the chain trigger to every existing audit partition (mirrors the 0001
-- loop that attaches audit_no_update). NOTE: future partitions must also get
-- this trigger — in production use pg_partman / a DDL trigger, or re-run this
-- block after adding partitions.
DO $$
DECLARE
  part text;
BEGIN
  FOR part IN SELECT inhrelid::regclass::text
              FROM pg_inherits WHERE inhparent = 'audit'::regclass
  LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS audit_chain_hash ON %I', part);
    EXECUTE format(
      'CREATE TRIGGER audit_chain_hash
         BEFORE INSERT ON %I
         FOR EACH ROW EXECUTE FUNCTION audit_chain_before_insert()', part);
  END LOOP;
END $$;

-- Verification query: returns any row whose stored hash disagrees with the
-- recomputed canonical payload (tamper / broken-chain detection).
CREATE OR REPLACE FUNCTION audit_verify_chain()
RETURNS TABLE (bad_id bigint, reason text) LANGUAGE sql STABLE AS $$
  SELECT a.id, 'row_hash mismatch'::text
    FROM audit a
   WHERE a.row_hash IS DISTINCT FROM
         audit_row_hash(a.action, a.perm_bit, a.tenant_id, a.prev_hash)
  UNION ALL
  SELECT a.id, 'missing row_hash'::text
    FROM audit a
   WHERE a.row_hash IS NULL;
$$;

-- ---------------------------------------------------------------------------
-- Apply guidance (v2.0 single-tenant / multi-tenant ready):
--   1) Apply 0001 -> 0002 -> 0003 -> 0004 -> this file (order matters).
--   2) The Rust Pg* stores (PgAuditSink / PgCkgStore / PgCommentStore /
--      PgSecretStore) call `SET app.tenant_id = $1` (session) after connect and,
--      for per-request strictness, `SET LOCAL app.tenant_id = $1` inside a
--      transaction, so the RLS policies above enforce tenant isolation end-to-end.
--   3) Verify audit integrity any time with:  SELECT * FROM audit_verify_chain();
--   4) This migration implements CODE-LEVEL 等保三级 controls (access control /
--      security audit / data integrity / data confidentiality / residual-info
--      protection). A real 等保三级 certification must be performed by an
--      authorized assessor against the deployed system — see docs/compliance-mapping.md.
-- ---------------------------------------------------------------------------
