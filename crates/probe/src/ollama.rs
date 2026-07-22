//! Ollama local-model completion backend (T04 / T08 productionization).
//!
//! T04 shipped a [`MockOllamaClient`] (deterministic stand-in) and a real
//! [`OllamaClient`] stub calling `/api/generate`. T08 *productionizes* the real
//! client and wraps it into [`NesClient`] — the backend selected for private
//! deployment:
//!
//!   * **Real endpoints** — `/api/generate` (completion) and `/api/embeddings`
//!     (vectorization), built with pure request/response (de)serialization
//!     helpers that are unit-tested without any network.
//!   * **Timeout + exponential backoff retry** — every request is bounded by a
//!     per-request timeout and retried with `compute_backoff_ms` (pure,
//!     testable). Keeps the L0 Tab p95 < 300ms SLA under transient failures.
//!   * **In-memory cache** — [`CompletionCache`] + [`CachedCompletionBackend`]
//!     cut p95 for repeated prefixes.
//!   * **Degradation** — [`DegradingBackend`] + [`RuleBasedBackend`] provide the
//!     rule-based fallback path: on any primary failure the client still returns
//!     a completion, preserving the latency budget (never returns an error to
//!     the caller on the hot path).
//!   * **Batch inference** — [`run_batch`] runs many completions with bounded
//!     concurrency, preserving input order.
//!
//! All network I/O is isolated behind [`CompletionBackend`], so the mock and the
//! wrappers are fully unit-testable with no model present (CI / offline).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// A raw completion candidate returned by a probe backend.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub text: String,
    /// Model-assigned confidence in [0,1].
    pub confidence: f32,
}

/// Backend contract for producing completion candidates.
#[async_trait]
pub trait CompletionBackend: Send + Sync {
    /// Produce candidates for the given `(prefix, suffix)` context.
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>>;
}

/// Configuration for the Ollama backend (T08: added retry/backoff/cache/batch).
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    pub endpoint: String,
    pub model: String,
    /// Per-request timeout; tuned under the L0 300ms budget.
    pub timeout: Duration,
    /// Model used for `/api/embeddings`.
    pub embed_model: String,
    /// Max retry attempts on transient failure (0 = no retry).
    pub max_retries: u32,
    /// Base backoff in ms for exponential retry (attempt 0 => base).
    pub backoff_base_ms: u64,
    /// Upper bound for backoff in ms (exponential growth is capped here).
    pub backoff_max_ms: u64,
    /// Completion cache capacity (FIFO eviction when exceeded).
    pub cache_capacity: usize,
    /// Max concurrent requests for batch inference.
    pub batch_concurrency: usize,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:11434".to_string(),
            model: "nes-tab:latest".to_string(),
            timeout: Duration::from_millis(280),
            embed_model: "nomic-embed-text".to_string(),
            max_retries: 2,
            backoff_base_ms: 50,
            backoff_max_ms: 500,
            cache_capacity: 1024,
            batch_concurrency: 4,
        }
    }
}

// ------------------------------ wire payloads ------------------------------

#[derive(Serialize)]
pub struct GenerateRequest {
    model: String,
    prompt: String,
    stream: bool,
}

#[derive(Deserialize)]
pub struct GenerateResponse {
    response: String,
}

#[derive(Serialize)]
pub struct EmbeddingRequest {
    model: String,
    prompt: String,
}

#[derive(Deserialize)]
pub struct EmbeddingResponse {
    embedding: Vec<f32>,
}

// ------------------------------ pure helpers -------------------------------
// Kept free of I/O and `reqwest` so they are trivially unit-testable.

/// Build the prompt sent to the model (tab-completion framing).
pub fn build_generate_prompt(prefix: &str, suffix: &str, language: &str) -> String {
    format!("LANGUAGE:{language}\nPREFIX:{prefix}\nSUFFIX:{suffix}\nCOMPLETE_THE_GAP:")
}

/// Parse a `/api/generate` JSON response. Pure.
pub fn parse_generate_response_json(s: &str) -> anyhow::Result<GenerateResponse> {
    serde_json::from_str(s).map_err(|e| anyhow::anyhow!("invalid generate response: {e}"))
}

/// Build an `/api/embeddings` request body. Pure.
pub fn build_embedding_request(model: &str, prompt: &str) -> EmbeddingRequest {
    EmbeddingRequest {
        model: model.to_string(),
        prompt: prompt.to_string(),
    }
}

/// Parse an `/api/embeddings` JSON response. Pure.
pub fn parse_embedding_response_json(s: &str) -> anyhow::Result<EmbeddingResponse> {
    serde_json::from_str(s).map_err(|e| anyhow::anyhow!("invalid embedding response: {e}"))
}

/// Exponential backoff in milliseconds, capped at `max_ms`. Pure & testable.
///
/// `attempt` is 0-based: attempt 0 => `base_ms`, attempt 1 => `base_ms * 2`, …
/// Overflow is guarded by `min(30)` on the shift and `min(max_ms)` on the sum.
pub fn compute_backoff_ms(attempt: u32, base_ms: u64, max_ms: u64) -> u64 {
    let exp = base_ms.saturating_mul(1u64 << attempt.min(30));
    exp.min(max_ms)
}

/// Whether another retry should be attempted (`attempt` is 0-based).
pub fn should_retry(attempt: u32, max_retries: u32) -> bool {
    attempt < max_retries
}

// ------------------------------ real client --------------------------------

/// Real Ollama backend (local small model). Calls `/api/generate` with a
/// per-request timeout and exponential backoff retry; `/api/embeddings` for
/// vectorization. Network calls are isolated here; [`NesClient`] adds the
/// cache / degradation / batch orchestration.
#[derive(Clone)]
pub struct OllamaClient {
    config: OllamaConfig,
    http: reqwest::Client,
}

impl OllamaClient {
    pub fn new(config: OllamaConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("valid reqwest client");
        Self { config, http }
    }

    /// Vectorize `text` via the Ollama `/api/embeddings` endpoint (T08).
    pub async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let body = build_embedding_request(&self.config.embed_model, text);
        let resp = self
            .http
            .post(format!("{}/api/embeddings", self.config.endpoint))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<EmbeddingResponse>()
            .await?;
        Ok(resp.embedding)
    }

    /// One unprotected attempt; callers wrap this with timeout + retry.
    async fn try_complete_once(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        let body = GenerateRequest {
            model: self.config.model.clone(),
            prompt: build_generate_prompt(prefix, suffix, language),
            stream: false,
        };
        let resp = tokio::time::timeout(
            self.config.timeout,
            self.http
                .post(format!("{}/api/generate", self.config.endpoint))
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("ollama request timeout after {:?}", self.config.timeout))?
        .map_err(|e| anyhow::anyhow!("ollama http error: {e}"))?
        .error_for_status()?
        .json::<GenerateResponse>()
        .await?;
        Ok(vec![Candidate {
            text: resp.response.trim().to_string(),
            confidence: 0.8,
        }])
    }
}

#[async_trait]
impl CompletionBackend for OllamaClient {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        let mut attempt: u32 = 0;
        loop {
            match self.try_complete_once(prefix, suffix, language).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    if !should_retry(attempt, self.config.max_retries) {
                        return Err(e);
                    }
                    let backoff =
                        compute_backoff_ms(attempt, self.config.backoff_base_ms, self.config.backoff_max_ms);
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    attempt += 1;
                }
            }
        }
    }
}

// ------------------------------ mock / rule --------------------------------

/// Deterministic stand-in backend used for demos, unit tests and the speed
/// harness. Returns canned candidates with stable confidence so ranking and
/// latency are reproducible without a model. Shares its rule with
/// [`RuleBasedBackend`] via [`crate::completion::derive_rule_candidates`].
pub struct MockOllamaClient;

impl MockOllamaClient {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockOllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CompletionBackend for MockOllamaClient {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        Ok(crate::completion::derive_rule_candidates(prefix, suffix, language))
    }
}

/// Deterministic rule engine used as the NES production degradation path (T08).
/// Identical output to [`MockOllamaClient`] but named for its production role,
/// so the degradation wiring reads clearly.
pub struct RuleBasedBackend;

#[async_trait]
impl CompletionBackend for RuleBasedBackend {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        Ok(crate::completion::derive_rule_candidates(prefix, suffix, language))
    }
}

// ------------------------------ cache --------------------------------------

/// Bounded in-memory completion cache (FIFO eviction by insertion order).
/// Keyed by `(prompt, language)`; stores ranked candidates. Used by
/// [`CachedCompletionBackend`] to cut p95 latency for repeated prefixes.
#[derive(Debug, Default)]
pub struct CompletionCache {
    capacity: usize,
    order: VecDeque<(String, String)>,
    map: HashMap<(String, String), Vec<Candidate>>,
}

impl CompletionCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    pub fn get(&self, prompt: &str, language: &str) -> Option<&Vec<Candidate>> {
        self.map.get(&(prompt.to_string(), language.to_string()))
    }

    pub fn insert(&mut self, prompt: &str, language: &str, value: Vec<Candidate>) {
        let key = (prompt.to_string(), language.to_string());
        if self.map.contains_key(&key) {
            // Refresh value in place; order unchanged.
            self.map.insert(key, value);
            return;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Wraps a backend with an in-memory cache. A second identical call (same
/// `prefix` + `language`) is served from cache without hitting the model.
pub struct CachedCompletionBackend {
    inner: Arc<dyn CompletionBackend>,
    cache: Mutex<CompletionCache>,
}

impl CachedCompletionBackend {
    pub fn new(inner: Arc<dyn CompletionBackend>, capacity: usize) -> Self {
        Self {
            inner,
            cache: Mutex::new(CompletionCache::new(capacity)),
        }
    }

    /// Read-only access to the underlying cache (used by unit tests).
    pub fn cache(&self) -> &Mutex<CompletionCache> {
        &self.cache
    }
}

#[async_trait]
impl CompletionBackend for CachedCompletionBackend {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        {
            let cache = self.cache.lock().unwrap();
            if let Some(hit) = cache.get(prefix, language) {
                return Ok(hit.clone());
            }
        }
        let res = self.inner.complete(prefix, suffix, language).await?;
        self.cache.lock().unwrap().insert(prefix, language, res.clone());
        Ok(res)
    }
}

// ------------------------------ degradation --------------------------------

/// Tries `primary`; on any error falls back to `fallback` (T08 degradation).
/// Never returns the primary's error to the caller — the fallback always
/// produces a completion (e.g. rule-based), preserving the L0 latency budget.
pub struct DegradingBackend {
    primary: Arc<dyn CompletionBackend>,
    fallback: Arc<dyn CompletionBackend>,
}

impl DegradingBackend {
    pub fn new(
        primary: Arc<dyn CompletionBackend>,
        fallback: Arc<dyn CompletionBackend>,
    ) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl CompletionBackend for DegradingBackend {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        match self.primary.complete(prefix, suffix, language).await {
            Ok(c) => Ok(c),
            Err(_) => self.fallback.complete(prefix, suffix, language).await,
        }
    }
}

// ------------------------------ batch --------------------------------------

/// One item for batch completion (T08 concurrent inference).
#[derive(Debug, Clone)]
pub struct BatchItem {
    pub prefix: String,
    pub suffix: String,
    pub language: String,
}

/// Run completions for many items with bounded concurrency, preserving input
/// order in the returned vector. Pure with respect to any single backend — a
/// mock backend makes this unit-testable.
pub async fn run_batch(
    backend: Arc<dyn CompletionBackend>,
    items: &[BatchItem],
    concurrency: usize,
) -> Vec<anyhow::Result<Vec<Candidate>>> {
    let concurrency = concurrency.max(1);
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut handles = Vec::with_capacity(items.len());
    for item in items {
        // Acquire a permit before spawning; the (concurrency+1)-th acquire blocks
        // until a prior task finishes and drops its permit, bounding concurrency.
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let backend = backend.clone();
        let prefix = item.prefix.clone();
        let suffix = item.suffix.clone();
        let language = item.language.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit; // held until this future completes
            backend.complete(&prefix, &suffix, &language).await
        });
        handles.push(handle);
    }
    let mut out = Vec::with_capacity(items.len());
    for h in handles {
        out.push(h.await.expect("batch task panicked"));
    }
    out
}

// ------------------------------ production client --------------------------

/// Productionized NES client (T08): a real [`OllamaClient`] wrapped with an
/// in-memory cache, a rule-based degradation fallback, and bounded-concurrency
/// batch inference. This is the backend selected by `CoreConfig.nes_backend =
/// "ollama"` (T09/T10).
pub struct NesClient {
    real: Arc<OllamaClient>,
    as_backend: Arc<dyn CompletionBackend>,
    cache: Mutex<CompletionCache>,
    fallback: Arc<dyn CompletionBackend>,
    batch_concurrency: usize,
}

impl NesClient {
    pub fn from_config(cfg: &OllamaConfig) -> Self {
        let real = Arc::new(OllamaClient::new(cfg.clone()));
        let as_backend: Arc<dyn CompletionBackend> = real.clone();
        let fallback: Arc<dyn CompletionBackend> = Arc::new(RuleBasedBackend);
        Self {
            real,
            as_backend,
            cache: Mutex::new(CompletionCache::new(cfg.cache_capacity)),
            fallback,
            batch_concurrency: cfg.batch_concurrency,
        }
    }

    /// Embedding via the Ollama `/api/embeddings` endpoint.
    pub async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        self.real.embed(text).await
    }

    /// Batch completion with bounded concurrency (delegates to [`run_batch`]).
    pub async fn complete_batch(
        &self,
        items: &[BatchItem],
    ) -> Vec<anyhow::Result<Vec<Candidate>>> {
        run_batch(self.as_backend.clone(), items, self.batch_concurrency).await
    }
}

#[async_trait]
impl CompletionBackend for NesClient {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        // 1) cache hit
        {
            let cache = self.cache.lock().unwrap();
            if let Some(hit) = cache.get(prefix, language) {
                return Ok(hit.clone());
            }
        }
        // 2) real backend (with its own timeout + retry/backoff)
        match self.real.complete(prefix, suffix, language).await {
            Ok(cands) => {
                self.cache.lock().unwrap().insert(prefix, language, cands.clone());
                Ok(cands)
            }
            // 3) degradation: rule-based fallback (always succeeds)
            Err(_) => self.fallback.complete(prefix, suffix, language).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Counting backend used to assert cache / batch behaviour.
    struct CountingBackend {
        calls: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl CompletionBackend for CountingBackend {
        async fn complete(
            &self,
            prefix: &str,
            _suffix: &str,
            _language: &str,
        ) -> anyhow::Result<Vec<Candidate>> {
            *self.calls.lock().unwrap() += 1;
            Ok(vec![Candidate {
                text: prefix.to_string(),
                confidence: 0.5,
            }])
        }
    }

    /// Backend that always fails — drives the degradation path.
    struct ErrBackend;
    #[async_trait]
    impl CompletionBackend for ErrBackend {
        async fn complete(
            &self,
            _prefix: &str,
            _suffix: &str,
            _language: &str,
        ) -> anyhow::Result<Vec<Candidate>> {
            anyhow::bail!("boom")
        }
    }

    #[test]
    fn build_generate_prompt_contains_context() {
        let p = build_generate_prompt("fn main(", "}", "rust");
        assert!(p.contains("LANGUAGE:rust"));
        assert!(p.contains("PREFIX:fn main("));
        assert!(p.contains("SUFFIX:}"));
    }

    #[test]
    fn parse_generate_response_json_ok() {
        let r = parse_generate_response_json(r#"{"response":"hello"}"#).unwrap();
        assert_eq!(r.response, "hello");
    }

    #[test]
    fn build_and_parse_embedding() {
        let req = build_embedding_request("nomic", "some text");
        assert_eq!(req.model, "nomic");
        assert_eq!(req.prompt, "some text");
        let r = parse_embedding_response_json(r#"{"embedding":[1.0,2.0,3.0]}"#).unwrap();
        assert_eq!(r.embedding, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(compute_backoff_ms(0, 50, 500), 50);
        assert_eq!(compute_backoff_ms(1, 50, 500), 100);
        assert_eq!(compute_backoff_ms(2, 50, 500), 200);
        assert_eq!(compute_backoff_ms(3, 50, 500), 400);
        assert_eq!(compute_backoff_ms(4, 50, 500), 500); // capped
        assert_eq!(compute_backoff_ms(10, 50, 500), 500); // capped
    }

    #[test]
    fn retry_policy() {
        assert!(should_retry(0, 2));
        assert!(should_retry(1, 2));
        assert!(!should_retry(2, 2));
        assert!(!should_retry(3, 2));
    }

    #[test]
    fn cache_insert_get_and_fifo_eviction() {
        let mut c = CompletionCache::new(2);
        c.insert("a", "rust", vec![Candidate { text: "A".into(), confidence: 1.0 }]);
        c.insert("b", "rust", vec![Candidate { text: "B".into(), confidence: 1.0 }]);
        assert_eq!(c.len(), 2);
        assert!(c.get("a", "rust").is_some());
        // Third insert evicts the oldest ("a").
        c.insert("c", "rust", vec![Candidate { text: "C".into(), confidence: 1.0 }]);
        assert_eq!(c.len(), 2);
        assert!(c.get("a", "rust").is_none());
        assert!(c.get("c", "rust").is_some());
    }

    #[tokio::test]
    async fn cached_backend_serves_second_call_from_cache() {
        let calls = Arc::new(Mutex::new(0u32));
        let inner = Arc::new(CountingBackend { calls: calls.clone() });
        let cached = CachedCompletionBackend::new(inner, 16);
        let _ = cached.complete("x", "", "rust").await.unwrap();
        let _ = cached.complete("x", "", "rust").await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "second call must hit the cache");
    }

    #[tokio::test]
    async fn degrading_backend_falls_back_on_error() {
        let primary = Arc::new(ErrBackend);
        let fallback = Arc::new(RuleBasedBackend);
        let degraded = DegradingBackend::new(primary, fallback);
        let res = degraded.complete("fn main(", "}", "rust").await.unwrap();
        assert!(!res.is_empty(), "degradation must still yield a candidate");
        // The rule engine's `fn main(` candidate has confidence 0.9.
        assert!(res.iter().any(|c| (c.confidence - 0.9).abs() < 1e-6));
    }

    #[tokio::test]
    async fn rule_based_backend_returns_candidates() {
        let b = RuleBasedBackend;
        let res = b.complete("fn main(", "}", "rust").await.unwrap();
        assert!(res.len() >= 2);
        assert!(res.iter().any(|c| c.text == "// fallback"));
    }

    #[tokio::test]
    async fn batch_runs_all_items_in_order_with_concurrency() {
        let calls = Arc::new(Mutex::new(0u32));
        let backend = Arc::new(CountingBackend { calls: calls.clone() });
        let items: Vec<BatchItem> = (0..5)
            .map(|i| BatchItem {
                prefix: format!("p{i}"),
                suffix: String::new(),
                language: "rust".to_string(),
            })
            .collect();
        let out = run_batch(backend, &items, 2).await;
        assert_eq!(out.len(), 5);
        assert_eq!(*calls.lock().unwrap(), 5);
        // Order preserved: item i echoes prefix "p{i}".
        for (i, r) in out.iter().enumerate() {
            let cands = r.as_ref().unwrap();
            assert_eq!(cands[0].text, format!("p{i}"));
        }
    }

    #[test]
    fn nes_client_constructs_without_network() {
        // Construction only builds clients; no request is made.
        let _client = NesClient::from_config(&OllamaConfig::default());
    }

    #[tokio::test]
    async fn nes_client_degrades_to_rule_when_ollama_unreachable() {
        // The production `NesClient` must preserve the L0 latency budget: when
        // the real Ollama endpoint is unreachable it must fall back to the
        // rule-based backend and still return a candidate (never surface the
        // primary error on the hot path). Uses an unroutable endpoint + zero
        // retries so it fails fast and deterministically.
        let mut cfg = OllamaConfig::default();
        cfg.endpoint = "http://127.0.0.1:1".to_string(); // connection refused
        cfg.max_retries = 0;
        cfg.timeout = Duration::from_millis(50);
        let client = NesClient::from_config(&cfg);
        let cands = client
            .complete("fn main(", "}", "rust")
            .await
            .expect("NesClient must degrade, never surface the primary error");
        assert!(
            !cands.is_empty(),
            "degradation must still yield a candidate"
        );
        // The rule-based fallback for `fn main(` returns confidence 0.9.
        assert!(
            cands.iter().any(|c| (c.confidence - 0.9).abs() < 1e-6),
            "degradation should return the rule candidate for `fn main(`"
        );
    }
}
