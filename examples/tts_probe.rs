//! 离线排查 TTS：把一句话交给 piper 合成，报告文件大小与时长（不播放）。
//!
//!   cargo run --release --example tts_probe -- "你好，我是小蒙同学" config.toml
//!
//! 树莓派上"没有声音"时先跑这个：能生成 WAV 说明 TTS 正常，问题在播放链路。

use anyhow::{Context, Result};
use std::path::PathBuf;

use xiaomeng::config::Config;
use xiaomeng::tts::Tts;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let text = args
        .next()
        .unwrap_or_else(|| "你好，我是小蒙同学。".to_string());
    let config = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let cfg = Config::load(&config)?;
    let tts = Tts::from_config(&cfg.tts)?;
    println!("引擎: {}", tts.describe());
    println!("文本: {text}");

    let started = std::time::Instant::now();
    let path = tts.synthesize(&text).await?;
    let elapsed = started.elapsed().as_millis();

    match path {
        None => println!("mock 引擎，没有生成音频（耗时 {elapsed}ms）"),
        Some(path) => {
            let meta = std::fs::metadata(&path)
                .with_context(|| format!("找不到输出: {}", path.display()))?;
            let reader = hound::WavReader::open(&path)?;
            let spec = reader.spec();
            let frames = reader.len() as f32 / spec.channels as f32;
            println!(
                "生成 {}（{:.1} KB，{:.2}s @ {}Hz {}ch），耗时 {elapsed}ms",
                path.display(),
                meta.len() as f32 / 1024.0,
                frames / spec.sample_rate as f32,
                spec.sample_rate,
                spec.channels
            );
            println!("试听: afplay {}   # macOS", path.display());
            println!("      aplay {}    # Linux", path.display());
        }
    }

    Ok(())
}
