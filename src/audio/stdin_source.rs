//! 开发模式：直接从标准输入读取文本，绕过麦克风与 ASR。

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

use crate::fsm::Event;

pub async fn run(tx: Sender<Event>) -> Result<()> {
    eprintln!("[dev] 输入一行文本并回车，模拟“听到”一句话；Ctrl-D 或 /quit 退出。");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/quit" {
            break;
        }
        if tx
            .send(Event::Heard {
                text: text.to_string(),
            })
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = tx.send(Event::Shutdown).await;
    Ok(())
}
