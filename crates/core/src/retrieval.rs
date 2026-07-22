//! Retrieval & ranking — pure logic for context engineering (T05, F5-base).
//!
//! This module is intentionally free of I/O and external dependencies so it can
//! be unit-tested with `cargo test` and reused by both the in-process
//! [`ContextManager`](crate::context_manager::ContextManager) and (later) a PG
//! `search_context` backend. The in-memory implementation mirrors the SQL
//! `search_context(p_project_id, query_vec, k)` semantics: embed the query,
//! rank candidates by cosine similarity, take the top-k.

use crate::ckg::CkgIndex;
use crate::context_manager::{embed, cosine_similarity, ContextChunk, ContextSource, Priority};

/// A retrieved context chunk paired with its similarity score in [0,1].
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievedChunk {
    pub chunk: ContextChunk,
    pub score: f32,
}

/// Retrieval backend abstraction. Production swaps this for a PG/pgvector or
/// symbol-index backed implementation; the pure ranking stays identical.
pub trait Retriever: Send + Sync {
    /// Return the top-`k` chunks most similar to `query`.
    fn retrieve(&self, query: &str, k: usize) -> Vec<RetrievedChunk>;
}

// NOTE: `Retriever` is intentionally a **sync** trait (retrieval is a pure
// lookup); no `async_trait` needed. Production backends that perform I/O would
// wrap their async call inside this method.

/// In-memory vector retriever over a pre-indexed corpus.
///
/// Embeddings are computed once at construction (mirroring how `embeddings`/
/// `context_sources` rows are stored in PG), so repeated `retrieve` calls are
/// cheap and deterministic.
pub struct InMemoryRetriever {
    indexed: Vec<(ContextChunk, Vec<f32>)>,
}

impl InMemoryRetriever {
    /// Build the index from a corpus of chunks (embeds each once).
    pub fn build(corpus: &[ContextChunk]) -> Self {
        let indexed = corpus
            .iter()
            .map(|c| (c.clone(), embed(&c.content)))
            .collect();
        Self { indexed }
    }

    /// Add a chunk to the index at runtime.
    pub fn add(&mut self, chunk: ContextChunk) {
        self.indexed.push((chunk.clone(), embed(&chunk.content)));
    }
}

impl Retriever for InMemoryRetriever {
    fn retrieve(&self, query: &str, k: usize) -> Vec<RetrievedChunk> {
        rank_chunks(query, &self.indexed, k)
    }
}

/// Pure ranking helper shared by [`InMemoryRetriever`] and any other backend.
///
/// Embeds `query`, scores every `(chunk, embedding)` pair by cosine similarity,
/// sorts descending and returns the top-`k`. No I/O — trivially testable.
pub fn rank_chunks(
    query: &str,
    indexed: &[(ContextChunk, Vec<f32>)],
    k: usize,
) -> Vec<RetrievedChunk> {
    let q = embed(query);
    let mut scored: Vec<RetrievedChunk> = indexed
        .iter()
        .map(|(c, e)| RetrievedChunk {
            chunk: c.clone(),
            score: cosine_similarity(&q, e),
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.into_iter().take(k.max(1)).collect()
}

/// Convenience wrapper: rank raw chunks (embedded on the fly) — used by
/// [`ContextManager::retrieve`](crate::context_manager::ContextManager::retrieve)
/// when no persistent index is available.
pub fn rank_raw(query: &str, candidates: &[ContextChunk], k: usize) -> Vec<RetrievedChunk> {
    let q = embed(query);
    let mut scored: Vec<RetrievedChunk> = candidates
        .iter()
        .map(|c| RetrievedChunk {
            chunk: c.clone(),
            score: cosine_similarity(&q, &embed(&c.content)),
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.into_iter().take(k.max(1)).collect()
}

/// Retrieve by text similarity **and** augment with CKG neighbors (T17).
///
/// Runs the usual vector ranking over `corpus`, then appends the code-knowledge-
/// graph neighbors of `query` (via [`CkgIndex::query`]) as extra
/// [`ContextChunk`]s. This lets a referenced symbol surface its callers/callees
/// even when those are not textually similar to the query — direct context
/// quality beyond what pure embedding retrieval can recover. Appended chunks
/// score `0.0` (they are *supplementary* context, ranked below the similarity
/// hits).
pub fn retrieve_with_ckg(
    query: &str,
    corpus: &[ContextChunk],
    k: usize,
    ckg: &dyn CkgIndex,
) -> Vec<RetrievedChunk> {
    let mut top = rank_raw(query, corpus, k);
    for r in ckg.query(query) {
        let chunk = ContextChunk::new(
            ContextSource::Symbol,
            &r.file,
            &format!(
                "{} {} (relation: {:?}, line {})",
                r.symbol.kind.as_str(),
                r.symbol.name,
                r.relation,
                r.symbol.line
            ),
            Priority::Medium,
        );
        top.push(RetrievedChunk {
            chunk,
            score: 0.0,
        });
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_manager::{ContextChunk, ContextSource, Priority};

    fn chunk(source: ContextSource, uri: &str, content: &str, pri: Priority) -> ContextChunk {
        ContextChunk::new(source, uri, content, pri)
    }

    #[test]
    fn ranks_by_similarity_top_k() {
        let corpus = vec![
            chunk(ContextSource::OpenFile, "a.rs", "retry the request on timeout", Priority::High),
            chunk(ContextSource::Symbol, "b.rs", "completely unrelated content", Priority::Low),
            chunk(ContextSource::RecentEdit, "c.rs", "retry the request when it fails", Priority::Medium),
        ];
        let retriever = InMemoryRetriever::build(&corpus);
        let top = retriever.retrieve("retry the request on timeout", 2);
        assert_eq!(top.len(), 2);
        // The two "retry" chunks should outrank the unrelated one.
        assert!(top[0].score > top.last().unwrap().score);
        assert_eq!(top[0].chunk.uri, "a.rs");
        // All scores must be within cosine bounds [0,1].
        for t in &top {
            assert!((0.0..=1.0).contains(&t.score));
        }
    }

    #[test]
    fn rank_raw_matches_indexed_ordering() {
        let corpus = vec![
            chunk(ContextSource::OpenFile, "a.rs", "fn main retry", Priority::High),
            chunk(ContextSource::Symbol, "b.rs", "unrelated text here", Priority::Low),
        ];
        let retriever = InMemoryRetriever::build(&corpus);
        let via_index = retriever.retrieve("fn main retry", 2);
        let via_raw = rank_raw("fn main retry", &corpus, 2);
        assert_eq!(via_index[0].chunk.uri, via_raw[0].chunk.uri);
    }

    #[test]
    fn retrieve_with_ckg_appends_neighbors() {
        use crate::ckg::InMemoryCkg;
        let corpus = vec![chunk(
            ContextSource::OpenFile,
            "a.rs",
            "retry the request on timeout",
            Priority::High,
        )];
        let mut ckg = InMemoryCkg::new();
        ckg.ingest_file("a.rs", "fn retry() {}\nfn process() {\n    retry();\n}\n");
        let top = retrieve_with_ckg("retry", &corpus, 1, &ckg);
        // The CKG neighbor (`process`, which calls `retry`) must be surfaced
        // beyond pure text similarity.
        assert!(
            top.iter().any(|t| t.chunk.content.contains("process")),
            "CKG neighbor process should be appended"
        );
        // The original similarity match is still present.
        assert!(top.iter().any(|t| t.chunk.uri == "a.rs"));
    }
}
