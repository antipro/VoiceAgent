//! 离线排查 ASR：把一段 WAV 直接喂给配置里的 whisper.cpp，打印识别结果。
//!
//!   cargo run --release --example asr_probe -- /tmp/test.wav config.toml
//!
//! 树莓派上排查"为什么唤醒不了"时特别有用：确认 whisper 能听懂你的录音。

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use xiaomeng::asr::Asr;
use xiaomeng::audio::resample::Resampler;
use xiaomeng::audio::TARGET_RATE;
use xiaomeng::config::Config;
use xiaomeng::util::{clean_transcript, is_wake};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let wav = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/test.wav"));
    let config = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let cfg = Config::load(&config)?;
    let (samples, rate) = read_wav(&wav)?;
    let mut resampler = Resampler::new(rate, TARGET_RATE);
    let mut pcm = Vec::new();
    resampler.process(&samples, &mut pcm);

    println!(
        "{}: {:.2}s @ {}Hz → {} 样本 @ {}Hz",
        wav.display(),
        samples.len() as f32 / rate as f32,
        rate,
        pcm.len(),
        TARGET_RATE
    );

    let asr = Asr::from_config(&cfg.asr)?;
    println!("引擎: {}", asr.describe());
    let started = std::time::Instant::now();
    let raw = asr.transcribe(&pcm).await?;
    let text = clean_transcript(&raw);
    println!("耗时: {}ms", started.elapsed().as_millis());
    println!("原文: {raw:?}");
    println!("清洗: {text:?}");
    println!(
        "唤醒: {}",
        if is_wake(&cfg.wake, &text) {
            "是"
        } else {
            "否"
        }
    );

    Ok(())
}

fn read_wav(path: &PathBuf) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("打开失败: {}", path.display()))?;
    let spec = reader.spec();
    let channels = (spec.channels as usize).max(1);

    let mut interleaved = Vec::new();
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for s in reader.samples::<f32>() {
                interleaved.push(s?);
            }
        }
        hound::SampleFormat::Int => {
            if spec.bits_per_sample == 0 || spec.bits_per_sample > 32 {
                bail!("不支持的位深: {}", spec.bits_per_sample);
            }
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            for s in reader.samples::<i32>() {
                interleaved.push(s? as f32 / max);
            }
        }
    }

    let mono = if channels == 1 {
        interleaved
    } else {
        interleaved
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    };
    Ok((mono, spec.sample_rate))
}
