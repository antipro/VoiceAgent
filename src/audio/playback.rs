//! 播放：WAV → cpal 输出流。
//!
//! cpal 的 `Stream` 不保证 `Send`，所以整个播放流程放在 `spawn_blocking` 里，
//! 用原子标志从异步侧叫停（唤醒词打断时使用）。

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};
use tokio::sync::watch;

use super::resample::Resampler;
use super::select_host;
use crate::config::AudioConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayOutcome {
    Played,
    Cancelled,
}

/// 播放一个 WAV 文件，可被 `wake_rx` / `audio_rx` 的任意一次变更打断。
pub async fn play_wav(
    path: PathBuf,
    audio_cfg: AudioConfig,
    wake_rx: &mut watch::Receiver<u64>,
    audio_rx: &mut watch::Receiver<u64>,
) -> Result<PlayOutcome> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_inner = stop.clone();
    let mut handle =
        tokio::task::spawn_blocking(move || play_blocking(&path, &audio_cfg, stop_inner));

    tokio::select! {
        res = &mut handle => {
            res.context("播放线程崩溃")??;
            return Ok(PlayOutcome::Played);
        }
        _ = wake_rx.changed() => {}
        _ = audio_rx.changed() => {}
    }

    stop.store(true, Ordering::SeqCst);
    // 等阻塞线程自行收尾，确保输出流被释放。
    let res = handle.await.context("播放线程崩溃")?;
    res?;
    Ok(PlayOutcome::Cancelled)
}

fn play_blocking(path: &Path, audio_cfg: &AudioConfig, stop: Arc<AtomicBool>) -> Result<()> {
    let volume = audio_cfg.playback_volume;
    let (samples, rate) = decode_wav(path)?;
    if samples.is_empty() {
        return Ok(());
    }

    let host = select_host(audio_cfg);
    let device = pick_output(&host, audio_cfg.output_device.as_deref())?;
    let supported = device
        .default_output_config()
        .context("读取默认输出配置失败")?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let out_rate = config.sample_rate;
    let channels = config.channels as usize;

    let mut resampler = Resampler::new(rate, out_rate);
    let mut pcm = Vec::with_capacity(samples.len() * out_rate as usize / rate.max(1) as usize + 16);
    resampler.process(&samples, &mut pcm);
    if volume != 1.0 {
        for s in pcm.iter_mut() {
            *s = (*s * volume).clamp(-1.0, 1.0);
        }
    }
    let total_samples = pcm.len();
    let pcm = Arc::new(pcm);
    let finished = Arc::new(AtomicBool::new(false));

    let stream = match format {
        SampleFormat::I8 => build::<i8>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::I16 => build::<i16>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::I32 => build::<i32>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::I64 => build::<i64>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::U8 => build::<u8>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::U16 => build::<u16>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::U32 => build::<u32>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::U64 => build::<u64>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::F32 => build::<f32>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        SampleFormat::F64 => build::<f64>(
            &device,
            &config,
            channels,
            pcm,
            finished.clone(),
            stop.clone(),
        ),
        other => bail!("不支持的输出采样格式: {other}"),
    }?;

    stream.play().context("启动输出流失败")?;

    // 预留一点硬件缓冲时间，避免最后一个字被截断。
    let grace = Duration::from_millis(150);
    // 兜底：设备没有回调时也要退出，避免任务永久卡住。
    let audio_secs = total_samples as f64 / out_rate.max(1) as f64;
    let deadline = StdInstant::now() + Duration::from_secs_f64(audio_secs * 1.5 + 3.0);
    while !finished.load(Ordering::SeqCst)
        && !stop.load(Ordering::SeqCst)
        && StdInstant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    if finished.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(grace);
    }
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    pcm: Arc<Vec<f32>>,
    finished: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = channels.max(1);
    let mut pos = 0usize;

    let stream = device
        .build_output_stream::<T, _, _>(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let muted = stop.load(Ordering::Relaxed);
                for frame in data.chunks_mut(channels) {
                    let s = if muted {
                        0.0
                    } else {
                        pcm.get(pos).copied().unwrap_or(0.0)
                    };
                    pos += 1;
                    let v = T::from_sample(s);
                    for out in frame.iter_mut() {
                        *out = v;
                    }
                }
                if pos >= pcm.len() {
                    finished.store(true, Ordering::SeqCst);
                }
            },
            |e| tracing::error!("输出流出错: {e}"),
            None,
        )
        .context("创建输出流失败")?;
    Ok(stream)
}

fn pick_output(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    if let Some(want) = name {
        let want = want.to_lowercase();
        let devices: Vec<cpal::Device> =
            host.output_devices().context("枚举输出设备失败")?.collect();
        if devices.is_empty() {
            bail!("系统里没有可用的输出设备");
        }
        let names: Vec<String> = devices.iter().map(|d| d.to_string()).collect();
        return devices
            .into_iter()
            .find(|d| d.to_string().to_lowercase().contains(&want))
            .ok_or_else(|| anyhow!("未找到匹配 `{want}` 的输出设备，可用: {names:?}"));
    }
    host.default_output_device()
        .ok_or_else(|| anyhow!("没有默认输出设备"))
}

/// 读取 WAV，下混为单声道 f32。
fn decode_wav(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("打开 WAV 失败: {}", path.display()))?;
    let spec = reader.spec();
    let channels = (spec.channels as usize).max(1);

    let mut interleaved: Vec<f32> = Vec::new();
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for s in reader.samples::<f32>() {
                interleaved.push(s?);
            }
        }
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample.saturating_sub(1).min(31))) as f32;
            for s in reader.samples::<i32>() {
                interleaved.push(s? as f32 / max);
            }
        }
    }

    let mono = if channels == 1 {
        interleaved
    } else {
        let mut mono = Vec::with_capacity(interleaved.len() / channels);
        for frame in interleaved.chunks(channels) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum / frame.len() as f32);
        }
        mono
    };

    Ok((mono, spec.sample_rate))
}
