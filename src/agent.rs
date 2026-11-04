//! Agent 闭环：用户输入 → DeepSeek → （可选）工具调用 → 最终回复。

use anyhow::{bail, Result};
use serde_json::json;
use std::sync::Arc;

use crate::config::Config;
use crate::llm::{ChatMessage, LlmClient};
use crate::memory::{Role, Store};
use crate::search::Searcher;

pub struct Agent {
    llm: LlmClient,
    search: Searcher,
    store: Arc<Store>,
    system_prompt: String,
    history_limit: usize,
    max_tool_rounds: usize,
    tool_definition: Option<serde_json::Value>,
}

impl Agent {
    pub fn new(cfg: &Config, store: Arc<Store>) -> Result<Self> {
        let search = Searcher::from_config(&cfg.search)?;
        Ok(Self {
            llm: LlmClient::new(&cfg.llm)?,
            tool_definition: search.tool_definition(),
            search,
            store,
            system_prompt: cfg.assistant.system_prompt.clone(),
            history_limit: cfg.llm.history_limit.max(2),
            max_tool_rounds: cfg.llm.max_tool_rounds.max(1),
        })
    }

    pub async fn respond(&self, session_id: &str, user_text: &str) -> Result<String> {
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

        for _round in 0..self.max_tool_rounds {
            let reply = self
                .llm
                .complete(&messages, self.tool_definition.as_ref())
                .await?;

            let calls = reply.tool_calls.clone().unwrap_or_default();
            if calls.is_empty() {
                let text = reply.content.unwrap_or_default().trim().to_string();
                return self.finish(session_id, text).await;
            }

            messages.push(ChatMessage::assistant_tool_calls(calls.clone()));
            for call in calls {
                let query = parse_query(&call.function.arguments);
                let result = if query.is_empty() {
                    "工具参数错误：缺少 query".to_string()
                } else {
                    tracing::info!(tool = %call.function.name, query = %query, "调用搜索");
                    match self.search.search(&query).await {
                        Ok(text) => text,
                        Err(e) => {
                            tracing::warn!("搜索失败: {e:#}");
                            format!("搜索失败：{e}")
                        }
                    }
                };
                messages.push(ChatMessage::tool(call.id.clone(), result));
            }
        }

        // 工具轮数用尽，强制收尾。
        let reply = self.llm.complete(&messages, None).await?;
        let text = reply.content.unwrap_or_default().trim().to_string();
        self.finish(session_id, text).await
    }

    async fn finish(&self, session_id: &str, text: String) -> Result<String> {
        let text = crate::util::strip_markdown(&text);
        if text.is_empty() {
            bail!("模型返回空回复");
        }
        self.store
            .append(session_id, Role::Assistant, &text)
            .await
            .ok();
        Ok(text)
    }
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
