//! NES probe — local small-model completion probe for L0 Tab completion (T04).
//!
//! Provides:
//!   * [`CompletionBackend`] — a probe that turns a `(prefix, suffix)` context
//!     into candidate completions. [`OllamaClient`] is the real local-model
//!     backend; [`MockOllamaClient`] is a deterministic stand-in for tests / CI;
//!     [`NesClient`] is the productionized wrapper (cache + degradation +
//!     batch).
//!   * [`CompletionProvider`] — an LSP-style completion hook interface that turns
//!     a document context into ranked completion items.
//!   * [`rank_completions`] / [`derive_rule_candidates`] — pure ranking / rule
//!     logic (unit-tested).
//!   * [`speed_test`] — a latency harness asserting the L0 budget (<300ms).
//!
//! All network I/O is isolated behind [`CompletionBackend`], so the mock and the
//! wrappers are fully unit-testable with no model present.

pub mod completion;
pub mod ollama;
pub mod openai;

pub use completion::{
    derive_rule_candidates, rank_completions, rule_complete, CompletionItem, CompletionProvider,
    LspContext, ProbeCompletionProvider, speed_test,
};
pub use ollama::{
    BatchItem, CachedCompletionBackend, Candidate, CompletionBackend, CompletionCache,
    DegradingBackend, EmbeddingRequest, EmbeddingResponse, GenerateRequest, GenerateResponse,
    MockOllamaClient, NesClient, OllamaClient, OllamaConfig, RuleBasedBackend, run_batch,
};
pub use openai::{OpenAiCompletionBackend, OpenAiConfig};
