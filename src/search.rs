//! Web Search：Tavily 或自建 SearXNG。
//!
//! 大模型只看到一个 `web_search` 工具，具体供应商由配置决定。

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

use crate::config::SearchConfig;

pub enum Searcher {
    Tavily {
        http: reqwest::Client,
        api_key: String,
        max_results: u32,
    },
    Searxng {
        http: reqwest::Client,
        base_url: String,
        max_results: u32,
    },
    Disabled,
}

impl Searcher {
    pub fn from_config(cfg: &SearchConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5)))
            .build()
            .context("创建搜索 HTTP 客户端失败")?;

        match cfg.provider.as_str() {
            "tavily" => Ok(Self::Tavily {
                http,
                api_key: cfg.api_key.expose().to_string(),
                max_results: cfg.max_results.clamp(1, 10),
            }),
            "searxng" => Ok(Self::Searxng {
                http,
                base_url: cfg.base_url.trim_end_matches('/').to_string(),
                max_results: cfg.max_results.clamp(1, 10),
            }),
            "none" | "disabled" => Ok(Self::Disabled),
            other => bail!("未知的 search.provider: {other}（可选 tavily / searxng / none）"),
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// 若启用搜索，返回 OpenAI function 定义。
    pub fn tool_definition(&self) -> Option<serde_json::Value> {
        if !self.enabled() {
            return None;
        }
        Some(serde_json::json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "搜索互联网，获取实时或你不掌握的信息（新闻、天气、价格、赛事等）。输入简洁的搜索关键词。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "搜索关键词，中文或英文均可"
                        }
                    },
                    "required": ["query"]
                }
            }
        }))
    }

    pub async fn search(&self, query: &str) -> Result<String> {
        match self {
            Self::Tavily {
                http,
                api_key,
                max_results,
            } => tavily(http, api_key, *max_results, query).await,
            Self::Searxng {
                http,
                base_url,
                max_results,
            } => searxng(http, base_url, *max_results, query).await,
            Self::Disabled => Ok("搜索未启用。".into()),
        }
    }
}

async fn tavily(
    http: &reqwest::Client,
    api_key: &str,
    max_results: u32,
    query: &str,
) -> Result<String> {
    if api_key.is_empty() {
        bail!("缺少 TAVILY_API_KEY");
    }
    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "search_depth": "basic",
        "include_answer": true,
        "max_results": max_results,
    });
    let resp = http
        .post("https://api.tavily.com/search")
        .json(&body)
        .send()
        .await
        .context("请求 Tavily 失败")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "Tavily 返回 {status}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
    let parsed: TavilyResponse = serde_json::from_str(&text).context("解析 Tavily 响应失败")?;
    Ok(format_tavily(query, parsed))
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

fn format_tavily(query: &str, resp: TavilyResponse) -> String {
    let mut out = format!("搜索：{query}\n");
    if let Some(answer) = resp.answer.filter(|a| !a.trim().is_empty()) {
        out.push_str(&format!("摘要：{}\n", trim_chars(&answer, 400)));
    }
    for (i, r) in resp.results.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}（{}）\n{}\n",
            i + 1,
            r.title.trim(),
            r.url.trim(),
            trim_chars(r.content.trim(), 350)
        ));
    }
    if resp.results.is_empty() && out.lines().count() <= 1 {
        out.push_str("没有找到结果。");
    }
    trim_chars(&out, 2400)
}

async fn searxng(
    http: &reqwest::Client,
    base_url: &str,
    max_results: u32,
    query: &str,
) -> Result<String> {
    let resp = http
        .get(format!("{base_url}/search"))
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .context("请求 SearXNG 失败")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "SearXNG 返回 {status}: {}（确认实例开启了 format=json）",
            text.chars().take(200).collect::<String>()
        );
    }
    let parsed: SearxResponse = serde_json::from_str(&text).context("解析 SearXNG 响应失败")?;

    let mut out = format!("搜索：{query}\n");
    for (i, r) in parsed.results.iter().take(max_results as usize).enumerate() {
        out.push_str(&format!(
            "{}. {}（{}）\n{}\n",
            i + 1,
            r.title.trim(),
            r.url.trim(),
            trim_chars(r.content.trim(), 350)
        ));
    }
    if parsed.results.is_empty() {
        out.push_str("没有找到结果。");
    }
    Ok(trim_chars(&out, 2400))
}

#[derive(Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

fn trim_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}
