#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use sender::encode::{EncodedFrame, Encoder};
use stream_receiver::backend::desktop::DesktopFfmpegBackend;
use stream_receiver::backend::{BackendError, FrameOutput, VideoBackend};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

fn make_bgra_frame(width: u32, height: u32, frame_no: u64) -> Vec<u8> {
    let mut frame = vec![0u8; (width as usize) * (height as usize) * 4];
    let stride = (width as usize) * 4;

    for y in 0..height as usize {
        for x in 0..width as usize {
            let i = y * stride + x * 4;
            frame[i] = ((x as u64 + frame_no) & 0xFF) as u8;
            frame[i + 1] = ((y as u64 + frame_no * 2) & 0xFF) as u8;
            frame[i + 2] = (((x + y) as u64 + frame_no * 3) & 0xFF) as u8;
            frame[i + 3] = 255;
        }
    }

    frame
}

fn generate_keyframe_payload() -> Vec<u8> {
    let rt = Runtime::new().expect("tokio runtime");
    let (sink_tx, mut sink_rx) = mpsc::channel::<EncodedFrame>(64);
    let encoder = Encoder::new(WIDTH, HEIGHT, sink_tx);
    let frame = make_bgra_frame(WIDTH, HEIGHT, 42);

    // Push a few dummy frames; we only need the first keyframe payload.
    for i in 0..8u64 {
        encoder.encode_bgra(&frame, i);
    }

    loop {
        let out = rt.block_on(async { sink_rx.recv().await.expect("encoded frame") });
        if out.is_key {
            return out.payload;
        }
    }
}

fn decode_one_frame(
    backend: &mut DesktopFfmpegBackend,
    payload: &[u8],
    frame_id: u64,
) -> Result<(), BackendError> {
    backend.push_encoded(payload, frame_id, None)?;

    loop {
        match backend.poll_output()? {
            FrameOutput::Yuv(_frame) => return Ok(()),
            FrameOutput::Pending => continue,
            FrameOutput::DirectToSurface => continue,
            _ => continue,
        }
    }
}

fn bench_decode(c: &mut Criterion) {
    let payload = generate_keyframe_payload();
    let mut backend = DesktopFfmpegBackend::new().expect("decoder backend");

    // Warm-up: populate decoder/scaler state before measuring.
    decode_one_frame(&mut backend, &payload, 0).expect("warmup decode");

    let mut group = c.benchmark_group("receiver_decode");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("decode_hevc_1080p_end_to_end", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let started = Instant::now();
                decode_one_frame(&mut backend, black_box(&payload), i + 1)
                    .expect("decode frame");
                total += started.elapsed();
            }
            total
        });
    });

    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
