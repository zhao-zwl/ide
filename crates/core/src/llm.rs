//! LLM capability used by the Planner (ReAct loop).
//!
//! [`Llm`] is intentionally tiny for M1: produce a "thought" and decide the
//! next action. Real implementations route to vLLM / Ollama via the Model
//! Gateway; [`MockLlm`] drives deterministic demos and unit tests.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A single reasoning step produced by the LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thought {
    pub text: String,
}

/// The next action the Planner should take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPlan {
    pub tool: String,
    pub argument: String,
}

/// LLM service contract required by the decision Core.
#[async_trait]
pub trait Llm: Send + Sync {
    /// Produce the next reasoning step given the running context.
    async fn think(&self, context: &str) -> anyhow::Result<Thought>;

    /// Decide which tool to call next, given a thought.
    async fn plan_action(&self, thought: &Thought) -> anyhow::Result<ActionPlan>;
}

/// Deterministic LLM used for demos / tests. It emits a canned thought and
/// routes to an `inspect` action, then `finish` once the goal marker appears.
pub struct MockLlm {
    goal_satisfied_marker: String,
}

impl Default for MockLlm {
    fn default() -> Self {
        Self::new()
    }
}

impl MockLlm {
    pub fn new() -> Self {
        Self {
            goal_satisfied_marker: "DONE".to_string(),
        }
    }
}

#[async_trait]
impl Llm for MockLlm {
    async fn think(&self, context: &str) -> anyhow::Result<Thought> {
        let text = if context.contains(&self.goal_satisfied_marker) {
            "Goal satisfied; finalizing.".to_string()
        } else {
            format!(
                "Inspecting context to progress toward goal. (ctx len={})",
                context.len()
            )
        };
        Ok(Thought { text })
    }

    async fn plan_action(&self, thought: &Thought) -> anyhow::Result<ActionPlan> {
        if thought.text.contains("finalizing") {
            Ok(ActionPlan {
                tool: "finish".to_string(),
                argument: String::new(),
            })
        } else {
            Ok(ActionPlan {
                tool: "inspect".to_string(),
                argument: "current_state".to_string(),
            })
        }
    }
}

// ===========================================================================
// 真实 LLM backend（决策 #A）：OllamaLlm / OpenAiLlm
//
// 二者都实现 [`Llm`] 契约，因此可被 [`Planner`](crate::planner::Planner) 与
// [`ChatEngine`](crate::chat::ChatEngine) 复用 —— 这是「端模型零配置对话」与
// 「在线厂商路由」的硬前置。
//
// 设计要点：
//   * `think(context)` 把上下文作为 user 消息发给真实 chat 模型，返回的文本内容
//     既作为 Chat 的助手回复，也作为 Quest 目标分解 / ReAct 推理的输入。
//   * `plan_action(thought)` 解析 `thought.text` 中可选的 `ACTION:` 指令行来驱动
//     ReAct 循环；缺失时返回空 action（Chat 场景不展示工具建议，ReAct 场景则
//     自然收敛；见 [`crate::chat::ChatEngine::reply`] 对空 tool 的跳过）。
// ===========================================================================

/// 解析 `thought.text` 里可选的 `ACTION:` 指令行，用于驱动 ReAct 循环。
///
/// 约定格式（单行）：`ACTION: <tool> <argument>`，例如 `ACTION: read_file src/main.rs`。
/// 没有该指令时返回 `None`（调用方据此回退为空 action）。
fn parse_action_directive(text: &str) -> Option<ActionPlan> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ACTION:") {
            let rest = rest.trim();
            if let Some((tool, arg)) = rest.split_once(char::is_whitespace) {
                return Some(ActionPlan {
                    tool: tool.trim().to_string(),
                    argument: arg.trim().to_string(),
                });
            } else if !rest.is_empty() {
                return Some(ActionPlan {
                    tool: rest.to_string(),
                    argument: String::new(),
                });
            }
        }
    }
    None
}

/// Ollama 本地模型 backend：通过 `/api/chat` 调用本地 Ollama 的真实推理。
///
/// 默认模型为 `nes-tab:latest`（由 qwen2.5:0.5b 顶替的 Modelfile 创建），与
/// `CoreConfig.model_name` 对齐。GUI 侧以 sidecar 拉起 Ollama 并 `ollama create
/// nes-tab` 后即可零配置对话。
pub struct OllamaLlm {
    endpoint: String,
    model: String,
    system_prompt: String,
    http: reqwest::Client,
}

impl OllamaLlm {
    /// 构造 Ollama backend。`endpoint` 为 Ollama 基址（默认 `http://localhost:11434`），
    /// `model` 为模型名（默认 `nes-tab:latest`）。
    pub fn new(endpoint: &str, model: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("valid reqwest client for OllamaLlm");
        Self {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            system_prompt: "你是由 aidea 集成的编程助手，内嵌于 IDE 中。请清晰、简洁地回答用户的问题，\
在合适处使用 Markdown。若用户用中文提问，用中文回答。"
                .to_string(),
            http,
        }
    }

    /// 拼接 `/api/chat` 地址。
    fn chat_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        format!("{base}/api/chat")
    }
}

#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct OllamaChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaChatMessage,
}

#[async_trait]
impl Llm for OllamaLlm {
    async fn think(&self, context: &str) -> anyhow::Result<Thought> {
        let req = OllamaChatRequest {
            model: self.model.clone(),
            messages: vec![
                OllamaChatMessage {
                    role: "system".to_string(),
                    content: self.system_prompt.clone(),
                },
                OllamaChatMessage {
                    role: "user".to_string(),
                    content: context.to_string(),
                },
            ],
            stream: false,
        };
        let resp = self
            .http
            .post(self.chat_url())
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("ollama chat request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("ollama http error: {e}"))?
            .json::<OllamaChatResponse>()
            .await
            .map_err(|e| anyhow::anyhow!("invalid ollama chat response: {e}"))?;
        Ok(Thought {
            text: resp.message.content.trim().to_string(),
        })
    }

    async fn plan_action(&self, thought: &Thought) -> anyhow::Result<ActionPlan> {
        Ok(parse_action_directive(&thought.text).unwrap_or_else(|| ActionPlan {
            tool: String::new(),
            argument: String::new(),
        }))
    }
}

/// OpenAI 兼容 backend：通过 `/v1/chat/completions` 调用任意 OpenAI 兼容厂商
/// （DeepSeek / 通义 / 智谱 / OpenAI 等）。API Key 来自配置（由 GUI 在 serve 启动
/// 时注入环境变量），进程内持有，前端不持久化、不日志。
pub struct OpenAiLlm {
    base_url: String,
    api_key: String,
    model: String,
    system_prompt: String,
    http: reqwest::Client,
}

impl OpenAiLlm {
    /// 构造 OpenAI 兼容 backend。`base_url` 含 `/v1`（如 `https://api.openai.com/v1`），
    /// `api_key` 为 Bearer 令牌（可空，用于免鉴权本地网关），`model` 为模型名。
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("valid reqwest client for OpenAiLlm");
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            system_prompt: "You are aidea, a coding assistant embedded in an IDE. Answer the \
user's question clearly and concisely, using Markdown when helpful. Reply in the \
same language the user writes in."
                .to_string(),
            http,
        }
    }

    /// 拼接 `/chat/completions` 地址。
    fn completions_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }
}

#[derive(Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiChatMessage>,
    stream: bool,
    temperature: f32,
}

#[derive(Serialize, Deserialize, Clone)]
struct OpenAiChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiChatMessage,
}

#[async_trait]
impl Llm for OpenAiLlm {
    async fn think(&self, context: &str) -> anyhow::Result<Thought> {
        let req = OpenAiChatRequest {
            model: self.model.clone(),
            messages: vec![
                OpenAiChatMessage {
                    role: "system".to_string(),
                    content: self.system_prompt.clone(),
                },
                OpenAiChatMessage {
                    role: "user".to_string(),
                    content: context.to_string(),
                },
            ],
            stream: false,
            temperature: 0.7,
        };

        let mut builder = self.http.post(self.completions_url()).json(&req);
        if !self.api_key.trim().is_empty() {
            builder = builder.bearer_auth(self.api_key.trim());
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("openai chat request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("openai http error: {e}"))?
            .json::<OpenAiChatResponse>()
            .await
            .map_err(|e| anyhow::anyhow!("invalid openai chat response: {e}"))?;

        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("openai returned no chat choice"))?;
        Ok(Thought { text })
    }

    async fn plan_action(&self, thought: &Thought) -> anyhow::Result<ActionPlan> {
        Ok(parse_action_directive(&thought.text).unwrap_or_else(|| ActionPlan {
            tool: String::new(),
            argument: String::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_directive_variants() {
        // 标准 `tool arg`
        let a = parse_action_directive("thinking...\nACTION: read_file src/main.rs\n").unwrap();
        assert_eq!(a.tool, "read_file");
        assert_eq!(a.argument, "src/main.rs");
        // 只有 tool
        let b = parse_action_directive("ACTION: finish").unwrap();
        assert_eq!(b.tool, "finish");
        assert!(b.argument.is_empty());
        // 无指令
        assert!(parse_action_directive("just a normal reply").is_none());
    }

    #[test]
    fn ollama_chat_url_handles_trailing_slash() {
        let llm = OllamaLlm::new("http://localhost:11434/", "nes-tab:latest");
        assert_eq!(llm.chat_url(), "http://localhost:11434/api/chat");
        let llm2 = OllamaLlm::new("http://localhost:11434", "nes-tab:latest");
        assert_eq!(llm2.chat_url(), "http://localhost:11434/api/chat");
    }

    #[test]
    fn openai_completions_url_handles_trailing_slash() {
        let llm = OpenAiLlm::new("https://api.openai.com/v1/", "gpt-4o-mini", "");
        assert_eq!(
            llm.completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }
}
