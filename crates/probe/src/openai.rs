//! OpenAI-compatible completion backend (aidea LLM backend 模块, 决策 #A).
//!
//! 该模块为 NES（代码补全）提供 **OpenAI 兼容** 的 [`CompletionBackend`]
//! 实现：[`OpenAiCompletionBackend`] 通过任意 OpenAI 兼容端点的
//! `/v1/chat/completions`（覆盖 DeepSeek / 通义 / 智谱 / OpenAI 等）生成补全
//! 候选。它复用了 [`crate::ollama`] 的 `build_generate_prompt` 来构造补全上下文，
//! 与 `OllamaClient` 共用同一套 `(prefix, suffix, language)` 语义，保证 NES 在
//! `ollama` 与 `openai` 两种 backend 下行为一致。
//!
//! 所有网络 I/O 隔离在 [`CompletionBackend::complete`] 之后，因此该 backend 与
//! `MockOllamaClient` 一样可在无模型环境下做纯逻辑单测（见模块底部测试）。API Key
//! 来自配置（aidea serve 启动时由 GUI 通过环境变量注入，前端不持久化）。

use crate::ollama::{build_generate_prompt, Candidate, CompletionBackend};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// OpenAI 兼容 backend 的配置。
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// 兼容端点 base url（包含 `/v1`，例如 `https://api.openai.com/v1` 或
    /// `https://api.deepseek.com/v1`）。实际请求会拼接 `/chat/completions`。
    pub base_url: String,
    /// API Key（Bearer 令牌）。本地网关不需要时可留空。
    pub api_key: String,
    /// 模型名，例如 `gpt-4o-mini`、`deepseek-chat`、`qwen-plus`。
    pub model: String,
    /// 单次请求超时，防止后端无响应时无限挂起。
    pub timeout: Duration,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            timeout: Duration::from_secs(60),
        }
    }
}

// ------------------------------ wire payloads ------------------------------

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    temperature: f32,
}

#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

// ------------------------------ backend ------------------------------------

/// OpenAI 兼容 NES 补全 backend：把 `(prefix, suffix, language)` 发往
/// `/v1/chat/completions`，将模型返回的整段文本作为单个补全候选。
///
/// 与 `OllamaClient` 不同，OpenAI 兼容接口没有原生的「补全中间 gap」语义，
/// 因此这里用 chat 接口，并在 system 提示中约束模型只输出填补 gap 的代码。
#[derive(Clone)]
pub struct OpenAiCompletionBackend {
    config: OpenAiConfig,
    http: reqwest::Client,
}

impl OpenAiCompletionBackend {
    /// 基于配置构造 backend。HTTP 客户端内置超时。
    pub fn new(config: OpenAiConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("valid reqwest client for OpenAiCompletionBackend");
        Self { config, http }
    }

    /// 拼接完整请求地址：`<base_url>/chat/completions`（去掉 base 结尾的斜杠）。
    fn completions_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }

    /// 真正发起一次补全请求（无重试，由上层编排负责兜底/降级）。
    async fn try_complete_once(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        let prompt = build_generate_prompt(prefix, suffix, language);
        let body = ChatRequest {
            model: self.config.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: "You are a code completion engine. Given a code prefix, suffix, \
and language, output ONLY the code that fills the gap between them. Do not repeat \
the prefix or suffix. No explanation, no markdown fences."
                        .to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: prompt,
                },
            ],
            stream: false,
            temperature: 0.1,
        };

        let mut builder = self
            .http
            .post(self.completions_url())
            .json(&body);
        // 仅在提供了 Key 时挂 Bearer 头（本地网关可免鉴权）。
        if !self.config.api_key.trim().is_empty() {
            builder = builder.bearer_auth(self.config.api_key.trim());
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("openai request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("openai http error: {e}"))?
            .json::<ChatResponse>()
            .await
            .map_err(|e| anyhow::anyhow!("invalid openai response: {e}"))?;

        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("openai returned no completion choice"))?;

        // 单候选，置信度 0.8（与 OllamaClient 对齐）。
        Ok(vec![Candidate {
            text,
            confidence: 0.8,
        }])
    }
}

#[async_trait]
impl CompletionBackend for OpenAiCompletionBackend {
    async fn complete(
        &self,
        prefix: &str,
        suffix: &str,
        language: &str,
    ) -> anyhow::Result<Vec<Candidate>> {
        self.try_complete_once(prefix, suffix, language).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单测：拼写完整请求地址（处理 base 结尾有无斜杠两种情形）。
    #[test]
    fn completions_url_handles_trailing_slash() {
        let b = OpenAiCompletionBackend::new(OpenAiConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            ..OpenAiConfig::default()
        });
        assert_eq!(
            b.completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        let b2 = OpenAiCompletionBackend::new(OpenAiConfig {
            base_url: "https://api.deepseek.com/v1/".to_string(),
            ..OpenAiConfig::default()
        });
        assert_eq!(
            b2.completions_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    /// 单测：构造的请求体字段正确（通过 serialize 校验）。
    #[test]
    fn request_body_serializes() {
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: "sys".to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: "LANGUAGE:rust".to_string(),
                },
            ],
            stream: false,
            temperature: 0.1,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"model\":\"deepseek-chat\""));
        assert!(s.contains("\"stream\":false"));
        assert!(s.contains("\"temperature\":0.1"));
    }

    /// 单测：解析一个标准 OpenAI 响应为候选。
    #[test]
    fn parse_response_into_candidate() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"  let x = 1;\n"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let cand = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .unwrap();
        assert_eq!(cand, "let x = 1;");
    }

    /// 单测：空 Key 时构造不应 panic（鉴权头在请求时按空跳过）。
    #[test]
    fn builds_without_api_key() {
        let _b = OpenAiCompletionBackend::new(OpenAiConfig::default());
    }
}
