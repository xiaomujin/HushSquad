//! P3-7 的门禁：把「会分配的环节」包进 `WorkerStage` 之后，音频侧回到零分配，工作线程跟得上实时。
//!
//! 只在 `--features denoise-dfn3` 下跑：这里要验证的正是真实的 DFN3（每帧约 185 次分配）。

#![cfg(feature = "denoise-dfn3")]
// 测量测试要打印数字。
#![allow(clippy::print_stdout)]

use std::time::Instant;

use audio_core::stages::Dfn3;
use audio_core::{FRAME_SAMPLES, Frame, Stage, WorkerStage};

use testkit::alloc;
use testkit::{baseline, wav};

const FRAMES: usize = 300;
const DEPTH: usize = 2;

#[test]
fn dfn3_on_a_worker_keeps_the_audio_side_clean_and_keeps_up() {
    let path = baseline::default_root().join("clean_48k_mono.wav");
    let samples = wav::read_mono_f32(&path).expect("读素材失败");
    let take = (FRAMES * FRAME_SAMPLES).min(samples.len());
    let frames = take / FRAME_SAMPLES;
    let input: Vec<Frame> = samples[..take].as_chunks::<FRAME_SAMPLES>().0.to_vec();

    // 参考：同一个环节、同一串输入，直接跑（不进队列）。
    let mut direct = Dfn3::new();
    let mut expected: Vec<Frame> = Vec::with_capacity(frames);
    for chunk in &input {
        let mut frame = *chunk;
        direct.process(&mut frame);
        expected.push(frame);
    }

    let (mut stage, mut worker) = WorkerStage::new(Box::new(Dfn3::new()), DEPTH);
    let mut audio_allocations = 0_usize;
    let mut worker_allocations = 0_usize;
    let mut audio_seconds = 0.0_f64;
    let mut worker_seconds = 0_f64;
    let mut mismatched = 0_usize;
    let mut nonzero = 0_usize;

    for (index, chunk) in input.iter().enumerate() {
        let mut frame = *chunk;

        let before = alloc::count();
        let start = Instant::now();
        stage.process(&mut frame);
        audio_seconds += start.elapsed().as_secs_f64();
        audio_allocations += alloc::count() - before;

        let before = alloc::count();
        let start = Instant::now();
        worker.run_pending();
        worker_seconds += start.elapsed().as_secs_f64();
        worker_allocations += alloc::count() - before;

        // 队列语义：第 index 帧的输出 = 直接处理第 index - DEPTH 帧的结果。
        if let Some(reference) = expected.get(index.wrapping_sub(DEPTH))
            && index >= DEPTH
        {
            if frame != *reference {
                mismatched += 1;
            }
            if frame.iter().any(|x| *x != 0.0) {
                nonzero += 1;
            }
        }
    }

    let budget = frames as f64 * 0.010;
    println!(
        "{frames} 帧：音频侧 {:.4} ms/帧、分配 {audio_allocations} 次；工作线程 {:.3} ms/帧、分配 {worker_allocations} 次（{:.0}/帧）",
        audio_seconds * 1_000.0 / frames as f64,
        worker_seconds * 1_000.0 / frames as f64,
        worker_allocations as f64 / frames as f64,
    );
    println!(
        "工作线程占用 {:.1}% 的实时预算（{:.2} s / {:.2} s）；有内容的输出帧 {nonzero}/{}",
        worker_seconds / budget * 100.0,
        worker_seconds,
        budget,
        frames - DEPTH
    );

    assert_eq!(
        audio_allocations, 0,
        "音频侧分配了 {audio_allocations} 次——队列里不该有任何分配"
    );
    assert!(
        worker_allocations > 0,
        "工作线程侧也没分配，这个测试证明不了任何事"
    );
    assert_eq!(
        mismatched, 0,
        "{mismatched} 帧的输出与「直接处理第 index-{DEPTH} 帧」不一致——队列的延迟或顺序错了"
    );
    assert!(
        worker_seconds < budget,
        "工作线程花了 {worker_seconds:.2} s，超过 {budget:.2} s 的实时预算——DFN3 在音频线程之外也跟不上"
    );
    assert_eq!(stage.stats().underruns, 0, "手工驱动不该有下溢");
}
