//! OpenAI 兼容的对话补全客户端（DeepSeek / 任何兼容网关）。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::LlmConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self::plain("system", text)
    }

    pub fn tool(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }

    fn plain(role: &str, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
}

pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    temperature: f32,
    max_tokens: u32,
}

impl LlmClient {
    pub fn new(cfg: &LlmConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5)))
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.expose().to_string(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
        })
    }

    fn endpoint(&self) -> String {
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    /// 发起一次补全，返回助手消息（可能包含 tool_calls）。
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&serde_json::Value>,
    ) -> Result<ChatMessage> {
        if self.api_key.is_empty() {
            bail!("缺少 DeepSeek API Key（设置 DEEPSEEK_API_KEY）");
        }

        let body = ChatRequest {
            model: &self.model,
            messages,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stream: false,
            tools,
            tool_choice: tools.map(|_| "auto"),
        };

        let mut last_err = None;
        for attempt in 0..3u32 {
            let resp = self
                .http
                .post(self.endpoint())
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        let parsed: ChatResponse =
                            serde_json::from_str(&text).with_context(|| {
                                format!("解析模型响应失败: {}", truncate(&text, 300))
                            })?;
                        if let Some(usage) = &parsed.usage {
                            tracing::debug!(
                                prompt = usage.prompt_tokens,
                                completion = usage.completion_tokens,
                                "token 用量"
                            );
                        }
                        let choice = parsed
                            .choices
                            .into_iter()
                            .next()
                            .ok_or_else(|| anyhow::anyhow!("模型没有返回 choices"))?;
                        if let Some(reason) = choice.finish_reason {
                            if reason == "length" {
                                tracing::warn!("回复被 max_tokens 截断");
                            }
                        }
                        return Ok(choice.message);
                    }

                    let detail = serde_json::from_str::<ApiErrorBody>(&text)
                        .map(|e| e.error.message)
                        .unwrap_or_else(|_| truncate(&text, 300));
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    tracing::warn!(%status, %detail, "模型请求失败");
                    if retryable && attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(800 * (1 << attempt))).await;
                        last_err = Some(anyhow::anyhow!("{status}: {detail}"));
                        continue;
                    }
                    bail!("模型请求失败（{status}）: {detail}");
                }
                Err(e) => {
                    tracing::warn!("网络错误: {e}");
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(800 * (1 << attempt))).await;
                        last_err = Some(anyhow::anyhow!("{e}"));
                        continue;
                    }
                    return Err(e).context("请求 DeepSeek 失败");
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("模型请求失败")))
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_assistant_tool_calls_without_content() {
        let msg = ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "web_search".into(),
                arguments: "{\"query\":\"天气\"}".into(),
            },
        }]);
        let v = serde_json::to_value(&msg).unwrap();
        assert!(v.get("content").is_none());
        assert_eq!(v["tool_calls"][0]["type"], "function");
    }

    #[test]
    fn deserializes_null_content() {
        let msg: ChatMessage =
            serde_json::from_str(r#"{"role":"assistant","content":null}"#).unwrap();
        assert!(msg.content.is_none());
    }
}
