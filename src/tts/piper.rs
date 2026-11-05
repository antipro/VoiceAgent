//! piper TTS 封装。
//!
//! 中文标点处的停顿很短（实测逗号只有 60~80ms），听起来像一口气念完。
//! 所以这里按标点把文本拆成小段，用 `--json-input` 在一次进程里逐段合成
//! （模型只加载一次），再按标点类型插入不同长度的静音拼接成一个 WAV。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::TtsConfig;

/// 最多拆成多少段，避免长回复产生过多临时文件。
const MAX_PIECES: usize = 12;

#[derive(Debug, Clone)]
struct Piece {
    text: String,
    /// 这段之后的静音，毫秒。
    pause_ms: u32,
}

pub struct Piper {
    command: String,
    model: PathBuf,
    config: Option<PathBuf>,
    speaker: Option<i32>,
    length_scale: f32,
    comma_pause_ms: u32,
    sentence_pause_ms: u32,
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
            comma_pause_ms: cfg.comma_pause_ms,
            sentence_pause_ms: cfg.sentence_pause_ms,
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

        let pieces = split_for_speech(text, self.comma_pause_ms, self.sentence_pause_ms);
        match pieces.len() {
            0 => bail!("TTS 输入没有可朗读内容"),
            1 => self.synthesize_one(&pieces[0].text).await,
            _ => self.synthesize_pieces(&pieces).await,
        }
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.arg("-m").arg(&self.model);
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
        cmd
    }

    async fn synthesize_one(&self, text: &str) -> Result<PathBuf> {
        let wav = self
            .tmp_dir
            .join(format!("tts-{}.wav", uuid::Uuid::new_v4()));

        let mut cmd = self.base_command();
        cmd.arg("-f").arg(&wav);
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

    /// 多段合成：一次 piper 进程，模型只加载一次。
    async fn synthesize_pieces(&self, pieces: &[Piece]) -> Result<PathBuf> {
        let id = uuid::Uuid::new_v4();
        let mut parts: Vec<(PathBuf, u32)> = Vec::with_capacity(pieces.len());
        let mut input = String::with_capacity(pieces.len() * 128);

        for (i, piece) in pieces.iter().enumerate() {
            let path = self.tmp_dir.join(format!("tts-{id}-{i}.wav"));
            let line = serde_json::json!({
                "text": piece.text,
                "output_file": path.to_string_lossy(),
            });
            input.push_str(&line.to_string());
            input.push('\n');
            parts.push((path, piece.pause_ms));
        }

        let out = self.tmp_dir.join(format!("tts-{id}.wav"));
        let mut cmd = self.base_command();
        // 每段都是独立合成，Piper 默认还会在段尾加 0.2s 静音；
        // 关掉它，停顿长度完全由 comma_pause_ms / sentence_pause_ms 决定。
        cmd.arg("--sentence_silence").arg("0");
        cmd.arg("--json-input");
        let mut child = cmd.spawn().with_context(|| {
            format!("无法执行 {}（确认 piper 已安装并加入 PATH）", self.command)
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes()).await?;
        }

        let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output()).await;
        let output = match output {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                cleanup(&parts).await;
                return Err(e).context("等待 piper 失败");
            }
            Err(_) => {
                cleanup(&parts).await;
                bail!("piper 合成超时（120s）");
            }
        };

        if !output.status.success() {
            cleanup(&parts).await;
            bail!(
                "piper 退出码 {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let result = concat_wavs(&parts, &out);
        cleanup(&parts).await;
        result?;
        Ok(out)
    }
}

async fn cleanup(parts: &[(PathBuf, u32)]) {
    for (path, _) in parts {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// 把多段 WAV 按顺序拼接，每段后插入对应时长的静音。
fn concat_wavs(parts: &[(PathBuf, u32)], out: &Path) -> Result<()> {
    let first = hound::WavReader::open(&parts[0].0)
        .with_context(|| format!("打开分段音频失败: {}", parts[0].0.display()))?;
    let spec = first.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!("不支持的 piper 输出格式: {spec:?}");
    }

    let mut writer = hound::WavWriter::create(out, spec)
        .with_context(|| format!("创建音频失败: {}", out.display()))?;

    for (path, pause_ms) in parts {
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("打开分段音频失败: {}", path.display()))?;
        let part_spec = reader.spec();
        if part_spec.sample_rate != spec.sample_rate
            || part_spec.channels != spec.channels
            || part_spec.bits_per_sample != spec.bits_per_sample
        {
            bail!("piper 各段输出格式不一致");
        }
        for sample in reader.samples::<i16>() {
            writer.write_sample(sample?)?;
        }

        let frames = spec.sample_rate as u64 * *pause_ms as u64 / 1000;
        for _ in 0..frames {
            for _ in 0..spec.channels {
                writer.write_sample(0i16)?;
            }
        }
    }

    writer.finalize()?;
    Ok(())
}

/// 按标点拆句，并给每段标注停顿长度。
fn split_for_speech(text: &str, comma_ms: u32, sentence_ms: u32) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut cur = String::new();

    for c in text.chars() {
        cur.push(c);
        let pause = match c {
            '，' | ',' | '、' | '：' | ':' | '；' | ';' => comma_ms,
            '。' | '！' | '？' | '!' | '?' | '.' | '\n' => sentence_ms,
            _ => 0,
        };
        if pause > 0 {
            let text = cur.trim().to_string();
            if !text.is_empty() {
                pieces.push(Piece {
                    text,
                    pause_ms: pause,
                });
            }
            cur.clear();
        }
    }

    let rest = cur.trim();
    if !rest.is_empty() {
        pieces.push(Piece {
            text: rest.to_string(),
            pause_ms: 0,
        });
    }

    // 太短的片段（"嗯，"）并到下一段，避免碎成一地。
    let mut i = 0;
    while i + 1 < pieces.len() {
        if visible_len(&pieces[i].text) < 2 {
            let next = pieces.remove(i + 1);
            pieces[i].text.push_str(&next.text);
            pieces[i].pause_ms = next.pause_ms;
        } else {
            i += 1;
        }
    }
    if pieces.len() > 1 && visible_len(&pieces.last().unwrap().text) < 2 {
        let last = pieces.pop().unwrap();
        if let Some(prev) = pieces.last_mut() {
            prev.text.push_str(&last.text);
            prev.pause_ms = last.pause_ms;
        }
    }

    // 最后一段不需要拖尾静音。
    if let Some(last) = pieces.last_mut() {
        last.pause_ms = 0;
    }

    if pieces.len() > MAX_PIECES {
        let tail: Vec<Piece> = pieces.split_off(MAX_PIECES - 1);
        if let Some(last) = pieces.last_mut() {
            for p in tail {
                last.text.push_str(&p.text);
                last.pause_ms = p.pause_ms;
            }
        }
    }

    pieces
}

fn visible_len(text: &str) -> usize {
    text.chars()
        .filter(|c| c.is_alphanumeric() || is_cjk(*c))
        .count()
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_punctuation_with_pauses() {
        let pieces = split_for_speech("你好呀，我是小蒙同学，有什么可以帮你的吗？", 180, 420);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0].text, "你好呀，");
        assert_eq!(pieces[0].pause_ms, 180);
        assert_eq!(pieces[2].text, "有什么可以帮你的吗？");
        assert_eq!(pieces[2].pause_ms, 0);
    }

    #[test]
    fn no_punctuation_is_single_piece() {
        let pieces = split_for_speech("今天天气不错", 180, 420);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].pause_ms, 0);
    }

    #[test]
    fn merges_short_fragments() {
        let pieces = split_for_speech("嗯，好的。", 180, 420);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].text, "嗯，好的。");
    }
}
