//! WAV 读写（只在 CLI 侧使用，实时链路不碰文件）。
//!
//! 约定：48 kHz / 单声道 / f32。读到 16/24/32-bit 整型素材时按满量程归一化成 f32，
//! 写出一律是 32-bit float，避免回归测试里再引入一次量化误差。

use std::path::Path;

/// 读成单声道 f32 采样。采样率不是 48 kHz 或不是单声道时直接报错（P2 才接重采样）。
pub fn read_mono_f32(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("打不开 {}：{e}", path.display()))?;
    let spec = reader.spec();

    if spec.sample_rate != audio_core::SAMPLE_RATE {
        return Err(format!(
            "{} 的采样率是 {} Hz，本工具只吃 {} Hz（P2 才接重采样）",
            path.display(),
            spec.sample_rate,
            audio_core::SAMPLE_RATE
        ));
    }
    if spec.channels != 1 {
        return Err(format!(
            "{} 是 {} 声道，本工具只吃单声道",
            path.display(),
            spec.channels
        ));
    }

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读 {} 失败：{e}", path.display()))?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / f64::from(1_i32 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("读 {} 失败：{e}", path.display()))?
        }
    };

    Ok(samples)
}

/// 写成 32-bit float 单声道 WAV。父目录不存在时自动创建。
pub fn write_mono_f32(path: &Path, samples: &[f32]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: audio_core::SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    for sample in samples {
        writer
            .write_sample(*sample)
            .map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("收尾 {} 失败：{e}", path.display()))?;
    Ok(())
}
