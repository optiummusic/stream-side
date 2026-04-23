use std::sync::{Arc, Mutex};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use opus_decoder::OpusDecoder;

use crate::backend::audio_output::AudioOutput;

pub struct CpalAudioOutput {
    buffer:  Arc<Mutex<Vec<f32>>>,
    decoder: OpusDecoder,
}

impl AudioOutput for CpalAudioOutput {
    fn new() -> Self {
        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let buffer_clone = buffer.clone();

        // Opus декодер
        let decoder = OpusDecoder::new(48_000, 2)
            .expect("Opus decoder init failed");

        // cpal output stream — запускается один раз и живёт вечно
        std::thread::spawn(move || {
            let host   = cpal::default_host();
            let device = host.default_output_device().expect("no output device");
            let config = cpal::StreamConfig {
                channels:    2,
                sample_rate: 48_000,
                buffer_size: cpal::BufferSize::Default,
            };

            let buf_clone = buffer_clone.clone();
            let stream = device.build_output_stream(
                &config,
                move |output: &mut [f32], _| {
                    let mut lock = buf_clone.lock().unwrap();
                    let available = lock.len().min(output.len());
                    output[..available].copy_from_slice(&lock[..available]);
                    lock.drain(..available);
                    // тишина если буфер пуст
                    output[available..].fill(0.0);
                },
                |e| log::error!("[Audio] stream error: {e}"),
                None,
            ).expect("build_output_stream failed");

            stream.play().expect("stream play failed");
            // держим поток живым
            loop { std::thread::sleep(std::time::Duration::from_secs(60)); }
        });

        Self { buffer, decoder }
    }

    /// Принимает сырой Opus пакет, декодирует и кладёт в буфер воспроизведения.
    fn push_opus(&mut self, payload: &[u8]) {
        let mut pcm = vec![0f32; 960 * 2]; // 20ms stereo
        match self.decoder.decode_float(payload, &mut pcm, false) {
            Ok(n) => {
                let mut lock = self.buffer.lock().unwrap();
                lock.extend_from_slice(&pcm[..n * 2]);
                
                // Не даём буферу расти > 200ms
                let max_samples = 48_000 * 2 / 5;
                if lock.len() > max_samples {
                    let to_remove = lock.len() - max_samples; // Вычисляем заранее
                    lock.drain(..to_remove);
                }
            }
            Err(e) => log::warn!("[Opus] decode error: {e}"),
        }
    }
}