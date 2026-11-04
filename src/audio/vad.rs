//! 端点检测：把连续音频流切成一句一句的话，交给 ASR。
//!
//! 基于 RMS 能量 + 自适应噪声底 + 起始/结束挂起，不依赖外部库，
//! 在树莓派 4 上开销可以忽略。

use std::collections::VecDeque;

use tokio::sync::mpsc;

use crate::audio::{rms, TARGET_RATE};
use crate::config::VadConfig;

pub struct Segmenter {
    frame_len: usize,
    start_frames: u32,
    end_silence_samples: usize,
    pre_roll_frames: usize,
    min_samples: usize,
    max_samples: usize,
    min_rms: f32,
    noise_multiplier: f32,

    noise: f32,
    frame: Vec<f32>,
    ring: VecDeque<Vec<f32>>,
    active: bool,
    voiced_run: u32,
    silence_run: u32,
    /// 真正高于阈值的样本数，用来过滤咳嗽之类的短促声。
    voiced_samples: usize,
    speech: Vec<f32>,
}

/// 把采集到的 PCM 连续切片，产出完整语句。
pub async fn run(
    mut pcm: mpsc::UnboundedReceiver<Vec<f32>>,
    out: mpsc::Sender<Vec<f32>>,
    cfg: VadConfig,
) {
    let mut segmenter = Segmenter::new(&cfg, TARGET_RATE);
    while let Some(chunk) = pcm.recv().await {
        for utterance in segmenter.push(&chunk) {
            tracing::debug!(samples = utterance.len(), "检测到一句话");
            if out.send(utterance).await.is_err() {
                return;
            }
        }
    }
}

impl Segmenter {
    pub fn new(cfg: &VadConfig, rate: u32) -> Self {
        let frame_len = ((rate as u64 * cfg.frame_ms as u64) / 1000).max(1) as usize;
        let end_silence_samples = ((rate as u64 * cfg.end_silence_ms as u64) / 1000) as usize;
        let pre_roll_frames = (cfg.pre_roll_ms / cfg.frame_ms.max(1)).max(1) as usize;
        let min_samples = ((rate as u64 * cfg.min_speech_ms as u64) / 1000) as usize;
        let max_samples = ((rate as u64 * cfg.max_utterance_ms as u64) / 1000) as usize;

        Self {
            frame_len,
            start_frames: cfg.start_frames.max(1),
            end_silence_samples: end_silence_samples.max(frame_len),
            pre_roll_frames,
            min_samples: min_samples.max(frame_len),
            max_samples: max_samples.max(frame_len),
            min_rms: cfg.min_rms,
            noise_multiplier: cfg.noise_multiplier.max(1.1),
            // 从一个极低的噪声底起步：首帧就是人声也能触发。
            noise: 1e-4,
            frame: Vec::with_capacity(frame_len),
            ring: VecDeque::with_capacity(pre_roll_frames + 1),
            active: false,
            voiced_run: 0,
            silence_run: 0,
            voiced_samples: 0,
            speech: Vec::new(),
        }
    }

    /// 喂入任意长度的 16 kHz 单声道音频，返回本次凑齐的完整语句。
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        let mut done = Vec::new();
        for &s in samples {
            self.frame.push(s);
            if self.frame.len() >= self.frame_len {
                let frame = std::mem::replace(&mut self.frame, Vec::with_capacity(self.frame_len));
                self.on_frame(frame, &mut done);
            }
        }
        done
    }

    fn on_frame(&mut self, frame: Vec<f32>, done: &mut Vec<Vec<f32>>) {
        let level = rms(&frame);
        let threshold = (self.noise * self.noise_multiplier).max(self.min_rms);

        if !self.active {
            if level > threshold {
                self.voiced_run += 1;
                self.ring.push_back(frame);
                while self.ring.len() > self.pre_roll_frames {
                    self.ring.pop_front();
                }
                if self.voiced_run >= self.start_frames {
                    self.active = true;
                    self.silence_run = 0;
                    self.voiced_samples = 0;
                    self.speech.clear();
                    for f in self.ring.drain(..) {
                        self.speech.extend_from_slice(&f);
                    }
                    self.voiced_samples = self.speech.len();
                }
            } else {
                self.voiced_run = 0;
                self.ring.push_back(frame);
                while self.ring.len() > self.pre_roll_frames {
                    self.ring.pop_front();
                }
                // 只在没有语音时更新噪声底，避免把说话声吸进背景。
                self.noise = self.noise * 0.95 + level * 0.05;
            }
            return;
        }

        self.speech.extend_from_slice(&frame);
        if level > threshold {
            self.silence_run = 0;
            self.voiced_samples += self.frame_len;
        } else {
            self.silence_run += self.frame_len as u32;
        }

        let silence_done = self.silence_run as usize >= self.end_silence_samples;
        let length_done = self.speech.len() >= self.max_samples;
        let quiet_tail = level <= threshold;
        if (silence_done && quiet_tail) || length_done {
            self.finish(done);
        }
    }

    fn finish(&mut self, done: &mut Vec<Vec<f32>>) {
        self.active = false;
        self.voiced_run = 0;
        self.silence_run = 0;
        self.ring.clear();
        let voiced = std::mem::take(&mut self.voiced_samples);
        // 去掉尾部静音，保留 200 ms 便于 whisper 断句。
        let keep_tail = (self.frame_len * 10).min(self.speech.len());
        let cut = self.speech.len().saturating_sub(keep_tail);
        if cut > self.min_samples {
            self.speech.truncate(cut);
        }
        if voiced >= self.min_samples && self.speech.len() >= self.min_samples {
            done.push(std::mem::take(&mut self.speech));
        } else {
            self.speech.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * 0.1).sin() * amp).collect()
    }

    #[test]
    fn splits_on_silence() {
        let cfg = VadConfig::default();
        let mut seg = Segmenter::new(&cfg, 16_000);
        let mut out = Vec::new();
        // 1 秒语音 + 1 秒静音
        for _ in 0..100 {
            out.extend(seg.push(&tone(160, 0.2)));
        }
        for _ in 0..100 {
            out.extend(seg.push(&[0.0f32; 160]));
        }
        assert_eq!(out.len(), 1, "should produce exactly one utterance");
        assert!(out[0].len() > 10_000);
    }

    #[test]
    fn ignores_short_blips() {
        let cfg = VadConfig::default();
        let mut seg = Segmenter::new(&cfg, 16_000);
        let mut out = Vec::new();
        out.extend(seg.push(&tone(1_600, 0.3))); // 100 ms
        for _ in 0..100 {
            out.extend(seg.push(&vec![0.0f32; 160]));
        }
        assert!(out.is_empty());
    }
}
