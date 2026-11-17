//! P4-5 的门禁：8 路真实链路（Opus → 抖动缓冲 → 混音）能混出声音，且混音本身零分配。

// 这个测试要打印测量数字，和 testkit 的其它测量一样允许 stdout。
#![allow(clippy::print_stdout)]

use std::f32::consts::PI;
use std::time::Instant;

use audio_core::receive::{IncomingPacket, ReceivePath};
use audio_core::{
    AudioCodec, FRAME_SAMPLES, Frame, MAX_PACKET_BYTES, MAX_TRACKS, Mixer, OpusCodec,
};

use testkit::alloc;

const FRAMES: usize = 200;
const SENDER_BASE: u32 = 0x1000;

fn tone(frequency: f32, frame_index: usize, amplitude: f32) -> Frame {
    let mut frame: Frame = [0.0; FRAME_SAMPLES];
    for (n, sample) in frame.iter_mut().enumerate() {
        let t = (frame_index * FRAME_SAMPLES + n) as f32 / audio_core::SAMPLE_RATE as f32;
        *sample = amplitude * (2.0 * PI * frequency * t).sin();
    }
    frame
}

#[test]
fn eight_real_tracks_mix_without_allocating_on_the_audio_side() {
    let (mut mixer, control) = Mixer::new();

    // 8 条完整接收链：每路一个发送方、一个抖动缓冲、一条 PCM 环。
    let mut senders = Vec::new();
    let mut paths = Vec::new();
    for slot in 0..MAX_TRACKS {
        let sender = SENDER_BASE + slot as u32;
        let (tx, path, pcm) = ReceivePath::new(sender, 64, 8).expect("构造接收路径");
        control.add_track(slot, Box::new(pcm)).expect("加轨失败");
        senders.push(tx);
        paths.push(path);
    }

    let mut codec = OpusCodec::with_defaults().expect("构造编码器");
    let mut packet = [0_u8; MAX_PACKET_BYTES];
    let mut output: Frame = [0.0; FRAME_SAMPLES];
    let mut audio_seconds = 0.0_f64;
    let mut audio_allocations = 0_usize;
    let mut peak = 0.0_f32;
    let mut underruns = 0_u32;

    for index in 0..FRAMES {
        // 网络/工作线程侧：每路编码一个不同频率的音，塞进各自的接收路径。
        for (slot, (tx, path)) in senders.iter_mut().zip(paths.iter_mut()).enumerate() {
            let frame = tone(300.0 + 100.0 * slot as f32, index, 0.05);
            let len = codec.encode(&frame, &mut packet);
            assert!(len > 0, "编码失败");
            let seq = index as u16;
            let timestamp = (index * FRAME_SAMPLES) as u32;
            assert!(tx.send(IncomingPacket::new(
                SENDER_BASE + slot as u32,
                seq,
                timestamp,
                &packet[..len]
            )));
            path.tick();
        }

        // 音频侧：只有混音这一步。
        let before = alloc::count();
        let start = Instant::now();
        let stats = mixer.mix(&mut output);
        audio_seconds += start.elapsed().as_secs_f64();
        audio_allocations += alloc::count() - before;

        if index > 20 {
            underruns += u32::from(stats.underruns);
            peak = peak.max(stats.peak);
        }
    }

    println!(
        "{FRAMES} 帧 × {MAX_TRACKS} 路：混音 {:.4} ms/帧、分配 {audio_allocations} 次、峰值 {peak:.3}",
        audio_seconds * 1_000.0 / FRAMES as f64
    );

    assert_eq!(
        audio_allocations, 0,
        "混音路径分配了 {audio_allocations} 次——音频回调里不能有 Vec/Box/format!"
    );
    assert!(peak > 0.1, "8 路都接了却没混出声音（峰值 {peak}）");
    assert_eq!(underruns, 0, "稳定后不该有缺帧");
    assert!(peak < 1.0, "8 路各 0.05 不该削波，峰值却到了 {peak}");
}

#[test]
fn soft_limiter_cost_when_everything_is_overloaded() {
    // 正常混音碰不到拐点（上一个测试峰值 0.61），所以软限幅在那条路上是免费的。
    // 这里故意让 8 路都过载，量一下「限幅真的介入」时的代价。
    let (mut mixer, control) = Mixer::new();
    let mut senders = Vec::new();
    let mut paths = Vec::new();
    for slot in 0..MAX_TRACKS {
        let (tx, path, pcm) = ReceivePath::new(SENDER_BASE + slot as u32, 64, 8).expect("构造");
        control.add_track(slot, Box::new(pcm)).expect("加轨失败");
        senders.push(tx);
        paths.push(path);
    }

    let mut codec = OpusCodec::with_defaults().expect("构造编码器");
    let mut packet = [0_u8; MAX_PACKET_BYTES];
    let mut output: Frame = [0.0; FRAME_SAMPLES];
    let mut seconds = 0.0_f64;
    let mut limited = 0_u32;
    let mut peak = 0.0_f32;

    for index in 0..FRAMES {
        for (slot, (tx, path)) in senders.iter_mut().zip(paths.iter_mut()).enumerate() {
            let frame = tone(300.0 + 50.0 * slot as f32, index, 0.5);
            let len = codec.encode(&frame, &mut packet);
            assert!(len > 0);
            assert!(tx.send(IncomingPacket::new(
                SENDER_BASE + slot as u32,
                index as u16,
                (index * FRAME_SAMPLES) as u32,
                &packet[..len]
            )));
            path.tick();
        }
        let start = Instant::now();
        let stats = mixer.mix(&mut output);
        seconds += start.elapsed().as_secs_f64();
        if index > 20 {
            limited += stats.clipped_samples;
            peak = peak.max(stats.peak);
        }
    }

    println!(
        "过载场景：混音 {:.4} ms/帧、峰值 {peak:.2}（+{:.1} dB）、被软限幅的样本 {limited}",
        seconds * 1_000.0 / FRAMES as f64,
        20.0 * peak.log10()
    );
    assert!(peak > 1.0, "这个测试本来就要过载，峰值只有 {peak}");
    assert!(limited > 0, "过载了却没统计到被限幅的样本");
    assert!(
        output.iter().all(|x| x.abs() <= 1.0),
        "软限幅之后仍有样本超过满量程"
    );
}

#[test]
fn solo_and_mute_work_through_the_command_ring() {
    // 端到端验证控制侧：真实接收路径 + 指令环 + 混音。
    let (mut mixer, control) = Mixer::new();
    let mut senders = Vec::new();
    let mut paths = Vec::new();
    for slot in 0..2 {
        let (tx, path, pcm) = ReceivePath::new(SENDER_BASE + slot as u32, 64, 8).expect("构造");
        control.add_track(slot, Box::new(pcm)).expect("加轨失败");
        senders.push(tx);
        paths.push(path);
    }

    let mut codec = OpusCodec::with_defaults().expect("构造编码器");
    let mut packet = [0_u8; MAX_PACKET_BYTES];
    let mut output: Frame = [0.0; FRAME_SAMPLES];

    let mut run = |mixer: &mut Mixer, frame_index: usize| -> f32 {
        for (slot, (tx, path)) in senders.iter_mut().zip(paths.iter_mut()).enumerate() {
            let frame = tone(400.0, frame_index, 0.1 + 0.1 * slot as f32);
            let len = codec.encode(&frame, &mut packet);
            assert!(len > 0);
            assert!(tx.send(IncomingPacket::new(
                SENDER_BASE + slot as u32,
                frame_index as u16,
                (frame_index * FRAME_SAMPLES) as u32,
                &packet[..len]
            )));
            path.tick();
        }
        mixer.mix(&mut output);
        output.iter().fold(0.0_f32, |acc, x| acc.max(x.abs()))
    };

    let both = run(&mut mixer, 0);
    assert!(both > 0.0, "两路都在却没声音");

    control.set_solo(0, true).expect("独听失败");
    let soloed = run(&mut mixer, 1);
    assert!(soloed < both, "独听之后峰值应当变小：{soloed} vs {both}");

    control.set_muted(0, true).expect("静音失败");
    // 静音是 10 ms 淡出，不是硬切：第一帧还有残留，第二帧才是 0。
    let fading = run(&mut mixer, 2);
    assert!(
        fading < soloed,
        "静音第一帧应当已经在淡出：{fading} vs {soloed}"
    );
    let muted = run(&mut mixer, 3);
    assert_eq!(muted, 0.0, "唯一独听的轨被静音后应当完全安静");
}
