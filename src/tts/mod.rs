//! TTS 引擎：默认调用 piper 命令行程序。

pub mod piper;

use anyhow::Result;
use std::path::PathBuf;

use crate::config::TtsConfig;
use piper::Piper;

pub enum Tts {
    Piper(Box<Piper>),
    /// 只打印文本，便于在开发机上跑通链路。
    Mock,
}

impl Tts {
    pub fn from_config(cfg: &TtsConfig) -> Result<Self> {
        match cfg.engine.as_str() {
            "piper" => Ok(Self::Piper(Box::new(Piper::new(cfg)?))),
            "mock" => Ok(Self::Mock),
            other => anyhow::bail!("未知的 tts.engine: {other}（可选 piper / mock）"),
        }
    }

    /// 预加载模型（piper 常驻进程），让第一句话也不用等模型加载。
    pub async fn warmup(&self) -> Result<()> {
        match self {
            Self::Piper(p) => p.warmup().await,
            Self::Mock => Ok(()),
        }
    }

    /// 按标点切分，返回 (文本, 停顿毫秒)。
    pub fn pieces(&self, text: &str) -> Vec<(String, u32)> {
        match self {
            Self::Piper(p) => p.pieces(text),
            Self::Mock => vec![(text.to_string(), 0)],
        }
    }

    /// 流式播放时逐段合成。
    pub async fn synthesize_piece(&self, text: &str) -> Result<PathBuf> {
        match self {
            Self::Piper(p) => p.synthesize_piece(text).await,
            Self::Mock => anyhow::bail!("mock 引擎不支持分段合成"),
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
