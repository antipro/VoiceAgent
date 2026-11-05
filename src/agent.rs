//! Agent 闭环：用户输入 → DeepSeek → （可选）工具调用 → 最终回复。

use anyhow::{bail, Result};
use serde_json::json;
use std::sync::Arc;

use crate::config::Config;
use crate::llm::{ChatMessage, LlmClient};
use crate::memory::{Role, Store};
use crate::search::Searcher;

/// 一轮问答的结果。
pub struct AgentReply {
    pub text: String,
    /// 模型调用了 `end_conversation`，播报完应回到 Idle。
    pub end_session: bool,
}

pub struct Agent {
    llm: LlmClient,
    search: Searcher,
    store: Arc<Store>,
    system_prompt: String,
    farewell: String,
    history_limit: usize,
    max_tool_rounds: usize,
    tools: serde_json::Value,
}

impl Agent {
    pub fn new(cfg: &Config, store: Arc<Store>) -> Result<Self> {
        let search = Searcher::from_config(&cfg.search, &cfg.llm)?;
        Ok(Self {
            llm: LlmClient::new(&cfg.llm)?,
            tools: build_tools(&search),
            search,
            store,
            system_prompt: cfg.assistant.system_prompt.clone(),
            farewell: cfg.assistant.farewell.clone(),
            history_limit: cfg.llm.history_limit.max(2),
            max_tool_rounds: cfg.llm.max_tool_rounds.max(1),
        })
    }

    pub async fn respond(&self, session_id: &str, user_text: &str) -> Result<AgentReply> {
        self.store
            .append(session_id, Role::User, user_text)
            .await
            .ok();

        let history = self
            .store
            .recent(session_id, self.history_limit)
            .await
            .unwrap_or_default();

        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(ChatMessage::system(self.system_prompt.clone()));
        messages.extend(history);

        let mut end_session = false;

        for _round in 0..self.max_tool_rounds {
            let reply = self.llm.complete(&messages, Some(&self.tools)).await?;

            let calls = reply.tool_calls.clone().unwrap_or_default();
            if calls.is_empty() {
                let text = reply.content.unwrap_or_default().trim().to_string();
                return self.finish(session_id, text, end_session).await;
            }

            messages.push(ChatMessage::assistant_tool_calls(calls.clone()));
            for call in calls {
                let result = match call.function.name.as_str() {
                    "end_conversation" => {
                        tracing::info!("模型决定结束会话");
                        end_session = true;
                        "会话已结束，请给用户一句简短的告别。".to_string()
                    }
                    "web_search" => {
                        let query = parse_query(&call.function.arguments);
                        if query.is_empty() {
                            "工具参数错误：缺少 query".to_string()
                        } else {
                            tracing::info!(query = %query, "调用搜索");
                            match self.search.search(&query).await {
                                Ok(text) => text,
                                Err(e) => {
                                    tracing::warn!("搜索失败: {e:#}");
                                    format!("搜索失败：{e}")
                                }
                            }
                        }
                    }
                    other => format!("未知工具：{other}"),
                };
                messages.push(ChatMessage::tool(call.id.clone(), result));
            }
        }

        // 工具轮数用尽，强制收尾。
        let reply = self.llm.complete(&messages, None).await?;
        let text = reply.content.unwrap_or_default().trim().to_string();
        self.finish(session_id, text, end_session).await
    }

    async fn finish(
        &self,
        session_id: &str,
        text: String,
        end_session: bool,
    ) -> Result<AgentReply> {
        let mut text = crate::util::strip_markdown(&text);
        if text.is_empty() {
            if end_session {
                text = self.farewell.clone();
            } else {
                bail!("模型返回空回复");
            }
        }
        self.store
            .append(session_id, Role::Assistant, &text)
            .await
            .ok();
        Ok(AgentReply { text, end_session })
    }
}

/// 搜索（可选）+ 结束会话，组装成 OpenAI 兼容的 tools 数组。
fn build_tools(search: &Searcher) -> serde_json::Value {
    let mut tools = Vec::new();
    if let Some(search_tool) = search.tool_definition() {
        tools.push(search_tool);
    }
    tools.push(serde_json::json!({
        "type": "function",
        "function": {
            "name": "end_conversation",
            "description": "当用户明确表示要结束对话时调用，例如再见、拜拜、没事了、先这样、结束吧。调用后助手会回到等待唤醒的状态。",
            "parameters": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }
    }));
    serde_json::Value::Array(tools)
}

fn parse_query(arguments: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
    value
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}
