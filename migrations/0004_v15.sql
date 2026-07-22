-- ============================================================================
-- v1.5 Stage B (T17 CKG) — pure-incremental migration.
--
-- Does NOT alter 0001_init.sql / 0002_v05.sql / 0003_v10.sql. Adds two tables
-- to persist the Code Knowledge Graph built by `crate::ckg::PgCkgStore`:
--   * ckg_symbols — extracted symbol nodes (mod/fn/struct/impl/trait).
--   * ckg_edges   — relationships (Calls/Defines/Imports/Contains).
-- Idempotent: safe to re-apply (IF NOT EXISTS / no conflicting objects).
-- ============================================================================

CREATE TABLE IF NOT EXISTS ckg_symbols (
    id    BIGSERIAL PRIMARY KEY,
    kind  TEXT NOT NULL,          -- 'Mod' | 'Fn' | 'Struct' | 'Impl' | 'Trait'
    name  TEXT NOT NULL,
    file  TEXT NOT NULL,
    line  INTEGER NOT NULL,
    doc   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS ckg_edges (
    id         BIGSERIAL PRIMARY KEY,
    from_name  TEXT NOT NULL,      -- symbol (or file) the edge originates from
    to_name    TEXT NOT NULL,      -- symbol the edge points to
    kind       TEXT NOT NULL,      -- 'Calls' | 'Defines' | 'Imports' | 'Contains'
    file       TEXT NOT NULL,
    line       INTEGER NOT NULL
);

-- Indexes for neighborhood queries (by name / by file / by edge endpoint).
CREATE INDEX IF NOT EXISTS idx_ckg_symbols_name ON ckg_symbols (name);
CREATE INDEX IF NOT EXISTS idx_ckg_symbols_file ON ckg_symbols (file);
CREATE INDEX IF NOT EXISTS idx_ckg_edges_from  ON ckg_edges (from_name);
CREATE INDEX IF NOT EXISTS idx_ckg_edges_to    ON ckg_edges (to_name);
CREATE INDEX IF NOT EXISTS idx_ckg_edges_kind  ON ckg_edges (kind);
