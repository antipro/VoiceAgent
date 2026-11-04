//! 端到端验证 Agent 闭环：模型返回 tool_calls → 调搜索 → 二次请求 → 最终回复。
//!
//! 用本地假服务器替代 DeepSeek 与 SearXNG，不需要任何密钥。

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use xiaomeng::agent::Agent;
use xiaomeng::config::{Config, Secret};
use xiaomeng::memory::{Role, Store};

#[tokio::test]
async fn tool_call_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let mut chat_bodies: Vec<String> = Vec::new();
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut socket).await;
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();

            if path.starts_with("/search") {
                respond(
                    &mut socket,
                    r#"{"results":[{"title":"北京天气","url":"https://example.com/w","content":"北京今天晴，25 摄氏度"}]}"#,
                )
                .await;
            } else {
                chat_bodies.push(body);
                if chat_bodies.len() == 1 {
                    respond(
                        &mut socket,
                        r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"web_search","arguments":"{\"query\":\"北京天气\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
                    )
                    .await;
                } else {
                    respond(
                        &mut socket,
                        r#"{"choices":[{"message":{"role":"assistant","content":"北京今天晴，25 摄氏度。"},"finish_reason":"stop"}],"usage":{"prompt_tokens":30,"completion_tokens":8}}"#,
                    )
                    .await;
                }
            }
        }
        chat_bodies
    });

    let dir = std::env::temp_dir().join(format!("xiaomeng-it-{}", uuid::Uuid::new_v4()));
    let mut cfg = Config::default();
    cfg.llm.base_url = format!("http://{addr}");
    cfg.llm.model = "test-model".into();
    cfg.llm.api_key = Secret("sk-test".into());
    cfg.llm.history_limit = 10;
    cfg.search.provider = "searxng".into();
    cfg.search.base_url = format!("http://{addr}");
    cfg.storage.path = dir.join("test.db");

    let store = Arc::new(Store::open(&cfg.storage.path).unwrap());
    let agent = Agent::new(&cfg, store.clone()).unwrap();

    let reply = agent.respond("session-1", "北京天气怎么样").await.unwrap();
    assert!(reply.contains("25"), "unexpected reply: {reply}");

    let history = store.recent("session-1", 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, "user");
    assert_eq!(history[1].role, "assistant");

    let bodies = server.await.unwrap();
    assert_eq!(bodies.len(), 2, "should call the model twice");
    assert!(
        bodies[1].contains("tool_call_id"),
        "second request must carry tool result: {}",
        bodies[1]
    );
    assert!(
        bodies[1].contains("北京今天晴"),
        "tool output must be forwarded: {}",
        bodies[1]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn persists_user_message_even_without_api_key() {
    let dir = std::env::temp_dir().join(format!("xiaomeng-it-{}", uuid::Uuid::new_v4()));
    let mut cfg = Config::default();
    cfg.storage.path = dir.join("test.db");
    cfg.llm.api_key = Secret(String::new());
    cfg.search.provider = "none".into();

    let store = Arc::new(Store::open(&cfg.storage.path).unwrap());
    let agent = Agent::new(&cfg, store.clone()).unwrap();

    assert!(agent.respond("s", "在吗").await.is_err());
    let history = store.recent("s", 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, Role::User.as_str());

    let _ = std::fs::remove_dir_all(&dir);
}

async fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];

    loop {
        let n = stream.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return (String::new(), String::new());
        }
        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    if key.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);

            let body_start = pos + 4;
            while buf.len() < body_start + content_length {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let end = (body_start + content_length).min(buf.len());
            let body = String::from_utf8_lossy(&buf[body_start..end]).to_string();
            return (head, body);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn respond(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}
