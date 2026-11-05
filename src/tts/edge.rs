//! Edge 在线 TTS（微软语音服务，`edge-tts` 命令行）。
//!
//! 音质接近真人，但要联网；失败时由 `Tts` 自动回退到 Piper。
//! 输出是 MP3，播放侧用 symphonia 解码后交给 cpal。

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::TtsConfig;

/// 连续失败几次后进入冷却，期间直接用离线语音。
const FAILURE_LIMIT: u32 = 2;
const COOLDOWN: Duration = Duration::from_secs(600);
/// 单次在线合成的超时。网络不好时不要让用户等 30 秒。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

pub struct Edge {
    command: String,
    voice: String,
    rate: Option<String>,
    tmp_dir: PathBuf,
    failures: AtomicU32,
    blocked_until: Mutex<Option<Instant>>,
}

impl Edge {
    pub fn new(cfg: &TtsConfig) -> Result<Self> {
        let tmp_dir = std::env::temp_dir().join("xiaomeng");
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("创建临时目录失败: {}", tmp_dir.display()))?;
        Ok(Self {
            command: cfg.edge_command.clone(),
            voice: cfg.voice.clone(),
            rate: cfg.edge_rate.clone(),
            tmp_dir,
            failures: AtomicU32::new(0),
            blocked_until: Mutex::new(None),
        })
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }

    /// 检查命令是否可用（启动时调用）。
    pub async fn check(&self) -> Result<()> {
        let output = Command::new(&self.command)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("无法执行 {}（需要 pip install edge-tts）", self.command))?;
        if !output.status.success() {
            bail!(
                "edge-tts 自检失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// 冷却期内为 true，调用方可直接走离线语音。
    pub async fn blocked(&self) -> bool {
        match *self.blocked_until.lock().await {
            Some(until) if Instant::now() < until => true,
            Some(_) => false,
            None => false,
        }
    }

    pub async fn synthesize(&self, text: &str) -> Result<PathBuf> {
        if text.trim().is_empty() {
            bail!("TTS 输入为空");
        }
        if self.blocked().await {
            bail!("在线 TTS 冷却中");
        }

        match self.request(text).await {
            Ok(path) => {
                self.failures.store(0, Ordering::Relaxed);
                *self.blocked_until.lock().await = None;
                Ok(path)
            }
            Err(e) => {
                let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= FAILURE_LIMIT {
                    *self.blocked_until.lock().await = Some(Instant::now() + COOLDOWN);
                    self.failures.store(0, Ordering::Relaxed);
                    tracing::warn!(
                        "在线 TTS 连续失败 {n} 次，接下来 {} 分钟改用离线语音",
                        COOLDOWN.as_secs() / 60
                    );
                }
                Err(e)
            }
        }
    }

    async fn request(&self, text: &str) -> Result<PathBuf> {
        let out = self
            .tmp_dir
            .join(format!("tts-{}.mp3", uuid::Uuid::new_v4()));

        let mut cmd = Command::new(&self.command);
        cmd.arg("--voice")
            .arg(&self.voice)
            .arg("--text")
            .arg(text)
            .arg("--write-media")
            .arg(&out)
            .arg("--no-progress");
        if let Some(rate) = &self.rate {
            cmd.arg("--rate").arg(rate);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(REQUEST_TIMEOUT, cmd.output()).await;
        let output = match output {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(e).context("执行 edge-tts 失败"),
            Err(_) => {
                let _ = tokio::fs::remove_file(&out).await;
                bail!("edge-tts 合成超时（{}s）", REQUEST_TIMEOUT.as_secs());
            }
        };

        if !output.status.success() {
            let _ = tokio::fs::remove_file(&out).await;
            bail!(
                "edge-tts 退出码 {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let meta = tokio::fs::metadata(&out)
            .await
            .with_context(|| format!("edge-tts 未生成音频: {}", out.display()))?;
        if meta.len() < 1024 {
            bail!("edge-tts 生成的音频为空");
        }
        Ok(out)
    }
}
