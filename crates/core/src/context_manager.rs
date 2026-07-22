//! Context manager — maintains the working memory fed to the LLM (T01 / T05).
//!
//! M1 provided a rolling window plus a deterministic placeholder embedding
//! ([`embed`]). v0.5 Stage A (T05) extends it into a **context-engineering**
//! subsystem:
//!   * **Multi-source collection** — open file content, current selection,
//!     LSP diagnostics, symbol table, and recent-edit diffs are gathered as
//!     typed [`ContextChunk`]s.
//!   * **Tiered token-budget trimming** — when the budget is exceeded, lowest
//!     priority chunks are dropped first (Low -> Medium -> High).
//!   * **Layered compression** — de-duplication and per-chunk truncation.
//!   * **Retrieval** — vector similarity over the gathered chunks, delegating to
//!     the pure ranking in [`crate::retrieval`] (which mirrors the SQL
//!     `search_context` semantics without requiring PG for the M1/v0.5 demo).
//!
//! The vectorization helper [`embed`] produces a deterministic placeholder
//! embedding (feature-hashing the input into a fixed-dimensional vector). This
//! keeps the Core free of an external model dependency while mirroring the
//! `embeddings`/`context_sources` table shape in the DDL. Pure and unit-tested.

use crate::ckg::CkgIndex;
use crate::retrieval::{rank_raw, RetrievedChunk};
use std::collections::{HashSet, VecDeque};

/// Fixed embedding dimension (must match `vector(N)` in the DDL).
pub const EMBED_DIM: usize = 1536;

/// Where a context chunk was gathered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextSource {
    /// Whole content of an open editor document.
    OpenFile,
    /// The current editor selection / highlighted range.
    Selection,
    /// LSP diagnostics (errors, warnings) for a file.
    Diagnostic,
    /// A symbol definition pulled from the symbol table / CKG.
    Symbol,
    /// A recent edit diff (last N changed lines).
    RecentEdit,
}

impl ContextSource {
    /// Stable string form (mirrors `context_sources.source` CHECK constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            ContextSource::OpenFile => "OpenFile",
            ContextSource::Selection => "Selection",
            ContextSource::Diagnostic => "Diagnostic",
            ContextSource::Symbol => "Symbol",
            ContextSource::RecentEdit => "RecentEdit",
        }
    }
}

/// Priority tier used for tiered token-budget dropping (Low dropped first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Dropped first under budget pressure.
    Low = 0,
    /// Dropped after Low.
    Medium = 1,
    /// Never dropped by budget trimming (kept at all costs).
    High = 2,
}

/// A single gathered piece of context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextChunk {
    pub source: ContextSource,
    pub uri: String,
    pub content: String,
    pub priority: Priority,
    /// Pre-computed token estimate (see [`estimate_tokens`]).
    pub token_est: usize,
}

impl ContextChunk {
    /// Build a chunk and pre-compute its `token_est`.
    pub fn new(source: ContextSource, uri: &str, content: &str, priority: Priority) -> Self {
        Self {
            source,
            uri: uri.to_string(),
            content: content.to_string(),
            priority,
            token_est: estimate_tokens(content),
        }
    }
}

/// Rough token estimate: ~4 chars per token (English-ish approximation).
/// Pure; unit-tested against a known string.
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() + 3) / 4
}

/// Report produced by [`ContextManager::budget_trim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrimReport {
    pub dropped: usize,
    pub kept: usize,
}

/// Rolling context window + multi-source chunk store + token budget.
#[derive(Debug, Default)]
pub struct ContextManager {
    /// Recent raw observations (the M1 rolling window).
    window: VecDeque<String>,
    max_items: usize,
    /// Typed multi-source context chunks (T05).
    chunks: Vec<ContextChunk>,
    /// Token budget across window + chunks.
    budget_tokens: usize,
}

impl ContextManager {
    /// Create a manager keeping the last `max_items` raw observations.
    /// Defaults the token budget to 4096 (M1-compatible behaviour).
    pub fn new(max_items: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(max_items),
            max_items: max_items.max(1),
            chunks: Vec::new(),
            budget_tokens: 4096,
        }
    }

    /// Create a manager with an explicit token budget (T05).
    pub fn with_budget(max_items: usize, budget_tokens: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(max_items.max(1)),
            max_items: max_items.max(1),
            chunks: Vec::new(),
            budget_tokens: budget_tokens.max(1),
        }
    }

    /// Append a raw observation to the window (evicting the oldest if full).
    pub fn push(&mut self, item: &str) {
        if self.window.len() == self.max_items {
            self.window.pop_front();
        }
        self.window.push_back(item.to_string());
    }

    /// Add a typed multi-source context chunk.
    pub fn add_chunk(&mut self, chunk: ContextChunk) {
        self.chunks.push(chunk);
    }

    /// Read-only view of gathered chunks.
    pub fn chunks(&self) -> &[ContextChunk] {
        &self.chunks
    }

    /// Current total estimated tokens (window + chunks).
    fn total_tokens(&self) -> usize {
        self.window.iter().map(|w| estimate_tokens(w)).sum::<usize>()
            + self.chunks.iter().map(|c| c.token_est).sum::<usize>()
    }

    /// Trim chunks to fit the token budget, dropping the lowest-priority chunks
    /// first (Low -> Medium -> High). High-priority chunks are never dropped by
    /// this routine; if only High chunks remain and we are still over budget,
    /// trimming stops (caller may compress instead).
    pub fn budget_trim(&mut self) -> TrimReport {
        let mut dropped = 0;
        while self.total_tokens() > self.budget_tokens && !self.chunks.is_empty() {
            // Find the index of the lowest-priority chunk (ties: earliest).
            let mut worst: Option<usize> = None;
            let mut worst_pri = Priority::High;
            for (i, c) in self.chunks.iter().enumerate() {
                if worst.is_none() || c.priority < worst_pri {
                    worst = Some(i);
                    worst_pri = c.priority;
                }
            }
            // Avoid an infinite loop: never drop High-priority chunks here.
            if worst_pri == Priority::High {
                break;
            }
            if let Some(i) = worst {
                self.chunks.remove(i);
                dropped += 1;
            } else {
                break;
            }
        }
        TrimReport {
            dropped,
            kept: self.chunks.len(),
        }
    }

    /// Layered compression: remove exact-duplicate content, then truncate any
    /// oversized chunk to a per-chunk cap, re-estimating tokens.
    pub fn compress(&mut self) {
        let mut seen = HashSet::new();
        self.chunks.retain(|c| seen.insert(c.content.clone()));
        const CAP: usize = 2000;
        for c in &mut self.chunks {
            if c.content.len() > CAP {
                c.content = format!("{}…[truncated]", &c.content[..CAP]);
                c.token_est = estimate_tokens(&c.content);
            }
        }
    }

    /// Serialize the raw window into a single context string for the LLM (M1).
    pub fn snapshot(&self) -> String {
        self.window
            .iter()
            .enumerate()
            .fold(String::new(), |mut acc, (i, s)| {
                acc.push_str(&format!("[{i}] {s}\n"));
                acc
            })
    }

    /// Build the full prompt-ready context: multi-source chunks first, then the
    /// rolling working-memory window. Used by both Chat (T06) and the Planner
    /// T05 wiring (T01 `run_with_context`).
    pub fn build_prompt(&self) -> String {
        let mut out = String::from("=== Context ===\n");
        for c in &self.chunks {
            out.push_str(&format!(
                "[{}] {}: {}\n",
                c.source.as_str(),
                c.uri,
                c.content
            ));
        }
        out.push_str("=== Working memory ===\n");
        out.push_str(&self.snapshot());
        out
    }

    /// Vector retrieval over the gathered chunks (mirrors SQL `search_context`).
    /// Returns the top-`k` chunks by cosine similarity to `query`.
    pub fn retrieve(&self, query: &str, k: usize) -> Vec<RetrievedChunk> {
        rank_raw(query, &self.chunks, k)
    }

    /// Enrich the context with code-knowledge-graph neighbors of `symbol` (T17).
    ///
    /// Queries the supplied [`CkgIndex`] and appends each related symbol as a
    /// `Symbol`-sourced [`ContextChunk`] at `Medium` priority. This goes *beyond*
    /// pure-text similarity: a referenced symbol pulls in the functions that
    /// call it / it calls, the `impl` that contains it, etc. The v1.5A
    /// governance chain is untouched — this only affects what context is fed to
    /// the LLM (no permission checks, no host edits).
    pub fn enrich_from_ckg(&mut self, symbol: &str, ckg: &dyn CkgIndex) {
        for r in ckg.query(symbol) {
            let content = format!(
                "{} {} (relation: {:?}, line {})",
                r.symbol.kind.as_str(),
                r.symbol.name,
                r.relation,
                r.symbol.line
            );
            self.add_chunk(ContextChunk::new(
                ContextSource::Symbol,
                &r.file,
                &content,
                Priority::Medium,
            ));
        }
    }
}

/// Deterministic placeholder embedding via feature hashing.
///
/// Real Core will call an embedding model; this keeps M1/v0.5 self-contained
/// and `cargo test`-able. Output is L2-normalized to mirror pgvector usage.
pub fn embed(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBED_DIM];
    for (i, ch) in text.chars().enumerate() {
        let idx = (ch as usize * 31 + i) % EMBED_DIM;
        vec[idx] += 1.0;
    }
    // L2 normalize.
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
    vec
}

/// Cosine similarity between two equal-length vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|y| y * y).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::RetrievedChunk;

    #[test]
    fn embed_is_deterministic_and_normalized() {
        let a = embed("fn main() {}");
        let b = embed("fn main() {}");
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
        assert_eq!(a.len(), EMBED_DIM);
    }

    #[test]
    fn similar_texts_score_higher() {
        let a = embed("retry the request on timeout");
        let b = embed("retry the request on timeout");
        let c = embed("completely different content here");
        assert!(cosine_similarity(&a, &b) > cosine_similarity(&a, &c));
    }

    #[test]
    fn context_window_rolls() {
        let mut mgr = ContextManager::new(2);
        mgr.push("one");
        mgr.push("two");
        mgr.push("three");
        let snap = mgr.snapshot();
        assert!(snap.contains("three"));
        assert!(!snap.contains("[0] one"));
    }

    #[test]
    fn estimate_tokens_approx_four_chars() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn budget_trim_drops_low_priority_first() {
        let mut mgr = ContextManager::with_budget(8, 8);
        // ~2 tokens each (8 chars). Budget 8 => keep ~4 chunks.
        mgr.add_chunk(ContextChunk::new(ContextSource::OpenFile, "a", "12345678", Priority::High));
        mgr.add_chunk(ContextChunk::new(ContextSource::Symbol, "b", "12345678", Priority::Low));
        mgr.add_chunk(ContextChunk::new(ContextSource::Diagnostic, "c", "12345678", Priority::Medium));
        mgr.add_chunk(ContextChunk::new(ContextSource::RecentEdit, "d", "12345678", Priority::Low));
        mgr.add_chunk(ContextChunk::new(ContextSource::Selection, "e", "12345678", Priority::Medium));
        mgr.add_chunk(ContextChunk::new(ContextSource::OpenFile, "f", "12345678", Priority::High));
        let report = mgr.budget_trim();
        // Two Low chunks must be dropped first.
        assert_eq!(report.dropped, 2);
        assert_eq!(report.kept, 4);
        // No High chunk should have been dropped.
        assert!(mgr.chunks().iter().all(|c| c.priority != Priority::Low));
        assert!(mgr.chunks().iter().any(|c| c.priority == Priority::High));
    }

    #[test]
    fn compress_dedupes_and_truncates() {
        let mut mgr = ContextManager::with_budget(8, 4096);
        mgr.add_chunk(ContextChunk::new(ContextSource::OpenFile, "a", "duplicate", Priority::Low));
        mgr.add_chunk(ContextChunk::new(ContextSource::OpenFile, "b", "duplicate", Priority::Low));
        mgr.add_chunk(ContextChunk::new(ContextSource::OpenFile, "c", "x".repeat(5000), Priority::Low));
        mgr.compress();
        // Exact-duplicate content removed.
        assert_eq!(mgr.chunks().len(), 2);
        // Oversized chunk truncated near the cap.
        let big = mgr.chunks().iter().find(|c| c.uri == "c").unwrap();
        assert!(big.content.starts_with('x'));
        assert!(big.content.len() <= 2000 + 12);
    }

    #[test]
    fn retrieve_ranks_by_similarity() {
        let mut mgr = ContextManager::with_budget(8, 4096);
        mgr.add_chunk(ContextChunk::new(
            ContextSource::OpenFile,
            "retry.rs",
            "retry the request on timeout",
            Priority::High,
        ));
        mgr.add_chunk(ContextChunk::new(
            ContextSource::Symbol,
            "other.rs",
            "completely unrelated content here",
            Priority::Low,
        ));
        let top: Vec<RetrievedChunk> = mgr.retrieve("retry the request on timeout", 1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].chunk.uri, "retry.rs");
        assert!(top[0].score > 0.9);
    }

    #[test]
    fn build_prompt_includes_chunks_and_window() {
        let mut mgr = ContextManager::with_budget(8, 4096);
        mgr.add_chunk(ContextChunk::new(
            ContextSource::OpenFile,
            "src/main.rs",
            "fn main() {}",
            Priority::High,
        ));
        mgr.push("goal: demo");
        let p = mgr.build_prompt();
        assert!(p.contains("src/main.rs"));
        assert!(p.contains("fn main() {}"));
        assert!(p.contains("goal: demo"));
    }

    #[test]
    fn enrich_from_ckg_adds_symbol_chunks() {
        use crate::ckg::InMemoryCkg;
        let mut ckg = InMemoryCkg::new();
        ckg.ingest_file(
            "a.rs",
            "fn retry() {}\nfn process() {\n    retry();\n}\n",
        );
        let mut mgr = ContextManager::with_budget(8, 4096);
        mgr.enrich_from_ckg("process", &ckg);
        let syms: Vec<&ContextChunk> = mgr
            .chunks()
            .iter()
            .filter(|c| c.source == ContextSource::Symbol)
            .collect();
        assert!(!syms.is_empty(), "CKG neighbors must be injected");
        assert!(
            syms.iter().any(|c| c.content.contains("retry")),
            "the caller/callee `retry` should appear via CKG"
        );
    }
}
