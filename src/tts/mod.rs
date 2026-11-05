//! TTS 引擎：离线 Piper / 在线 Edge / mock。

pub mod edge;
pub mod piper;

use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::config::TtsConfig;
use edge::Edge;
use piper::Piper;

pub enum Tts {
    Piper(Box<Piper>),
    /// 在线 Edge TTS；失败时自动回退到 Piper（若模型存在）。
    Edge {
        edge: Box<Edge>,
        fallback: Option<Box<Piper>>,
    },
    /// 只打印文本，便于在开发机上跑通链路。
    Mock,
}

impl Tts {
    pub fn from_config(cfg: &TtsConfig) -> Result<Self> {
        match cfg.engine.as_str() {
            "piper" => Ok(Self::Piper(Box::new(Piper::new(cfg)?))),
            "edge" => {
                let edge = Box::new(Edge::new(cfg)?);
                let fallback = if cfg.model.exists() {
                    Some(Box::new(Piper::new(cfg)?))
                } else {
                    tracing::warn!(
                        model = %cfg.model.display(),
                        "找不到 Piper 模型，在线 TTS 失败时无法回退"
                    );
                    None
                };
                Ok(Self::Edge { edge, fallback })
            }
            "mock" => Ok(Self::Mock),
            other => bail!("未知的 tts.engine: {other}（可选 piper / edge / mock）"),
        }
    }

    /// 预加载模型（piper 常驻进程），让第一句话也不用等模型加载。
    pub async fn warmup(&self) -> Result<()> {
        match self {
            Self::Piper(p) => p.warmup().await,
            Self::Edge { edge, fallback } => {
                // 回退引擎也预热好，断网时切回 Piper 不用再等模型加载。
                if let Some(p) = fallback {
                    if let Err(e) = p.warmup().await {
                        tracing::warn!("Piper 回退预热失败: {e:#}");
                    }
                }
                edge.check().await
            }
            Self::Mock => Ok(()),
        }
    }

    /// 按标点切分，返回 (文本, 停顿毫秒)。
    pub fn pieces(&self, text: &str) -> Vec<(String, u32)> {
        match self {
            Self::Piper(p) => p.pieces(text),
            // Edge 自带断句和停顿，整段一次合成。
            Self::Edge { .. } | Self::Mock => vec![(text.to_string(), 0)],
        }
    }

    /// 流式播放时逐段合成。
    pub async fn synthesize_piece(&self, text: &str) -> Result<PathBuf> {
        match self {
            Self::Piper(p) => p.synthesize_piece(text).await,
            Self::Edge { .. } | Self::Mock => bail!("该引擎不支持分段合成"),
        }
    }

    /// 合成语音，返回 WAV 路径；Mock 返回 `None`。
    /// 调用方负责在播放后删除临时文件。
    pub async fn synthesize(&self, text: &str) -> Result<Option<PathBuf>> {
        match self {
            Self::Piper(p) => Ok(Some(p.synthesize(text).await?)),
            Self::Edge { edge, fallback } => {
                // 冷却期内直接走离线，避免每条回复都等一次超时。
                if edge.blocked().await {
                    if let Some(p) = fallback {
                        tracing::debug!("在线 TTS 冷却中，使用离线语音");
                        return Ok(Some(p.synthesize(text).await?));
                    }
                }
                match edge.synthesize(text).await {
                    Ok(path) => Ok(Some(path)),
                    Err(e) => match fallback {
                        Some(p) => {
                            tracing::warn!("在线 TTS 失败，回退 Piper: {e:#}");
                            Ok(Some(p.synthesize(text).await?))
                        }
                        None => Err(e),
                    },
                }
            }
            Self::Mock => {
                tracing::info!(tts = %text, "（mock）");
                Ok(None)
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Piper(p) => format!("piper ({})", p.model().display()),
            Self::Edge { edge, fallback } => format!(
                "edge ({}, fallback={})",
                edge.voice(),
                if fallback.is_some() { "piper" } else { "none" }
            ),
            Self::Mock => "mock".into(),
        }
    }
}
