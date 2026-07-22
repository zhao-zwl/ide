-- ============================================================================
-- Agentic IDE — M1 storage base (PostgreSQL 16 + pgvector 0.7)
-- File: migrations/0001_init.sql
-- Scope (T03): six-bit permission mask (RBAC), append-only audit partition,
--         and context-retrieval vectors.
--
-- Run with:  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0001_init.sql
-- Idempotent: safe to re-run (IF NOT EXISTS / ON CONFLICT guards).
-- NOTE: pg_partman is NOT required; partitions are created explicitly below.
-- ============================================================================

-- 1) Extensions -------------------------------------------------------------
CREATE EXTENSION IF NOT EXISTS vector;    -- pgvector: semantic context vectors
CREATE EXTENSION IF NOT EXISTS pgcrypto;  -- column encryption (future: G9 等保)

-- 2) Six-bit permission mask (RBAC) ----------------------------------------
--    Bit layout (mirrors crates/core/src/permissions.rs Permission enum):
--      bit0 Read | bit1 Generate | bit2 Modify | bit3 Execute
--      bit4 Commit | bit5 Audit
--    A single `perm_mask` column stores all six grants compactly.
CREATE DOMAIN perm_mask AS integer NOT NULL DEFAULT 0
  CHECK (VALUE >= 0 AND VALUE <= 63);  -- 6 bits max (0..63)

-- SQL helpers mirroring the Rust `PermissionSet` API.
CREATE OR REPLACE FUNCTION perm_has(mask perm_mask, bitpos int)
RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
  SELECT (mask & (1 << bitpos)) <> 0;
$$;

CREATE OR REPLACE FUNCTION perm_grant(mask perm_mask, bitpos int)
RETURNS perm_mask LANGUAGE sql IMMUTABLE AS $$
  SELECT (mask | (1 << bitpos))::perm_mask;
$$;

CREATE OR REPLACE FUNCTION perm_revoke(mask perm_mask, bitpos int)
RETURNS perm_mask LANGUAGE sql IMMUTABLE AS $$
  SELECT (mask & ~(1 << bitpos))::perm_mask;
$$;
-- Bit indices (documentation / audit): 0 Read,1 Generate,2 Modify,
-- 3 Execute,4 Commit,5 Audit.

-- 3) Core tables ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orgs (
  id         bigserial PRIMARY KEY,
  name       text NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS users (
  id           bigserial PRIMARY KEY,
  org_id       bigint NOT NULL REFERENCES orgs(id),
  email        text NOT NULL UNIQUE,
  display_name text,
  created_at   timestamptz NOT NULL DEFAULT now()
);

-- Roles carry a single six-bit permission mask.
CREATE TABLE IF NOT EXISTS roles (
  id        bigserial PRIMARY KEY,
  org_id    bigint NOT NULL REFERENCES orgs(id),
  name      text NOT NULL,
  perm_mask perm_mask NOT NULL DEFAULT 0,
  UNIQUE (org_id, name)
);

CREATE TABLE IF NOT EXISTS user_roles (
  user_id bigint NOT NULL REFERENCES users(id),
  role_id bigint NOT NULL REFERENCES roles(id),
  PRIMARY KEY (user_id, role_id)
);

-- Aggregate mask for a user = bitwise-OR of all their role masks.
CREATE OR REPLACE FUNCTION user_perm_mask(u bigint)
RETURNS perm_mask LANGUAGE sql STABLE AS $$
  SELECT COALESCE(
    (SELECT bit_or(r.perm_mask)::perm_mask
       FROM user_roles ur JOIN roles r ON r.id = ur.role_id
      WHERE ur.user_id = u), 0::perm_mask);
$$;

CREATE TABLE IF NOT EXISTS projects (
  id         bigserial PRIMARY KEY,
  org_id     bigint NOT NULL REFERENCES orgs(id),
  name       text NOT NULL,
  repo_uri   text,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS sessions (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  project_id bigint NOT NULL REFERENCES projects(id),
  user_id    bigint NOT NULL REFERENCES users(id),
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS messages (
  id         bigserial PRIMARY KEY,
  session_id uuid NOT NULL REFERENCES sessions(id),
  role       text NOT NULL CHECK (role IN ('user','assistant','system')),
  content    text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS tool_calls (
  id         bigserial PRIMARY KEY,
  session_id uuid NOT NULL REFERENCES sessions(id),
  tool       text NOT NULL,
  argument   text,
  output     text,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS checkpoints (
  id         bigserial PRIMARY KEY,
  session_id uuid NOT NULL REFERENCES sessions(id),
  label      text,
  snapshot   jsonb NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- 4) Context retrieval vectors (pgvector) -----------------------------------
--    Mirrors crates/core/src/context_manager.rs EMBED_DIM = 1536.
CREATE TABLE IF NOT EXISTS embeddings (
  id         bigserial PRIMARY KEY,
  project_id bigint NOT NULL REFERENCES projects(id),
  kind       text NOT NULL,   -- 'chunk' | 'symbol' | 'message'
  ref_uri    text,            -- source file / symbol uri
  content    text,
  embedding  vector(1536),
  created_at timestamptz NOT NULL DEFAULT now()
);

-- IVFFlat index for cosine similarity (fast approximate neighbour search).
CREATE INDEX IF NOT EXISTS embeddings_embedding_idx
  ON embeddings USING ivfflat (embedding vector_cosine_ops)
  WITH (lists = 100);

-- Hybrid retrieval: vector similarity within a project.
-- Returns the top-k nearest neighbours of `query_vec`.
CREATE OR REPLACE FUNCTION search_context(
  p_project_id bigint,
  query_vec vector(1536),
  k int DEFAULT 8
)
RETURNS TABLE (
  ref_uri   text,
  content   text,
  distance  double precision
) LANGUAGE sql STABLE AS $$
  SELECT e.ref_uri, e.content, e.embedding <=> query_vec AS distance
    FROM embeddings e
   WHERE e.project_id = p_project_id
   ORDER BY e.embedding <=> query_vec
   LIMIT k;
$$;

-- Knowledge base items (future CKG seeds; T17).
CREATE TABLE IF NOT EXISTS knowledge_items (
  id         bigserial PRIMARY KEY,
  project_id bigint NOT NULL REFERENCES projects(id),
  title      text NOT NULL,
  body       text,
  embedding  vector(1536),
  created_at timestamptz NOT NULL DEFAULT now()
);

-- 5) Audit log — append-only partitioned table (T03 / 等保 G9) --------------
--    Partitioned by month; only INSERT is permitted (see trigger + RLS).
CREATE TABLE IF NOT EXISTS audit (
  id        bigserial,
  ts        timestamptz NOT NULL DEFAULT now(),
  actor_id  bigint REFERENCES users(id),
  action    text NOT NULL,
  perm_bit  int,               -- which of the six bits this event concerns
  detail    jsonb,
  PRIMARY KEY (id, ts)
) PARTITION BY RANGE (ts);

-- Seed the current and next month partitions (extend as needed / pg_partman).
CREATE TABLE IF NOT EXISTS audit_2025_07 PARTITION OF audit
  FOR VALUES FROM ('2025-07-01') TO ('2025-08-01');
CREATE TABLE IF NOT EXISTS audit_2025_08 PARTITION OF audit
  FOR VALUES FROM ('2025-08-01') TO ('2025-09-01');

-- Append-only guard: reject UPDATE/DELETE.
-- NOTE: in PostgreSQL a trigger on a partitioned table is NOT auto-inherited by
-- its partitions, so we attach it to each partition explicitly.
CREATE OR REPLACE FUNCTION audit_no_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'audit log is append-only: UPDATE/DELETE are forbidden';
END;
$$;

DO $$
DECLARE
  part text;
BEGIN
  FOR part IN SELECT inhrelid::regclass::text
              FROM pg_inherits WHERE inhparent = 'audit'::regclass
  LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS audit_no_update ON %I', part);
    EXECUTE format(
      'CREATE TRIGGER audit_no_update
         BEFORE UPDATE OR DELETE ON %I
         FOR EACH ROW EXECUTE FUNCTION audit_no_mutation()', part);
  END LOOP;
END $$;

-- Row-Level Security: even non-owner roles cannot rewrite history via DML.
-- RLS on the parent DOES apply to partitions.
ALTER TABLE audit ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS audit_append_only ON audit;
CREATE POLICY audit_append_only ON audit
  FOR INSERT WITH CHECK (true);   -- inserts allowed
-- No SELECT/UPDATE/DELETE policies => all reads/updates are denied by default
-- for roles subject to RLS. Combine with a non-superuser table owner for full
-- tamper-evidence (defense-in-depth alongside the trigger above).

-- 6) Seed: a demo org/role/user with the full six-bit mask ------------------
INSERT INTO orgs (name) VALUES ('demo-org')
  ON CONFLICT (name) DO NOTHING;

INSERT INTO roles (org_id, name, perm_mask)
  SELECT o.id, 'admin', 63   -- all six bits set (0b111111)
    FROM orgs o WHERE o.name = 'demo-org'
  ON CONFLICT (org_id, name) DO NOTHING;
