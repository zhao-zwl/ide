-- ============================================================================
-- Agentic IDE — v0.5 Stage A incremental migration (T05 / T06 / T07).
-- File: migrations/0002_v05.sql
-- Scope (v0.5 Stage A): persist Craft proposals (T07) and a context-source
--         feed table (T05) on top of 0001_init.sql.
--
-- Run AFTER 0001_init.sql:
--   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0002_v05.sql
-- Idempotent: safe to re-run (IF NOT EXISTS / ON CONFLICT guards).
-- NOTE: this file only ADDS objects; it never alters 0001_init.sql.
-- ============================================================================

-- 1) Craft proposals (T07) -------------------------------------------------
--    Human-led editing: the Agent proposes an edit; it is applied only after
--    the user confirms. Persisting proposals gives an auditable plan record.
CREATE TABLE IF NOT EXISTS craft_proposals (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  session_id    uuid REFERENCES sessions(id),
  document_uri  text NOT NULL,
  old_text      text NOT NULL,
  new_text      text NOT NULL,
  rationale     text,
  -- Mirrors crate::craft::EditKind (FileEdit | RunCommand | Commit).
  kind          text NOT NULL CHECK (kind IN ('FileEdit','RunCommand','Commit')),
  -- Mirrors crate::craft::CraftState (Suggestion | PendingConfirm | Applied | Rejected).
  state         text NOT NULL CHECK (state IN ('Suggestion','PendingConfirm','Applied','Rejected')),
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS craft_proposals_session_idx
  ON craft_proposals (session_id);

-- 2) Context-source feed (T05) ---------------------------------------------
--    Multi-source context chunks (open file / selection / diagnostic / symbol
--    / recent edit) gathered by the ContextManager, mirroring the `embeddings`
--    table but tagged with a priority tier for token-budget tiered dropping.
CREATE TABLE IF NOT EXISTS context_sources (
  id            bigserial PRIMARY KEY,
  project_id    bigint NOT NULL REFERENCES projects(id),
  -- Mirrors crate::context_manager::ContextSource.
  source        text NOT NULL CHECK (source IN
                  ('OpenFile','Selection','Diagnostic','Symbol','RecentEdit')),
  uri           text,
  content       text NOT NULL,
  -- 0 Low, 1 Medium, 2 High (tiered drop order under token budget).
  priority      int NOT NULL DEFAULT 1,
  embedding     vector(1536),
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS context_sources_project_idx
  ON context_sources (project_id);

-- IVFFlat index for cosine similarity (mirrors embeddings_embedding_idx).
CREATE INDEX IF NOT EXISTS context_sources_embedding_idx
  ON context_sources USING ivfflat (embedding vector_cosine_ops)
  WITH (lists = 100);

-- 3) Audit (T03 append-only) ------------------------------------------------
--    The append-only `audit` table, its trigger and RLS policy are defined in
--    0001_init.sql and apply to any new rows. We only insert (never mutate), so
--    no schema change is required here. A helper to record a craft apply:
CREATE OR REPLACE FUNCTION log_craft_apply(
  p_actor_id bigint,
  p_action   text,
  p_perm_bit int,
  p_detail   jsonb
)
RETURNS void LANGUAGE sql AS $$
  INSERT INTO audit (actor_id, action, perm_bit, detail)
  VALUES (p_actor_id, p_action, p_perm_bit, p_detail);
$$;
