//! 麦克风采集：cpal 回调线程 → 16 kHz 单声道 → tokio 通道。

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use super::resample::Resampler;
use super::{select_host, TARGET_RATE};
use crate::config::AudioConfig;

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn start(cfg: &AudioConfig, tx: UnboundedSender<Vec<f32>>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_inner = stop.clone();
        let cfg = cfg.clone();
        let thread = std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                if let Err(e) = run(cfg, tx, stop_inner) {
                    tracing::error!("音频采集失败: {e:#}");
                }
            })
            .context("启动采集线程失败")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(cfg: AudioConfig, tx: UnboundedSender<Vec<f32>>, stop: Arc<AtomicBool>) -> Result<()> {
    let host = select_host(&cfg);
    let device = pick_input(&host, cfg.input_device.as_deref())?;
    let supported = device
        .default_input_config()
        .context("读取默认输入配置失败（检查麦克风是否被占用）")?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let rate = config.sample_rate;

    tracing::info!(device = %device, rate, channels, ?format, "麦克风已打开");

    let resampler = Resampler::new(rate, TARGET_RATE);
    let stream = match format {
        SampleFormat::I8 => build::<i8>(&device, &config, channels, resampler, tx),
        SampleFormat::I16 => build::<i16>(&device, &config, channels, resampler, tx),
        SampleFormat::I32 => build::<i32>(&device, &config, channels, resampler, tx),
        SampleFormat::I64 => build::<i64>(&device, &config, channels, resampler, tx),
        SampleFormat::U8 => build::<u8>(&device, &config, channels, resampler, tx),
        SampleFormat::U16 => build::<u16>(&device, &config, channels, resampler, tx),
        SampleFormat::U32 => build::<u32>(&device, &config, channels, resampler, tx),
        SampleFormat::U64 => build::<u64>(&device, &config, channels, resampler, tx),
        SampleFormat::F32 => build::<f32>(&device, &config, channels, resampler, tx),
        SampleFormat::F64 => build::<f64>(&device, &config, channels, resampler, tx),
        other => bail!("不支持的采样格式: {other}"),
    }?;

    stream.play().context("启动输入流失败")?;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut resampler: Resampler,
    tx: UnboundedSender<Vec<f32>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = channels.max(1);
    let mut mono = Vec::<f32>::with_capacity(2048);
    let mut out = Vec::<f32>::with_capacity(2048);

    let stream = device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                mono.clear();
                out.clear();
                for frame in data.chunks(channels) {
                    let mut sum = 0.0f32;
                    for &s in frame {
                        sum += f32::from_sample(s);
                    }
                    mono.push(sum / frame.len() as f32);
                }
                resampler.process(&mono, &mut out);
                if !out.is_empty() {
                    // 无界通道：回调里绝不阻塞。
                    let _ = tx.send(out.clone());
                }
            },
            |e| tracing::error!("输入流出错: {e}"),
            None,
        )
        .context("创建输入流失败")?;
    Ok(stream)
}

fn pick_input(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    if let Some(want) = name {
        let want = want.to_lowercase();
        let devices: Vec<cpal::Device> =
            host.input_devices().context("枚举输入设备失败")?.collect();
        if devices.is_empty() {
            bail!("系统里没有可用的输入设备");
        }
        let names: Vec<String> = devices.iter().map(|d| d.to_string()).collect();
        return devices
            .into_iter()
            .find(|d| d.to_string().to_lowercase().contains(&want))
            .ok_or_else(|| anyhow!("未找到匹配 `{want}` 的输入设备，可用: {names:?}"));
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("没有默认输入设备"))
}
