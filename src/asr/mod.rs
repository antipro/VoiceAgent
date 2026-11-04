//! ASR 引擎：默认调用 whisper.cpp 的命令行程序（whisper-cli）。

pub mod whisper_cli;

use anyhow::Result;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::config::AsrConfig;
use crate::fsm::Event;
use crate::util::clean_transcript;
use whisper_cli::WhisperCli;

pub enum Asr {
    Whisper(WhisperCli),
    /// 仅用于测试链路，不产生文本。
    Mock,
}

impl Asr {
    pub fn from_config(cfg: &AsrConfig) -> Result<Self> {
        match cfg.engine.as_str() {
            "whisper" => Ok(Self::Whisper(WhisperCli::new(cfg)?)),
            "mock" => Ok(Self::Mock),
            other => anyhow::bail!("未知的 asr.engine: {other}（可选 whisper / mock）"),
        }
    }

    pub async fn transcribe(&self, samples: &[f32]) -> Result<String> {
        match self {
            Self::Whisper(w) => w.transcribe(samples).await,
            Self::Mock => {
                if samples.is_empty() {
                    Ok(String::new())
                } else {
                    Ok("[mock-asr]".to_string())
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Whisper(w) => format!("whisper.cpp ({})", w.model().display()),
            Self::Mock => "mock".into(),
        }
    }
}

/// 后台任务：语句 → 文本 → 事件。
pub async fn run(asr: Asr, mut segments: mpsc::Receiver<Vec<f32>>, events: mpsc::Sender<Event>) {
    while let Some(samples) = segments.recv().await {
        let started = Instant::now();
        match asr.transcribe(&samples).await {
            Ok(text) => {
                let text = clean_transcript(&text);
                if text.is_empty() {
                    tracing::debug!(ms = started.elapsed().as_millis(), "ASR 无有效文本");
                    continue;
                }
                tracing::info!(
                    ms = started.elapsed().as_millis(),
                    audio_ms = samples.len() * 1000 / crate::audio::TARGET_RATE as usize,
                    text = %text,
                    "识别"
                );
                if events.send(Event::Heard { text }).await.is_err() {
                    return;
                }
            }
            Err(e) => tracing::warn!("ASR 失败: {e:#}"),
        }
    }
    let _ = events.send(Event::Shutdown).await;
}
