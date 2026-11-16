//! 合成测试素材生成（P1-5）。
//!
//! 决策背景（执行方案 §12 第 3 条）：真实开黑素材最终必须测，但当前先用**合成**素材把流程与基线跑通。
//! 随机种子固定 ⇒ 同一份代码永远生成同一份素材，因此素材可以当 fixture 用，不必进仓库。
//!
//! 生成器只做「能验证链路」的信号，不追求像真人：合成素材只能看趋势与做回归，
//! **不能**用于 D6 盲听裁决或「降噪有没有吞字」这类主观判断（见 docs/measurements.md 头注）。

use std::f32::consts::PI;
use std::path::Path;

use crate::wav;

/// 素材生成函数签名：`(样本数, 种子) -> 采样`。
type Generator = fn(usize, u64) -> Vec<f32>;

const FS: f32 = audio_core::SAMPLE_RATE as f32;
/// 峰值目标：约 −6 dBFS，和真实录音的常见电平一致。
const PEAK: f32 = 0.5;

/// `testkit gen --out <dir> [--seed 42] [--seconds 20]`
pub fn main(args: &[String]) -> Result<(), String> {
    let mut out_dir = "testdata/synthetic".to_owned();
    let mut seed = 42_u64;
    let mut seconds = 20_u32;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out_dir = super::next_value(args, &mut i, "--out")?;
            }
            "--seed" => {
                seed = super::next_value(args, &mut i, "--seed")?
                    .parse()
                    .map_err(|e| format!("--seed 不是整数：{e}"))?;
            }
            "--seconds" => {
                seconds = super::next_value(args, &mut i, "--seconds")?
                    .parse()
                    .map_err(|e| format!("--seconds 不是整数：{e}"))?;
            }
            other => return Err(format!("gen 不认识参数 {other:?}")),
        }
        i += 1;
    }

    let dir = Path::new(&out_dir);
    let n = (seconds as usize) * audio_core::SAMPLE_RATE as usize;
    let items: [(&str, Generator, &str); 6] = [
        (
            "clean_48k_mono.wav",
            clean,
            "合成元音 + 慢扫频：链路不炸、增益正确",
        ),
        (
            "keyboard_48k_mono.wav",
            keyboard,
            "脉冲群：非平稳瞬态压制趋势",
        ),
        ("fan_48k_mono.wav", fan, "粉噪 + 低频加重：稳态噪声压制"),
        ("chair_48k_mono.wav", chair, "100–800 Hz 摩擦噪声：低频瞬态"),
        (
            "babble_48k_mono.wav",
            babble,
            "多路元音叠加：背景人声压制趋势",
        ),
        (
            "noise_only_48k_mono.wav",
            noise_only,
            "白噪：音乐噪声与泄漏检测",
        ),
    ];

    for (name, make, note) in items {
        let mut samples = make(n, seed);
        normalize(&mut samples, PEAK);
        let path = dir.join(name);
        wav::write_mono_f32(&path, &samples)?;
        println!(
            "gen {:<28} {:>6.2} s  {:<28} {}",
            name,
            samples.len() as f32 / FS,
            format!("{:.1} MB", samples.len() as f32 * 4.0 / 1_048_576.0),
            note
        );
    }
    println!(
        "素材已生成到 {}（种子 {seed}，{} s/条）",
        dir.display(),
        seconds
    );
    Ok(())
}

/// 确定性伪随机（xorshift64*）：同一种子 ⇒ 同一份素材。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0, 1)
    fn unit(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / 16_777_216.0
    }

    /// [-1, 1)
    fn bipolar(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }
}

/// RBJ 双二阶滤波器（生成器内部用，不追求实时性）。
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn bandpass(freq: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * freq / FS;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            b1: 0.0,
            b2: -alpha / a0,
            a1: -2.0 * cos_w0 / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn lowpass(freq: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * freq / FS;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b1 = (1.0 - cos_w0) / a0;
        Self {
            b0: b1 / 2.0,
            b1,
            b2: b1 / 2.0,
            a1: -2.0 * cos_w0 / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// 共振峰元音：基频谐波堆按三条共振峰加权，再乘音节包络。
fn vowel_into(out: &mut [f32], f0: f32, syllables: f32, gain: f32, offset: usize) {
    let formants = [
        (700.0_f32, 130.0_f32, 1.0_f32),
        (1_220.0, 170.0, 0.6),
        (2_600.0, 250.0, 0.3),
    ];
    let harmonics = 40;
    for (index, sample) in out.iter_mut().enumerate() {
        let n = (index + offset) as f32;
        let t = n / FS;
        let mut value = 0.0_f32;
        for k in 1..=harmonics {
            let freq = f0 * k as f32;
            if freq > 8_000.0 {
                break;
            }
            let mut weight = 0.0_f32;
            for (center, bandwidth, amp) in formants {
                let d = (freq - center) / bandwidth;
                weight += amp * (-d * d).exp();
            }
            if weight > 1e-3 {
                value += weight * (2.0 * PI * freq * t).sin() / (k as f32).sqrt();
            }
        }
        // 音节包络：0.5–4 Hz 的起伏，让编码器看到真实的语音动态
        let envelope = 0.5 * (1.0 - (2.0 * PI * syllables * t).cos());
        *sample += gain * value * envelope;
    }
}

fn clean(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0_f32; n];
    vowel_into(&mut out, 120.0, 3.5, 0.25, 0);
    // 叠一条慢扫频（200 Hz → 4 kHz），把中高频也覆盖到
    let duration = n as f32 / FS;
    for (index, sample) in out.iter_mut().enumerate() {
        let t = index as f32 / FS;
        let phase = 2.0 * PI * (200.0 * t + (4_000.0 - 200.0) * t * t / (2.0 * duration));
        *sample += 0.05 * phase.sin();
    }
    // 一点底噪，避免出现「数字静音」这种不真实的极端
    for sample in &mut out {
        *sample += 0.002 * rng.bipolar();
    }
    out
}

fn keyboard(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0_f32; n];
    let mut cursor = 0_usize;
    while cursor < n {
        // 10–40 次/秒的击键
        let rate = 10.0 + rng.unit() * 30.0;
        cursor += (FS / rate) as usize;
        if cursor >= n {
            break;
        }
        let len = ((0.005 + rng.unit() * 0.015) * FS) as usize; // 5–20 ms
        let freq = 2_000.0 + rng.unit() * 6_000.0; // 2–8 kHz
        let amp = 0.3 + rng.unit() * 0.5;
        let decay = (len as f32) / 4.0;
        for i in 0..len.min(n - cursor) {
            let env = (-(i as f32) / decay).exp();
            out[cursor + i] += amp * env * (2.0 * PI * freq * i as f32 / FS).sin();
        }
    }
    for sample in &mut out {
        *sample += 0.002 * rng.bipolar();
    }
    out
}

/// Paul Kellet 的粉噪近似（3 极点）。
fn fan(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0_f32; n];
    let (mut b0, mut b1, mut b2) = (0.0_f32, 0.0_f32, 0.0_f32);
    let mut low = Biquad::lowpass(300.0, 0.7);
    for sample in &mut out {
        let white = rng.bipolar();
        b0 = 0.997_65 * b0 + white * 0.099_046;
        b1 = 0.963_00 * b1 + white * 0.296_516;
        b2 = 0.570_00 * b2 + white * 1.052_691;
        let pink = (b0 + b1 + b2 + white * 0.184_8) * 0.2;
        // 1 kHz 以下加重，模拟风扇的低频轰鸣
        *sample = pink + 0.6 * low.process(pink);
    }
    out
}

fn chair(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0_f32; n];
    let mut band = Biquad::bandpass(400.0, 0.8);
    let mut envelope = 0.0_f32;
    for sample in &mut out {
        // 慢速随机包络：摩擦声是「有起伏的噪声」而不是稳态噪声
        envelope = 0.995 * envelope + 0.005 * rng.unit();
        *sample = band.process(rng.bipolar()) * (0.2 + 2.0 * envelope);
    }
    out
}

fn babble(n: usize, seed: u64) -> Vec<f32> {
    let mut out = vec![0.0_f32; n];
    // 5 路「人声」，基频与音节节奏各不相同
    let voices = [
        (98.0_f32, 4.1_f32, 0.5_f32),
        (132.0, 3.3, 0.45),
        (156.0, 5.2, 0.4),
        (184.0, 2.7, 0.35),
        (110.0, 4.6, 0.42),
    ];
    for (index, (f0, syllables, gain)) in voices.iter().enumerate() {
        let offset = index * 977;
        vowel_into(&mut out, *f0, *syllables, *gain, offset);
    }
    let mut rng = Rng::new(seed);
    for sample in &mut out {
        *sample += 0.01 * rng.bipolar();
    }
    out
}

fn noise_only(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| rng.bipolar()).collect()
}

fn normalize(samples: &mut [f32], target: f32) {
    let peak = samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    if peak <= f32::EPSILON {
        return;
    }
    let gain = target / peak;
    for sample in samples {
        *sample *= gain;
    }
}
