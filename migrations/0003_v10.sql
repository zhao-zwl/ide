-- ============================================================================
-- Agentic IDE — v1.0 Stage A incremental migration (T11 / T14).
-- File: migrations/0003_v10.sql
-- Scope (v1.0 Stage A):
--   * T11 — runtime six-bit enforcement already lives in Core
--          (permissions / principal / audit modules). This migration adds the
--          append-only SQL helper the Core PgAuditSink calls to land an audit
--          row in the 0001 `audit` partition table, stamping the principal
--          identity (tenant_id / user_id) into the `detail` JSONB.
--   * T14 — observability is served over a separate admin TCP port (Prometheus
--          /metrics + /healthz); no DB objects are required for it here.
--
-- Run AFTER 0001_init.sql AND 0002_v05.sql:
--   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0003_v10.sql
-- Idempotent: safe to re-run (CREATE OR REPLACE). This file ONLY ADDS objects;
-- it never alters 0001/0002.
-- ============================================================================

-- Append-only audit insert used by the Core PgAuditSink (T11). The `audit`
-- table (0001) already enforces append-only via its BEFORE UPDATE/DELETE trigger
-- and RLS policy, so this function only ever INSERTs. The principal identity is
-- written into the `detail` JSONB so each row is self-describing (the `audit`
-- table has no dedicated `tenant_id` / `user_id` columns).
CREATE OR REPLACE FUNCTION log_audit(
  p_actor_id  bigint,
  p_action    text,
  p_perm_bit  int,
  p_tenant_id text,
  p_user_id   text,
  p_granted   boolean,
  p_payload   jsonb
)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit (actor_id, action, perm_bit, detail)
  VALUES (
    p_actor_id,
    p_action,
    p_perm_bit,
    jsonb_build_object(
      'tenant_id', p_tenant_id,
      'user_id',   p_user_id,
      'granted',   p_granted,
      'payload',   p_payload
    )
  );
END;
$$;

-- Convenience wrapper matching the Core AuditEvent.detail_json() shape exactly
-- (T11). Lets callers (or SQL-side tooling) land a fully-formed audit row in one
-- call. The Core PgAuditSink issues its own direct INSERT mirroring this JSONB
-- assembly, so this is purely an ergonomic / symmetry helper.
CREATE OR REPLACE FUNCTION log_audit_event(
  p_actor_id bigint,
  p_action   text,
  p_perm_bit int,
  p_detail   jsonb
)
RETURNS void LANGUAGE sql AS $$
  SELECT log_audit(
    p_actor_id,
    p_action,
    p_perm_bit,
    p_detail->>'tenant_id',
    p_detail->>'user_id',
    (p_detail->>'granted')::boolean,
    (p_detail->'payload')::jsonb
  );
$$;

-- ---------------------------------------------------------------------------
-- Apply guidance (v1.0 single-tenant):
--   1) Apply 0001 then 0002 then this file (order matters for the `audit` table).
--   2) The Core PgAuditSink connects with a role that satisfies the 0001 RLS
--      `audit_append_only` policy (INSERT allowed; no SELECT/UPDATE/DELETE) so
--      the append-only guarantee holds end-to-end.
-- ---------------------------------------------------------------------------
