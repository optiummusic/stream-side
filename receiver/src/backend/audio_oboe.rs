use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use oboe::{
    AudioOutputCallback, AudioStreamAsync, AudioStreamBuilder,
    DataCallbackResult, Output, PerformanceMode, SharingMode,
    AudioOutputStreamSafe, Stereo, AudioStream
};
use opus_decoder::OpusDecoder;
use crate::backend::audio_output::AudioOutput;

// Используем VecDeque для O(1) удаления из начала
type SharedBuf = Arc<Mutex<VecDeque<f32>>>;

struct OboeCallback {
    buffer: SharedBuf,
}

impl AudioOutputCallback for OboeCallback {
    type FrameType = (f32, Stereo);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn AudioOutputStreamSafe,
        frames: &mut [(f32, f32)],
    ) -> DataCallbackResult {
        // Стараемся держать лок как можно меньше
        if let Ok(mut lock) = self.buffer.lock() {
            for frame in frames.iter_mut() {
                // Извлекаем сэмплы без сдвига всего массива
                frame.0 = lock.pop_front().unwrap_or(0.0); // L
                frame.1 = lock.pop_front().unwrap_or(0.0); // R
            }
        } else {
            // Если мьютекс отравлен, гоним тишину
            for frame in frames.iter_mut() {
                *frame = (0.0, 0.0);
            }
        }
        DataCallbackResult::Continue
    }
}

pub struct OboeAudioOutput {
    buffer:  SharedBuf,
    decoder: Option<OpusDecoder>, // Делаем Option на случай ошибки инициализации
    _stream: Option<AudioStreamAsync<Output, OboeCallback>>,
}

impl AudioOutput for OboeAudioOutput {
    fn new() -> Self {
        let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(48_000)));
        let callback = OboeCallback { buffer: buffer.clone() };

        // Пытаемся открыть поток
        let stream_result = AudioStreamBuilder::default()
            .set_performance_mode(PerformanceMode::LowLatency)
            .set_sharing_mode(SharingMode::Shared)
            .set_sample_rate(48_000)
            .set_channel_count::<Stereo>()
            .set_format::<f32>()
            .set_callback(callback)
            .open_stream();

        let mut stream = match stream_result {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("[Oboe] Failed to open stream: {:?}", e);
                None
            }
        };

        // Пытаемся запустить поток
        if let Some(s) = stream.as_mut() {
            if let Err(e) = s.start() {
                log::error!("[Oboe] Failed to start stream: {:?}", e);
                // Если не стартовал, лучше вообще закрыть
                stream = None;
            }
        }

        // Инициализируем декодер
        let decoder = match OpusDecoder::new(48_000, 2) {
            Ok(d) => Some(d),
            Err(e) => {
                log::error!("[Opus] Decoder init failed: {:?}", e);
                None
            }
        };

        Self { 
            buffer, 
            decoder, 
            _stream: stream 
        }
    }

    fn push_opus(&mut self, payload: &[u8]) {
        // Если декодера нет — ловить нечего
        let decoder = match self.decoder.as_mut() {
            Some(d) => d,
            None => return,
        };

        let mut pcm = vec![0f32; 960 * 2]; 
        match decoder.decode_float(payload, &mut pcm, false) {
            Ok(n) => {
                if let Ok(mut lock) = self.buffer.lock() {
                    lock.extend(pcm[..n * 2].iter());
                    
                    // Лимит 200мс (48000 * 2 канала * 0.2 сек)
                    let max = 19_200; 
                    if lock.len() > max {
                        let to_drain = lock.len() - max;
                        lock.drain(..to_drain);
                    }
                }
            }
            Err(e) => log::warn!("[Opus] decode error: {e:?}"),
        }
    }
}