//! LSP-style completion hook interface + ranking (T04 / T08).
//!
//! [`CompletionProvider`] mirrors an LSP `textDocument/completion` hook: given a
//! document context it returns ranked items. [`rank_completions`] is the pure
//! scoring logic, unit-tested independently of any model.
//!
//! T08 adds [`derive_rule_candidates`] / [`rule_complete`] — the deterministic
//! rule-based completion used both by [`MockOllamaClient`] and the production
//! [`RuleBasedBackend`] degradation path. Keeping it here (pure, no I/O) means
//! the fallback behaviour is tested once and shared by both backends.

use crate::ollama::{Candidate, CompletionBackend};
use async_trait::async_trait;
use std::time::Instant;

/// A document context handed to the completion hook (LSP-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspContext {
    pub document_uri: String,
    pub language: String,
    /// Text before the cursor.
    pub prefix: String,
    /// Text after the cursor.
    pub suffix: String,
    /// Cursor offset (char index) in the document.
    pub cursor_offset: usize,
}

/// A ranked completion item shown as grey ghost text.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionItem {
    pub text: String,
    /// Final display score in [0,1] (higher = better).
    pub score: f32,
}

/// LSP-style completion hook: turns a context into ranked items.
#[async_trait]
pub trait CompletionProvider: Send + Sync {
    /// Produce at most `limit` ranked completions for `ctx`.
    async fn provide(&self, ctx: &LspContext, limit: usize) -> anyhow::Result<Vec<CompletionItem>>;
}

/// Default provider backed by a `CompletionBackend` (Ollama / mock).
pub struct ProbeCompletionProvider<B: CompletionBackend> {
    backend: B,
}

impl<B: CompletionBackend> ProbeCompletionProvider<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: CompletionBackend> CompletionProvider for ProbeCompletionProvider<B> {
    async fn provide(&self, ctx: &LspContext, limit: usize) -> anyhow::Result<Vec<CompletionItem>> {
        let candidates = self.backend.complete(&ctx.prefix, &ctx.suffix, &ctx.language).await?;
        Ok(rank_completions(candidates, limit))
    }
}

/// Pure ranking: sort candidates by confidence (desc), then take `limit`.
///
/// Kept free of I/O so it is trivially unit-testable and reusable by the
/// server-side stream.
pub fn rank_completions(mut candidates: Vec<Candidate>, limit: usize) -> Vec<CompletionItem> {
    candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
        .into_iter()
        .take(limit.max(1))
        .map(|c| CompletionItem {
            text: c.text,
            score: c.confidence,
        })
        .collect()
}

/// Deterministic rule-based completion — the NES degradation path (T08).
///
/// Pure: given `(prefix, suffix, language)` it derives a fallback candidate
/// list without any model. Shared by [`MockOllamaClient`] (tests / CI) and the
/// production [`RuleBasedBackend`] (hot-path degradation), so both stay in
/// lock-step. The first candidate is the heuristic "best guess"; the second is
/// a constant `// fallback` so ranking always has something to surface.
///
/// [`MockOllamaClient`]: crate::ollama::MockOllamaClient
/// [`RuleBasedBackend`]: crate::ollama::RuleBasedBackend
pub fn derive_rule_candidates(prefix: &str, _suffix: &str, _language: &str) -> Vec<Candidate> {
    let cand = if prefix.trim_end().ends_with("fn main(") {
        Candidate {
            text: ") {\n    \n}".to_string(),
            confidence: 0.9,
        }
    } else if let Some(last) = prefix.split_whitespace().last() {
        Candidate {
            text: format!("{last}_suffix"),
            confidence: 0.6,
        }
    } else {
        Candidate {
            text: String::new(),
            confidence: 0.1,
        }
    };
    vec![
        cand,
        Candidate {
            text: "// fallback".to_string(),
            confidence: 0.2,
        },
    ]
}

/// Convenience: the single best rule candidate (used by tests / CLI degradation).
pub fn rule_complete(prefix: &str, suffix: &str, language: &str) -> Candidate {
    derive_rule_candidates(prefix, suffix, language)
        .into_iter()
        .max_by(|a, b| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(Candidate {
            text: String::new(),
            confidence: 0.0,
        })
}

/// Measure end-to-end completion latency and report the L0 Tab budget.
///
/// Returns `(p50_ms, p95_ms)`. Caller asserts `p95 < 300`. Uses the provided
/// backend so it works with [`MockOllamaClient`] (no model required) and with
/// the production [`NesClient`] when a model is reachable.
///
/// [`MockOllamaClient`]: crate::ollama::MockOllamaClient
/// [`NesClient`]: crate::ollama::NesClient
pub async fn speed_test(
    backend: &dyn CompletionBackend,
    samples: usize,
) -> anyhow::Result<(f32, f32)> {
    let mut latencies: Vec<f32> = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let _ = backend.complete("fn main(", "}", "rust").await?;
        latencies.push(start.elapsed().as_secs_f32() * 1000.0);
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = latencies[latencies.len() / 2];
    let idx = ((latencies.len() as f32) * 0.95) as usize;
    let p95 = latencies[idx.min(latencies.len() - 1)];
    Ok((p50, p95))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ollama::MockOllamaClient;

    #[tokio::test]
    async fn ranks_by_confidence_and_limits() {
        let backend = MockOllamaClient::new();
        let ctx = LspContext {
            document_uri: "mem://a.rs".to_string(),
            language: "rust".to_string(),
            prefix: "fn main(".to_string(),
            suffix: "}".to_string(),
            cursor_offset: 8,
        };
        let provider = ProbeCompletionProvider::new(backend);
        let items = provider.provide(&ctx, 1).await.unwrap();
        assert_eq!(items.len(), 1);
        // Highest-confidence candidate is first and is the `fn main(` completion.
        assert!(items[0].score >= 0.9);
        assert!(items[0].text.contains(')'));
    }

    #[tokio::test]
    async fn speed_test_within_budget() {
        let backend = MockOllamaClient::new();
        let (p50, p95) = speed_test(&backend, 20).await.unwrap();
        println!("L0 Tab latency: p50={p50:.1}ms p95={p95:.1}ms");
        assert!(p95 < 300.0, "L0 Tab budget exceeded: p95={p95:.1}ms");
    }

    #[test]
    fn rule_derives_fn_main_candidate() {
        let c = derive_rule_candidates("fn main(", "}", "rust");
        assert_eq!(c[0].text, ") {\n    \n}");
        assert!((c[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn rule_has_fallback_candidate() {
        let c = derive_rule_candidates("x", "", "rust");
        assert!(c.len() >= 2);
        assert!(c.iter().any(|x| x.text == "// fallback"));
        // Best rule candidate for "x" repeats the last token.
        assert_eq!(rule_complete("x word", "", "rust").text, "word_suffix");
    }
}
