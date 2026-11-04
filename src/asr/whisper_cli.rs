//! whisper.cpp 命令行封装。
//!
//! 每次识别写一个临时 16 kHz WAV，调用 `whisper-cli -m <model> -f <wav> -l zh -nt`。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::audio;
use crate::config::AsrConfig;

pub struct WhisperCli {
    command: String,
    model: PathBuf,
    language: String,
    threads: u32,
    extra_args: Vec<String>,
    tmp_dir: PathBuf,
}

impl WhisperCli {
    pub fn new(cfg: &AsrConfig) -> Result<Self> {
        let tmp_dir = std::env::temp_dir().join("xiaomeng");
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("创建临时目录失败: {}", tmp_dir.display()))?;
        Ok(Self {
            command: cfg.command.clone(),
            model: cfg.model.clone(),
            language: cfg.language.clone(),
            threads: cfg.threads.max(1),
            extra_args: cfg.extra_args.clone(),
            tmp_dir,
        })
    }

    pub fn model(&self) -> &Path {
        &self.model
    }

    pub async fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }

        let wav = self
            .tmp_dir
            .join(format!("utt-{}.wav", uuid::Uuid::new_v4()));
        audio::write_wav_16k(&wav, samples)?;

        let output = Command::new(&self.command)
            .arg("-m")
            .arg(&self.model)
            .arg("-f")
            .arg(&wav)
            .arg("-l")
            .arg(&self.language)
            .arg("-t")
            .arg(self.threads.to_string())
            .arg("-nt")
            .args(&self.extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();

        let result = tokio::time::timeout(Duration::from_secs(180), output).await;
        let _ = tokio::fs::remove_file(&wav).await;

        let output = match result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(e).with_context(|| {
                    format!(
                        "无法执行 {}（确认 whisper.cpp 已编译并加入 PATH）",
                        self.command
                    )
                })
            }
            Err(_) => bail!("whisper 识别超时（180s）"),
        };

        if !output.status.success() {
            bail!(
                "whisper 退出码 {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
