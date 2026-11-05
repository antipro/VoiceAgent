//! piper TTS 封装。
//!
//! 两件事决定了这里的实现：
//!
//! 1. **模型加载很贵**（Pi 4 上约 3~4 秒）。所以 piper 以 `--json-input` 常驻，
//!    启动时加载一次，之后每句话只付推理时间。
//! 2. **中文标点停顿很短**（实测逗号只有 60~80ms）。所以按标点拆段，
//!    逐段合成后按标点类型插入静音再拼接。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::config::TtsConfig;

/// 最多拆成多少段，避免长回复产生过多临时文件。
const MAX_PIECES: usize = 12;
/// 单次合成的超时。
const SYNTH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct Piece {
    text: String,
    /// 这段之后的静音，毫秒。
    pause_ms: u32,
}

struct PiperProcess {
    /// 持有子进程句柄；drop 时由 kill_on_drop 结束 piper。
    _child: Child,
    stdin: ChildStdin,
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
    /// 常驻进程；出错或退出后置空，下次合成时重建。
    process: Mutex<Option<PiperProcess>>,
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
            process: Mutex::new(None),
        })
    }

    pub fn model(&self) -> &Path {
        &self.model
    }

    /// 提前把模型加载好（启动时调用，第一句话就不用等）。
    ///
    /// piper 没有「就绪」信号，所以真的合成一个短音节，等它写出文件，
    /// 这样才能确认模型已经加载完毕。
    pub async fn warmup(&self) -> Result<()> {
        let path = self.synth_line("嗯").await?;
        let _ = tokio::fs::remove_file(&path).await;
        Ok(())
    }

    /// 按标点切分，返回 (文本, 该段之后的停顿毫秒)。
    pub fn pieces(&self, text: &str) -> Vec<(String, u32)> {
        split_for_speech(text, self.comma_pause_ms, self.sentence_pause_ms)
            .into_iter()
            .map(|p| (p.text, p.pause_ms))
            .collect()
    }

    /// 合成单独一段（不含停顿）。流式播放时逐段调用。
    pub async fn synthesize_piece(&self, text: &str) -> Result<PathBuf> {
        self.synth_line(text).await
    }

    pub async fn synthesize(&self, text: &str) -> Result<PathBuf> {
        if text.trim().is_empty() {
            bail!("TTS 输入为空");
        }
        let pieces = self.pieces(text);
        if pieces.is_empty() {
            bail!("TTS 输入没有可朗读内容");
        }
        if pieces.len() == 1 {
            return self.synth_line(&pieces[0].0).await;
        }

        let id = uuid::Uuid::new_v4();
        let mut parts: Vec<(PathBuf, u32)> = Vec::with_capacity(pieces.len());
        for (piece, pause) in &pieces {
            let path = self.synth_line(piece).await?;
            // synth_line 生成的是随机文件名，这里保持原样即可。
            parts.push((path, *pause));
        }
        let out = self.tmp_dir.join(format!("tts-{id}.wav"));
        let result = concat_wavs(&parts, &out);
        cleanup(&parts).await;
        result?;
        Ok(out)
    }

    /// 让常驻 piper 合成一段，等 WAV 写完后返回路径。
    async fn synth_line(&self, text: &str) -> Result<PathBuf> {
        let path = self
            .tmp_dir
            .join(format!("tts-{}.wav", uuid::Uuid::new_v4()));
        let line = serde_json::json!({
            "text": text,
            "output_file": path.to_string_lossy(),
        });

        let mut guard = self.process.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn_process()?);
        }
        let proc = guard.as_mut().expect("process just created");

        let write = async {
            proc.stdin.write_all(line.to_string().as_bytes()).await?;
            proc.stdin.write_all(b"\n").await?;
            proc.stdin.flush().await
        }
        .await;
        if let Err(e) = write {
            *guard = None;
            return Err(e).context("写入 piper 失败");
        }

        let deadline = Instant::now() + SYNTH_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                *guard = None;
                let _ = tokio::fs::remove_file(&path).await;
                bail!("piper 合成超时（{}s）", SYNTH_TIMEOUT.as_secs());
            }
            if let Ok(meta) = tokio::fs::metadata(&path).await {
                if meta.len() > 1024 {
                    // 等文件大小稳定，避免读到写了一半的 WAV。
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if let Ok(again) = tokio::fs::metadata(&path).await {
                        if again.len() == meta.len() {
                            break;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        Ok(path)
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.arg("-m").arg(&self.model);
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
        cmd.args(&self.extra_args);
        cmd
    }

    fn spawn_process(&self) -> Result<PiperProcess> {
        let mut cmd = self.base_command();
        // 每段独立合成，关掉 piper 默认的 0.2s 段尾静音，停顿完全由我们控制。
        cmd.arg("--sentence_silence")
            .arg("0")
            .arg("--json-input")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().with_context(|| {
            format!("无法执行 {}（确认 piper 已安装并加入 PATH）", self.command)
        })?;
        let stdin = child.stdin.take().context("piper stdin 不可用")?;

        // 必须把 stderr 读掉，否则管道写满会把 piper 卡住。
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::trace!(piper = %line);
                }
            });
        }

        tracing::info!("piper 常驻进程已启动（模型只加载一次）");
        Ok(PiperProcess {
            _child: child,
            stdin,
        })
    }
}

impl Drop for Piper {
    fn drop(&mut self) {
        // kill_on_drop 会处理 Child；这里显式说明意图。
        if let Ok(mut guard) = self.process.try_lock() {
            guard.take();
        }
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
        let pieces = split_for_speech("你好呀，我是小蒙同学，有什么可以帮你的吗？", 120, 350);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0].text, "你好呀，");
        assert_eq!(pieces[0].pause_ms, 120);
        assert_eq!(pieces[2].text, "有什么可以帮你的吗？");
        assert_eq!(pieces[2].pause_ms, 0);
    }

    #[test]
    fn no_punctuation_is_single_piece() {
        let pieces = split_for_speech("今天天气不错", 120, 350);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].pause_ms, 0);
    }

    #[test]
    fn merges_short_fragments() {
        let pieces = split_for_speech("嗯，好的。", 120, 350);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].text, "嗯，好的。");
    }
}
