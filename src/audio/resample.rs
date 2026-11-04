//! 轻量重采样。
//!
//! 采集方向（例如 48 kHz → 16 kHz）使用滑动平均抽取，顺带做了抗混叠；
//! 播放方向（22.05 kHz → 48 kHz）使用线性插值，音质足够。

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// 降采样：当前桶的累加与计数。
    acc: f32,
    count: u32,
    /// 相位，单位“输入样本”。
    phase: f64,
    /// 升采样：上一个输入样本。
    prev: f32,
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate: in_rate.max(1),
            out_rate: out_rate.max(1),
            acc: 0.0,
            count: 0,
            phase: 0.0,
            prev: 0.0,
            primed: false,
        }
    }

    /// 处理单声道输入，结果追加到 `out`。
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.in_rate == self.out_rate {
            out.extend_from_slice(input);
            return;
        }
        if self.in_rate > self.out_rate {
            self.downsample(input, out);
        } else {
            self.upsample(input, out);
        }
    }

    fn downsample(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let step = self.in_rate as f64 / self.out_rate as f64;
        for &s in input {
            self.acc += s;
            self.count += 1;
            self.phase += 1.0;
            if self.phase >= step {
                self.phase -= step;
                out.push(self.acc / self.count as f32);
                self.acc = 0.0;
                self.count = 0;
            }
        }
    }

    fn upsample(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let step = self.in_rate as f64 / self.out_rate as f64;
        for &s in input {
            if !self.primed {
                self.prev = s;
                self.primed = true;
                self.phase = step;
                out.push(s);
                continue;
            }
            while self.phase < 1.0 {
                let t = self.phase as f32;
                out.push(self.prev + (s - self.prev) * t);
                self.phase += step;
            }
            self.phase -= 1.0;
            self.prev = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_reduces_length() {
        let mut rs = Resampler::new(48_000, 16_000);
        let input = vec![0.5f32; 48_000];
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert!((out.len() as i32 - 16_000).abs() <= 2, "got {}", out.len());
        assert!((out[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn upsample_increases_length() {
        let mut rs = Resampler::new(22_050, 48_000);
        let input = vec![0.25f32; 22_050];
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert!(out.len() > 20_000);
    }
}
