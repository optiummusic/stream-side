// #![cfg(target_os = "linux")]

// use std::time::{Duration, Instant};

// use criterion::{black_box, criterion_group, criterion_main, Criterion};
// use tokio::runtime::Runtime;
// use tokio::sync::mpsc;

// use sender::encode::{EncodedFrame, Encoder};

// const WIDTH: u32 = 1920;
// const HEIGHT: u32 = 1080;

// fn make_bgra_frame(width: u32, height: u32, frame_no: u64) -> Vec<u8> {
//     let mut frame = vec![0u8; (width as usize) * (height as usize) * 4];
//     let stride = (width as usize) * 4;

//     for y in 0..height as usize {
//         for x in 0..width as usize {
//             let i = y * stride + x * 4;
//             frame[i] = ((x as u64 + frame_no) & 0xFF) as u8;
//             frame[i + 1] = ((y as u64 + frame_no * 2) & 0xFF) as u8;
//             frame[i + 2] = (((x + y) as u64 + frame_no * 3) & 0xFF) as u8;
//             frame[i + 3] = 255;
//         }
//     }

//     frame
// }

// fn bench_encode(c: &mut Criterion) {
//     let rt = Runtime::new().expect("tokio runtime");
//     let (sink_tx, mut sink_rx) = mpsc::channel::<EncodedFrame>(64);
//     let encoder = Encoder::new(WIDTH, HEIGHT, sink_tx);
//     let frame = make_bgra_frame(WIDTH, HEIGHT, 1);

//     // Warm-up until the first frame makes it all the way through the worker.
//     encoder.encode_bgra(&frame, 0);
//     let _ = rt.block_on(async { sink_rx.recv().await.expect("warmup frame") });

//     let mut group = c.benchmark_group("sender_encode");
//     group.sample_size(20);
//     group.measurement_time(Duration::from_secs(10));

//     group.bench_function("encode_bgra_1080p_end_to_end", |b| {
//         b.iter_custom(|iters| {
//             let mut total = Duration::ZERO;
//             for i in 0..iters {
//                 let started = Instant::now();
//                 encoder.encode_bgra(black_box(&frame), i + 1);
//                 let _encoded = rt.block_on(async {
//                     sink_rx.recv().await.expect("encoded frame")
//                 });
//                 total += started.elapsed();
//             }
//             total
//         });
//     });

//     group.finish();
// }

// criterion_group!(benches, bench_encode);
// criterion_main!(benches);
fn main() {}
