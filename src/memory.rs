//! SQLite 会话持久化。
//!
//! rusqlite 是阻塞 API，统一丢到 `spawn_blocking` 里执行。

use anyhow::{Context, Result};
use chrono::{Local, SecondsFormat};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::llm::ChatMessage;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chat_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    timestamp TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chat_history_session ON chat_history(session_id, id);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建数据目录失败: {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)
            .context("初始化 chat_history 表失败")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn append(&self, session_id: &str, role: Role, content: &str) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        let content = content.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let c = conn.lock().map_err(|_| anyhow::anyhow!("数据库锁中毒"))?;
            c.execute(
                "INSERT INTO chat_history (session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, role.as_str(), content, now_iso()],
            )?;
            Ok(())
        })
        .await
        .context("数据库任务失败")??;
        Ok(())
    }

    /// 取该会话最近 `limit` 条消息，按时间正序返回。
    pub async fn recent(&self, session_id: &str, limit: usize) -> Result<Vec<ChatMessage>> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
            let c = conn.lock().map_err(|_| anyhow::anyhow!("数据库锁中毒"))?;
            let mut stmt = c.prepare(
                "SELECT role, content FROM chat_history WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let iter = stmt.query_map(params![session_id, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for r in iter {
                out.push(r?);
            }
            out.reverse();
            Ok(out)
        })
        .await
        .context("数据库任务失败")??;

        Ok(rows
            .into_iter()
            .map(|(role, content)| ChatMessage {
                role,
                content: Some(content),
                tool_calls: None,
                tool_call_id: None,
            })
            .collect())
    }

    pub async fn message_count(&self) -> Result<i64> {
        let conn = self.conn.clone();
        let n = tokio::task::spawn_blocking(move || -> Result<i64> {
            let c = conn.lock().map_err(|_| anyhow::anyhow!("数据库锁中毒"))?;
            let n = c.query_row("SELECT COUNT(*) FROM chat_history", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
        .context("数据库任务失败")??;
        Ok(n)
    }
}

fn now_iso() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_and_reads_history() {
        let dir = std::env::temp_dir().join(format!("xiaomeng-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("test.db");
        let store = Store::open(&path).unwrap();

        store.append("s1", Role::User, "你好").await.unwrap();
        store.append("s1", Role::Assistant, "我在").await.unwrap();
        store.append("s2", Role::User, "别的会话").await.unwrap();

        let msgs = store.recent("s1", 10).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.as_deref(), Some("你好"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(store.message_count().await.unwrap(), 3);

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
