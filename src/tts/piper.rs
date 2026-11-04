//! piper TTS 封装：文本从 stdin 传入，WAV 写到临时文件。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::TtsConfig;

pub struct Piper {
    command: String,
    model: PathBuf,
    config: Option<PathBuf>,
    speaker: Option<i32>,
    length_scale: f32,
    extra_args: Vec<String>,
    tmp_dir: PathBuf,
}

impl Piper {
    pub fn new(cfg: &TtsConfig) -> Result<Self> {
        let tmp_dir = std::env::temp_dir().join("xiaomeng");
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("创建临时目录失败: {}", tmp_dir.display()))?;
        Ok(Self {
            command: cfg.command.clone(),
            model: cfg.model.clone(),
            config: cfg.config.clone(),
            speaker: cfg.speaker,
            length_scale: cfg.length_scale,
            extra_args: cfg.extra_args.clone(),
            tmp_dir,
        })
    }

    pub fn model(&self) -> &Path {
        &self.model
    }

    pub async fn synthesize(&self, text: &str) -> Result<PathBuf> {
        if text.trim().is_empty() {
            bail!("TTS 输入为空");
        }

        let wav = self
            .tmp_dir
            .join(format!("tts-{}.wav", uuid::Uuid::new_v4()));

        let mut cmd = Command::new(&self.command);
        cmd.arg("-m").arg(&self.model).arg("-f").arg(&wav);
        // piper 2023.11.x 用 --length_scale；等于 1.0 时省略，兼容其它版本。
        if (self.length_scale - 1.0).abs() > f32::EPSILON {
            cmd.arg("--length_scale")
                .arg(format!("{:.2}", self.length_scale));
        }
        if let Some(cfg) = &self.config {
            cmd.arg("-c").arg(cfg);
        }
        if let Some(sp) = self.speaker {
            cmd.arg("--speaker").arg(sp.to_string());
        }
        cmd.args(&self.extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().with_context(|| {
            format!("无法执行 {}（确认 piper 已安装并加入 PATH）", self.command)
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
            // drop 关闭 stdin，piper 才会开始合成。
        }

        let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await;
        let output = match output {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(e).context("等待 piper 失败"),
            Err(_) => {
                let _ = tokio::fs::remove_file(&wav).await;
                bail!("piper 合成超时（60s）");
            }
        };

        if !output.status.success() {
            let _ = tokio::fs::remove_file(&wav).await;
            bail!(
                "piper 退出码 {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let meta = tokio::fs::metadata(&wav)
            .await
            .with_context(|| format!("piper 未生成音频文件: {}", wav.display()))?;
        if meta.len() == 0 {
            bail!("piper 生成了空文件");
        }

        Ok(wav)
    }
}
