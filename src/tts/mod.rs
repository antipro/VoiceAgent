//! TTS 引擎：默认调用 piper 命令行程序。

pub mod piper;

use anyhow::Result;
use std::path::PathBuf;

use crate::config::TtsConfig;
use piper::Piper;

pub enum Tts {
    Piper(Piper),
    /// 只打印文本，便于在开发机上跑通链路。
    Mock,
}

impl Tts {
    pub fn from_config(cfg: &TtsConfig) -> Result<Self> {
        match cfg.engine.as_str() {
            "piper" => Ok(Self::Piper(Piper::new(cfg)?)),
            "mock" => Ok(Self::Mock),
            other => anyhow::bail!("未知的 tts.engine: {other}（可选 piper / mock）"),
        }
    }

    /// 合成语音，返回 WAV 路径；Mock 返回 `None`。
    /// 调用方负责在播放后删除临时文件。
    pub async fn synthesize(&self, text: &str) -> Result<Option<PathBuf>> {
        match self {
            Self::Piper(p) => Ok(Some(p.synthesize(text).await?)),
            Self::Mock => {
                tracing::info!(tts = %text, "（mock）");
                Ok(None)
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Piper(p) => format!("piper ({})", p.model().display()),
            Self::Mock => "mock".into(),
        }
    }
}
