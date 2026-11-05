//! 开发模式：从终端读取文本，绕过麦克风与 ASR。
//!
//! 用 rustyline 而不是直接读 stdin：内核的 tty 行规程按「字节」处理退格，
//! 一个汉字是 3 个 UTF-8 字节，按退格只会删掉 1 字节、光标退 1 列，
//! 中英文混排时体验很差。rustyline 自带行编辑器，按字符/显示宽度处理。

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use tokio::sync::mpsc::Sender;

use crate::fsm::Event;

pub async fn run(tx: Sender<Event>) -> Result<()> {
    let input = tx.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut editor = DefaultEditor::new()?;
        loop {
            match editor.readline("你说> ") {
                Ok(line) => {
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if text == "/quit" {
                        break;
                    }
                    let _ = editor.add_history_entry(text);
                    if input
                        .blocking_send(Event::Heard {
                            text: text.to_string(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    })
    .await;

    // 无论正常退出还是读输入出错，都要通知状态机收尾，否则进程会一直等下去。
    let _ = tx.send(Event::Shutdown).await;

    match result {
        Ok(inner) => inner,
        Err(join) => Err(anyhow::anyhow!("输入线程异常退出: {join}")),
    }
}
