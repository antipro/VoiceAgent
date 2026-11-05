//! 音频子系统：采集、播放、重采样与端点检测（VAD）。

pub mod capture;
pub mod playback;
pub mod resample;
pub mod stdin_source;
pub mod vad;

use anyhow::{Context, Result};
use std::path::Path;

use crate::config::AudioConfig;

/// 选择音频主机。
///
/// Linux 上优先 PulseAudio/PipeWire：用户态就能用，蓝牙音箱、AEC 都走它。
/// 没有 Pulse 套接字（或配置强制 alsa）时回退到 ALSA。
pub fn select_host(cfg: &AudioConfig) -> cpal::Host {
    #[cfg(target_os = "linux")]
    {
        let use_pulse = match cfg.host.as_str() {
            "pulse" | "pulseaudio" | "pipewire" => true,
            "alsa" => false,
            _ => pulse_socket().is_some(),
        };
        if use_pulse {
            match cpal::host_from_id(cpal::HostId::PulseAudio) {
                Ok(host) => {
                    tracing::info!("音频主机: PulseAudio/PipeWire");
                    return host;
                }
                Err(e) => tracing::warn!("PulseAudio 主机不可用（{e}），回退 ALSA"),
            }
        }
        tracing::info!("音频主机: ALSA");
    }
    let _ = cfg;
    cpal::default_host()
}

/// `$XDG_RUNTIME_DIR/pulse/native`，或显式设置的 `PULSE_SERVER`。
#[cfg(target_os = "linux")]
fn pulse_socket() -> Option<std::path::PathBuf> {
    if let Ok(server) = std::env::var("PULSE_SERVER") {
        if !server.trim().is_empty() {
            return Some(std::path::PathBuf::from(server));
        }
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let path = std::path::PathBuf::from(runtime).join("pulse/native");
    path.exists().then_some(path)
}

/// ASR / VAD 统一使用 16 kHz 单声道。
pub const TARGET_RATE: u32 = 16_000;

/// 把 f32 采样写成 16-bit PCM WAV（whisper.cpp 的输入格式）。
pub fn write_wav_16k(path: &Path, samples: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("创建 WAV 失败: {}", path.display()))?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer.write_sample(v)?;
    }
    writer.finalize()?;
    Ok(())
}

/// 均方根音量，用于 VAD 与日志。
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}
