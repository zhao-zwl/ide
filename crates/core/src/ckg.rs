//! CKG — Code Knowledge Graph (T17, F5 P1).
//!
//! A *lightweight* code knowledge graph used to enrich LLM context **beyond
//! pure-text similarity**. Given a referenced symbol, the CKG can pull in the
//! functions that call it / it calls, the `impl` block that contains it, the
//! trait it implements, etc. — structured neighborhood that vector retrieval
//! alone cannot recover.
//!
//! Design constraints (per the v1.5 Stage B brief):
//!   * **No heavy parsing deps** — parsing is done with manual string scanning
//!     (no `tree-sitter` / `syn` / `regex` crates). It targets `.rs` sources and
//!     is trivially extensible to other languages by adding a parser branch.
//!   * **No external graph library** — the graph is an in-memory `Vec` of
//!     [`Symbol`] + [`Edge`] (kept small and deterministic), queried linearly.
//!   * **Optional Postgres persistence** — [`PgCkgStore`] mirrors the graph in
//!     memory (so `query` stays sync + fast) and best-effort persists it to the
//!     `0004_v15.sql` tables. If the DB is unreachable it degrades to an
//!     in-memory-only store (query still works off the mirror).
//!   * **Pure-logic testable** — parsing, edge construction, and neighborhood
//!     queries have no I/O and run under `cargo test` with zero external deps.
//!
//! The [`CkgIndex`] trait is the single abstraction point: [`InMemoryCkg`] is the
//! default backend; [`PgCkgStore`] is a drop-in alternative. It is consumed by
//! `context_manager::ContextManager::enrich_from_ckg` and
//! `retrieval::retrieve_with_ckg` (T17 integration) without touching the
//! v1.5A governance chain.

use std::sync::{Arc, Mutex};
use tokio_postgres::NoTls;

/// Kind of a code symbol extracted from source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// `mod` declaration.
    Mod,
    /// `fn` declaration.
    Fn,
    /// `struct` declaration.
    Struct,
    /// `impl` block (name stored as `impl <Type>`).
    Impl,
    /// `trait` declaration.
    Trait,
}

impl SymbolKind {
    /// Stable string form (mirrors the `ckg_symbols.kind` CHECK domain).
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Mod => "Mod",
            SymbolKind::Fn => "Fn",
            SymbolKind::Struct => "Struct",
            SymbolKind::Impl => "Impl",
            SymbolKind::Trait => "Trait",
        }
    }
}

/// A code symbol node in the knowledge graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    pub file: String,
    pub line: usize,
    /// Preceding `///` doc comment (if any).
    pub doc: String,
}

/// Kind of a relationship edge between two symbols / a symbol and a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// `from` calls `to` (e.g. function invokes another function).
    Calls,
    /// `from` defines / implements `to` (e.g. `impl Trait for Type`).
    Defines,
    /// `from` imports `to` (e.g. a `use` statement).
    Imports,
    /// `from` contains `to` (e.g. an `impl` block contains a method).
    Contains,
}

impl EdgeKind {
    /// Stable string form (mirrors the `ckg_edges.kind` CHECK domain).
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "Calls",
            EdgeKind::Defines => "Defines",
            EdgeKind::Imports => "Imports",
            EdgeKind::Contains => "Contains",
        }
    }
}

/// A directed edge in the knowledge graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub file: String,
    pub line: usize,
}

/// A symbol in the neighborhood of a queried name, together with how it relates
/// to the seed and where it lives (used to group by file in context injection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedSymbol {
    pub symbol: Symbol,
    pub relation: EdgeKind,
    pub file: String,
    pub line: usize,
}

/// The CKG query abstraction. Implemented by [`InMemoryCkg`] (default) and
/// [`PgCkgStore`] (Postgres-backed). Kept **sync** so it slots into the existing
/// sync `retrieval` / `context_manager` paths without forcing them async.
pub trait CkgIndex: Send + Sync {
    /// Return the neighborhood of `symbol_or_name`: symbols connected by
    /// `Calls` / `Defines` / `Contains` edges (both directions), grouped by file
    /// (sorted by file then line) and de-duplicated.
    fn query(&self, symbol_or_name: &str) -> Vec<RelatedSymbol>;
}

// ---------------------------------------------------------------------------
// Parsing (manual, dependency-free)
// ---------------------------------------------------------------------------

/// Whole-word boundary check + identifier extraction after a keyword.
///
/// Returns the identifier that follows `kw` (e.g. `fn` -> function name,
/// `impl` -> implemented type) only when `kw` appears as a standalone keyword
/// (preceded by a boundary char, followed directly by a boundary char), so
/// `implicit` does not match `impl` and `myfn` does not match `fn`.
fn decl_name(line: &str, kw: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut pos = 0;
    while let Some(p) = line[pos..].find(kw) {
        let abs = pos + p;
        // Preceding boundary: start of line, space, tab, '(' or newline.
        if abs > 0 {
            let c = bytes[abs - 1];
            if !(c == b' ' || c == b'\t' || c == b'(' || c == b'\n') {
                pos = abs + kw.len();
                continue;
            }
        }
        // Following char must be a boundary (space/tab/'<'/'(', or end of line).
        let after_idx = abs + kw.len();
        if after_idx < bytes.len() {
            let c = bytes[after_idx];
            if !(c == b' ' || c == b'\t' || c == b'<' || c == b'(') {
                pos = after_idx;
                continue;
            }
        }
        // Extract the identifier (skip a leading generic `<...>` group).
        let mut rest = &line[after_idx..];
        if let Some(stripped) = rest.strip_prefix('<') {
            rest = stripped;
        }
        rest = rest.trim_start();
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if ident.is_empty() {
            return None;
        }
        return Some(ident);
    }
    None
}

/// Rust keywords that look like calls (`if (`, `while (`, ...) but are not.
fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "if" | "for"
            | "while"
            | "loop"
            | "match"
            | "return"
            | "let"
            | "fn"
            | "struct"
            | "impl"
            | "trait"
            | "mod"
            | "use"
            | "const"
            | "static"
            | "async"
            | "await"
            | "move"
            | "ref"
            | "where"
            | "as"
            | "unsafe"
            | "mut"
            | "pub"
            | "self"
            | "Self"
            | "crate"
            | "super"
            | "dyn"
            | "enum"
            | "type"
            | "in"
            | "do"
            | "else"
            | "break"
            | "continue"
            | "true"
            | "false"
    )
}

/// Collect identifiers immediately followed by `(` on a line (call sites).
/// Skips keywords (handled by the caller).
fn collect_calls(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_ident_start(b) {
            let start = i;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let ident: String = line[start..i].chars().collect();
            // Skip whitespace, then expect '('.
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                out.push(ident);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse `use` statement targets into imported symbol names.
/// Handles `use a::b::C;`, `use a::{B, C};`, `use a::B as X;`.
fn parse_use_targets(line: &str) -> Vec<String> {
    let body = line.trim().strip_prefix("use ").unwrap_or(line.trim());
    let body = body.strip_suffix(';').unwrap_or(body).trim();
    let mut out = Vec::new();
    if let Some(idx) = body.find('{') {
        let inner = body[idx + 1..].split('}').next().unwrap_or("");
        for part in inner.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            // `B as X` -> take `B`.
            let name = p.split_whitespace().next().unwrap_or(p).trim().to_string();
            if !name.is_empty() {
                out.push(name);
            }
        }
        return out;
    }
    if let Some(seg) = body.rsplit("::").next() {
        let seg = seg.trim();
        if !seg.is_empty() {
            out.push(seg.to_string());
        }
    }
    out
}

/// Lightweight source parser: extract symbols + edges from one file.
///
/// Pure (no I/O). Walks lines tracking the current `fn` (for `Calls` edges) and
/// the current `impl` (for `Defines` / `Contains` edges), and accumulates
/// preceding `///` doc comments onto the next declaration.
pub fn parse_source(file: &str, content: &str) -> (Vec<Symbol>, Vec<Edge>) {
    let mut symbols: Vec<Symbol> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut doc = String::new();
    let mut current_fn: Option<String> = None;
    let mut current_impl: Option<String> = None;

    for (i, raw) in content.lines().enumerate() {
        let lineno = i + 1;
        let trimmed = raw.trim_start();

        // Doc comment accumulation (skip the line; do not reset `current_*`).
        if let Some(d) = trimmed.strip_prefix("///") {
            if !doc.is_empty() {
                doc.push(' ');
            }
            doc.push_str(d.trim());
            continue;
        }

        // `impl <Type>` / `impl<T> Type` / `impl Trait for Type`.
        // The impl node is named after the **TYPE** (e.g. `impl Dog`),
        // not the trait, so `Defines`/`Contains` edges key off the type.
        if raw.trim_start().starts_with("impl") {
            // For `impl Trait for Type` the node is named after `Type` and the
            // impl "Defines" the trait for that type; otherwise (inherent impl)
            // there is no trait to define.
            let (impl_type, trait_name) = if let Some(pos) = raw.find(" for ") {
                let ty = raw[pos + " for ".len()..]
                    .trim_start()
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("")
                    .to_string();
                let tr = raw[..pos]
                    .trim_start()
                    .strip_prefix("impl")
                    .unwrap_or("")
                    .trim_start()
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("")
                    .to_string();
                (ty, tr)
            } else {
                (decl_name(raw, "impl").unwrap_or_default(), String::new())
            };
            if !impl_type.is_empty() {
                symbols.push(Symbol {
                    kind: SymbolKind::Impl,
                    name: format!("impl {impl_type}"),
                    file: file.to_string(),
                    line: lineno,
                    doc: std::mem::take(&mut doc),
                });
                // `impl Trait for Type` -> the impl defines the trait for `Type`.
                if !trait_name.is_empty() {
                    edges.push(Edge {
                        from: format!("impl {impl_type}"),
                        to: trait_name,
                        kind: EdgeKind::Defines,
                        file: file.to_string(),
                        line: lineno,
                    });
                }
                current_impl = Some(impl_type);
                current_fn = None;
                continue;
            }
        }
        // `struct <Name>` / `trait <Name>` / `mod <Name>` — all "defined" by the
        // enclosing impl (if any) via a `Defines` edge.
        if let Some(name) = decl_name(raw, "struct")
            .or_else(|| decl_name(raw, "trait"))
            .or_else(|| decl_name(raw, "mod"))
        {
            if let Some(t) = current_impl.clone() {
                edges.push(Edge {
                    from: format!("impl {t}"),
                    to: name.clone(),
                    kind: EdgeKind::Defines,
                    file: file.to_string(),
                    line: lineno,
                });
            }
            symbols.push(Symbol {
                kind: if decl_name(raw, "struct").is_some() {
                    SymbolKind::Struct
                } else if decl_name(raw, "trait").is_some() {
                    SymbolKind::Trait
                } else {
                    SymbolKind::Mod
                },
                name,
                file: file.to_string(),
                line: lineno,
                doc: std::mem::take(&mut doc),
            });
            current_fn = None;
            continue;
        }
        // `fn <name>` — contained by the enclosing impl (if any) via `Contains`,
        // and a source of `Calls` edges for callees found on later lines.
        if let Some(name) = decl_name(raw, "fn") {
            if let Some(t) = current_impl.clone() {
                edges.push(Edge {
                    from: format!("impl {t}"),
                    to: name.clone(),
                    kind: EdgeKind::Contains,
                    file: file.to_string(),
                    line: lineno,
                });
            }
            symbols.push(Symbol {
                kind: SymbolKind::Fn,
                name: name.clone(),
                file: file.to_string(),
                line: lineno,
                doc: std::mem::take(&mut doc),
            });
            current_fn = Some(name);
            continue;
        }
        // `use ...` => `Imports` edge (file -> imported name).
        if trimmed.starts_with("use ") {
            for target in parse_use_targets(raw) {
                edges.push(Edge {
                    from: file.to_string(),
                    to: target,
                    kind: EdgeKind::Imports,
                    file: file.to_string(),
                    line: lineno,
                });
            }
            continue;
        }
        // Call sites within the current function region.
        if let Some(fname) = current_fn.clone() {
            for callee in collect_calls(raw) {
                if is_keyword(&callee) {
                    continue;
                }
                edges.push(Edge {
                    from: fname.clone(),
                    to: callee,
                    kind: EdgeKind::Calls,
                    file: file.to_string(),
                    line: lineno,
                });
            }
        }
    }

    (symbols, edges)
}

// ---------------------------------------------------------------------------
// In-memory CKG backend (default)
// ---------------------------------------------------------------------------

/// In-memory code knowledge graph. The default [`CkgIndex`] implementation:
/// parse files with [`parse_source`], then query neighborhoods linearly.
#[derive(Debug, Default)]
pub struct InMemoryCkg {
    symbols: Vec<Symbol>,
    edges: Vec<Edge>,
}

impl InMemoryCkg {
    /// Empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse and ingest one source file.
    pub fn ingest_file(&mut self, file: &str, content: &str) {
        let (syms, edges) = parse_source(file, content);
        self.symbols.extend(syms);
        self.edges.extend(edges);
    }

    /// Build a graph from a corpus of `(file, content)` pairs.
    pub fn from_files(files: &[(String, String)]) -> Self {
        let mut g = Self::new();
        for (f, c) in files {
            g.ingest_file(f, c);
        }
        g
    }

    /// Read-only view of extracted symbols.
    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Read-only view of extracted edges.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    fn symbol_by_name(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.name == name)
    }

    /// Neighborhood query: returns symbols connected to any symbol whose name
    /// contains `symbol_or_name` via `Calls` / `Defines` / `Contains` edges
    /// (both directions), grouped by file and de-duplicated.
    pub fn query(&self, symbol_or_name: &str) -> Vec<RelatedSymbol> {
        let needle = symbol_or_name.trim();
        let relevant = [
            EdgeKind::Calls,
            EdgeKind::Defines,
            EdgeKind::Contains,
        ];
        let mut out: Vec<RelatedSymbol> = Vec::new();
        let seeds: Vec<&Symbol> = self
            .symbols
            .iter()
            .filter(|s| s.name.contains(needle))
            .collect();
        for seed in &seeds {
            for e in &self.edges {
                if !relevant.contains(&e.kind) {
                    continue;
                }
                let related: Option<&Symbol> = if e.from == seed.name {
                    self.symbol_by_name(&e.to)
                } else if e.to == seed.name {
                    self.symbol_by_name(&e.from)
                } else {
                    None
                };
                if let Some(sym) = related {
                    if sym.name == seed.name {
                        continue; // skip self-loops
                    }
                    out.push(RelatedSymbol {
                        symbol: sym.clone(),
                        relation: e.kind,
                        file: sym.file.clone(),
                        line: sym.line,
                    });
                }
            }
        }
        // Group by file: sort by file, then line, then name; then de-duplicate.
        out.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.symbol.name.cmp(&b.symbol.name))
        });
        out.dedup_by(|a, b| {
            a.file == b.file
                && a.line == b.line
                && a.symbol.name == b.symbol.name
                && a.relation == b.relation
        });
        out
    }
}

impl CkgIndex for InMemoryCkg {
    fn query(&self, symbol_or_name: &str) -> Vec<RelatedSymbol> {
        self.query(symbol_or_name)
    }
}

// ---------------------------------------------------------------------------
// Optional Postgres-backed CKG (pure-incremental 0004_v15.sql)
// ---------------------------------------------------------------------------

/// Postgres-backed CKG store (T17, optional).
///
/// Keeps an in-memory [`InMemoryCkg`] **mirror** so [`CkgIndex::query`] stays
/// sync + lock-free-ish (reads the mirror), while best-effort persisting parsed
/// symbols/edges to the `0004_v15.sql` tables. If the DB is unreachable,
/// [`PgCkgStore::connect`] degrades to an in-memory-only store (query still
/// works); [`PgCkgStore::new`] builds such a store directly (no DB).
pub struct PgCkgStore {
    mirror: Mutex<InMemoryCkg>,
    client: Mutex<Option<Arc<tokio_postgres::Client>>>,
    url: String,
    /// Tenant this store writes/reads under (v2.0 T20 RLS). Empty for the
    /// in-memory-only store built by [`PgCkgStore::new`].
    tenant_id: String,
}

impl Default for PgCkgStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PgCkgStore {
    /// Build an in-memory-only store (no DB connection attempted).
    pub fn new() -> Self {
        Self {
            mirror: Mutex::new(InMemoryCkg::new()),
            client: Mutex::new(None),
            url: String::new(),
            tenant_id: String::new(),
        }
    }

    /// Connect to Postgres and load any previously persisted graph. On DB
    /// failure, logs and returns a usable in-memory-only store. Pins `tenant_id`
    /// as the session tenant so the 0005 RLS policy scopes every row (v2.0 T20).
    pub async fn connect(url: &str, tenant_id: &str) -> Self {
        let mut store = Self::new();
        store.url = url.to_string();
        store.tenant_id = tenant_id.to_string();
        match tokio_postgres::connect(url, NoTls).await {
            Ok((client, conn)) => {
                // Drive the connection in the background.
                tokio::spawn(async move {
                    if let Err(e) = conn.await {
                        tracing::warn!("ckg db connection closed: {e}");
                    }
                });
                // Pin the session tenant so the tenant RLS policy is satisfied.
                let _ = client.execute("SET app.tenant_id = $1", &[&tenant_id]).await;
                *store.client.lock().expect("ckg client poisoned") = Some(Arc::new(client));
                if let Err(e) = store.load().await {
                    tracing::warn!("ckg load from db failed: {e}");
                }
            }
            Err(e) => {
                tracing::warn!("ckg db unavailable ({e}); using in-memory mirror only");
            }
        }
        store
    }

    /// Parse + ingest a source file into the mirror; best-effort persist to PG.
    pub async fn ingest_file(&self, file: &str, content: &str) {
        let (syms, edges) = parse_source(file, content);
        {
            let mut m = self.mirror.lock().expect("ckg mirror poisoned");
            m.symbols.extend(syms.clone());
            m.edges.extend(edges.clone());
        }
        // Persist outside the lock (clone the client handle first).
        let client = self.client.lock().expect("ckg client poisoned").clone();
        if let Some(client) = client {
            for s in &syms {
                let line = s.line as i64;
                if let Err(e) = client
                    .execute(
                        "INSERT INTO ckg_symbols (tenant_id, kind, name, file, line, doc) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                        &[
                            &self.tenant_id,
                            &s.kind.as_str(),
                            &s.name.as_str(),
                            &s.file.as_str(),
                            &line,
                            &s.doc.as_str(),
                        ],
                    )
                    .await
                {
                    tracing::warn!("ckg persist symbol failed: {e}");
                }
            }
            for e in &edges {
                let line = e.line as i64;
                if let Err(err) = client
                    .execute(
                        "INSERT INTO ckg_edges (tenant_id, from_name, to_name, kind, file, line) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                        &[
                            &self.tenant_id,
                            &e.from,
                            &e.to.as_str(),
                            &e.kind.as_str(),
                            &e.file.as_str(),
                            &line,
                        ],
                    )
                    .await
                {
                    tracing::warn!("ckg persist edge failed: {err}");
                }
            }
        }
    }

    /// Load persisted symbols/edges from PG into the mirror.
    async fn load(&self) -> anyhow::Result<()> {
        let client = match self.client.lock().expect("ckg client poisoned").clone() {
            Some(c) => c,
            None => return Ok(()),
        };
        let rows = client
            .query("SELECT kind, name, file, line, doc FROM ckg_symbols", &[])
            .await?;
        let mut m = self.mirror.lock().expect("ckg mirror poisoned");
        for row in rows {
            let kind: String = row.try_get(0)?;
            let name: String = row.try_get(1)?;
            let file: String = row.try_get(2)?;
            let line: i64 = row.try_get(3)?;
            let doc: String = row.try_get(4)?;
            let kind = match kind.as_str() {
                "Mod" => SymbolKind::Mod,
                "Fn" => SymbolKind::Fn,
                "Struct" => SymbolKind::Struct,
                "Impl" => SymbolKind::Impl,
                "Trait" => SymbolKind::Trait,
                _ => SymbolKind::Fn,
            };
            m.symbols.push(Symbol {
                kind,
                name,
                file,
                line: line as usize,
                doc,
            });
        }
        let erows = client
            .query(
                "SELECT from_name, to_name, kind, file, line FROM ckg_edges",
                &[],
            )
            .await?;
        for row in erows {
            let from: String = row.try_get(0)?;
            let to: String = row.try_get(1)?;
            let kind: String = row.try_get(2)?;
            let file: String = row.try_get(3)?;
            let line: i64 = row.try_get(4)?;
            let kind = match kind.as_str() {
                "Calls" => EdgeKind::Calls,
                "Defines" => EdgeKind::Defines,
                "Imports" => EdgeKind::Imports,
                "Contains" => EdgeKind::Contains,
                _ => EdgeKind::Calls,
            };
            m.edges.push(Edge {
                from,
                to,
                kind,
                file,
                line: line as usize,
            });
        }
        Ok(())
    }
}

impl CkgIndex for PgCkgStore {
    fn query(&self, symbol_or_name: &str) -> Vec<RelatedSymbol> {
        self.mirror.lock().expect("ckg mirror poisoned").query(symbol_or_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
/// retries a request on timeout
fn retry() {}

fn process() {
    retry();
    let _ = compute(1);
}

fn compute(x: i32) -> i32 { x }
";

    #[test]
    fn extracts_symbols_and_edges_from_source() {
        let (syms, edges) = parse_source("a.rs", SRC);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"retry"));
        assert!(names.contains(&"process"));
        assert!(names.contains(&"compute"));

        let calls: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .map(|e| e.to.as_str())
            .collect();
        assert!(calls.contains(&"retry"), "process must call retry");
        assert!(calls.contains(&"compute"), "process must call compute");

        let retry = syms.iter().find(|s| s.name == "retry").unwrap();
        assert_eq!(retry.kind, SymbolKind::Fn);
        assert_eq!(retry.doc, "retries a request on timeout");
        assert_eq!(retry.line, 2);
    }

    #[test]
    fn parses_impl_contains_and_defines() {
        let src = "\
trait Animal { fn speak(&self); }
struct Dog;
impl Animal for Dog {
    fn speak(&self) { bark(); }
}
fn bark() {}
";
        let (syms, edges) = parse_source("b.rs", src);
        assert!(syms.iter().any(|s| s.name == "impl Dog"));
        // impl Dog Defines trait Animal.
        assert!(edges.iter().any(|e| e.from == "impl Dog"
            && e.to == "Animal"
            && e.kind == EdgeKind::Defines));
        // impl Dog Contains speak.
        assert!(edges.iter().any(|e| e.from == "impl Dog"
            && e.to == "speak"
            && e.kind == EdgeKind::Contains));
        // speak calls bark.
        assert!(edges.iter().any(|e| e.from == "speak"
            && e.to == "bark"
            && e.kind == EdgeKind::Calls));
    }

    #[test]
    fn parses_use_imports() {
        let (_, edges) = parse_source("c.rs", "use std::collections::HashMap;\nuse foo::{Bar, Baz};\n");
        let imports: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .map(|e| e.to.as_str())
            .collect();
        assert!(imports.contains(&"HashMap"));
        assert!(imports.contains(&"Bar"));
        assert!(imports.contains(&"Baz"));
    }

    #[test]
    fn query_neighborhood_and_reverse_lookup() {
        let mut ckg = InMemoryCkg::new();
        ckg.ingest_file("a.rs", SRC);
        // Forward: query `process` -> its callees retry + compute.
        let rel = ckg.query("process");
        let names: Vec<&str> = rel.iter().map(|r| r.symbol.name.as_str()).collect();
        assert!(names.contains(&"retry"));
        assert!(names.contains(&"compute"));
        assert!(
            rel.iter()
                .any(|r| r.symbol.name == "retry" && r.relation == EdgeKind::Calls)
        );
        // Reverse: query `retry` -> the caller `process` (incoming Calls).
        let rel2 = ckg.query("retry");
        assert!(rel2.iter().any(|r| r.symbol.name == "process"));
    }

    #[test]
    fn query_groups_by_file_and_dedups() {
        let mut ckg = InMemoryCkg::new();
        ckg.ingest_file("a.rs", SRC);
        ckg.ingest_file("z.rs", SRC); // same content in a different file
        let rel = ckg.query("process");
        // Both files contribute; sorted by file, no duplicate (file,line,name).
        assert!(rel.iter().any(|r| r.file == "a.rs"));
        assert!(rel.iter().any(|r| r.file == "z.rs"));
        // Within a single file, `retry` appears once (not duplicated by direction).
        let a_retry = rel.iter().filter(|r| r.file == "a.rs" && r.symbol.name == "retry").count();
        assert_eq!(a_retry, 1);
    }

    #[tokio::test]
    async fn pg_ckg_store_works_without_db() {
        // `new()` builds an in-memory-only store (no DB); query reads the mirror.
        let store = PgCkgStore::new();
        store.ingest_file("a.rs", SRC).await;
        let rel = store.query("retry");
        assert!(rel.iter().any(|r| r.symbol.name == "process"));
    }
}
