//! Linux audio capture: PipeWire + Opus encode.
//!
//! # Архитектура
//!
//! ```text
//!   PipeWire (дефолтный audio source, без Portal)
//!       │  process callback (синхронный OS-поток)
//!       │  F32LE сэмплы, stereo, 48000 Hz
//!       ▼
//!   Arc<Mutex<Vec<f32>>>  ←─ накапливаем сэмплы
//!       │
//!       │  отдельный поток — drain каждые 20ms
//!       ▼
//!   Opus encode (20ms фрейм = 960 сэмплов @ 48kHz)
//!       ▼
//!   mpsc::Sender<AudioFrame>  →  сериализатор → QUIC
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pipewire::{
    context::ContextBox,
    main_loop::MainLoopBox,
    spa::pod::PropertyFlags,
};
use pipewire::spa::sys as spa_sys;
use pw::properties::properties;
use tokio::sync::{mpsc, oneshot};

use common::{AudioFrame, FrameTrace};

use crate::capture::LinuxAudioSender;

use super::SenderError;

// ── Константы ────────────────────────────────────────────────────────────────

const SAMPLE_RATE:   u32 = 48_000;
const CHANNELS:      u32 = 2;
/// Opus фрейм 20ms @ 48kHz = 960 сэмплов на канал.
const FRAME_SAMPLES: usize = 960;

// ── Публичный тип ─────────────────────────────────────────────────────────────


// ── AudioSender impl ──────────────────────────────────────────────────────────

impl LinuxAudioSender {
    pub async fn run(self, sink: mpsc::Sender<AudioFrame>) -> Result<(), SenderError> {
        let (err_tx, err_rx) = oneshot::channel::<SenderError>();
        let rt_handle = tokio::runtime::Handle::current();

        std::thread::Builder::new()
            .name("pipewire-audio".into())
            .spawn(move || {
                let _guard = rt_handle.enter();
                if let Err(e) = run_pipewire_audio(sink) {
                    let _ = err_tx.send(e);
                }
            })
            .map_err(|e| SenderError::CaptureInit(format!("audio thread spawn: {e}")))?;

        match err_rx.await {
            Ok(e)  => Err(e),
            Err(_) => Ok(()),
        }
    }
}

// ── PipeWire audio loop (blocking OS thread) ──────────────────────────────────

fn run_pipewire_audio(sink: mpsc::Sender<AudioFrame>) -> Result<(), SenderError> {
    pw::init();

    let mainloop = MainLoopBox::new(None)
        .map_err(|e| SenderError::CaptureInit(format!("pw audio mainloop: {e}")))?;
    let context = ContextBox::new(&mainloop.loop_(), None)
        .map_err(|e| SenderError::CaptureInit(format!("pw audio context: {e}")))?;
    let core = context
        .connect(None)
        .map_err(|e| SenderError::CaptureInit(format!("pw audio connect: {e}")))?;

    // Разделяемый буфер сэмплов между PW callback и encode-потоком.
    let sample_buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sample_buf_pw = sample_buf.clone();

    let stream = pw::stream::StreamBox::new(&core, "audio-capture", properties! {
        *pw::keys::MEDIA_TYPE     => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Music",
    })
    .map_err(|e| SenderError::CaptureInit(format!("pw audio stream: {e}")))?;

    // ── Encode поток ──────────────────────────────────────────────────────────
    // Дрейним буфер каждые 20ms и кодируем Opus фреймами.
    {
        let sample_buf_enc = sample_buf.clone();
        let sink_enc = sink.clone();

        std::thread::Builder::new()
            .name("opus-encode".into())
            .spawn(move || {
                let mut encoder = opus::Encoder::new(
                    SAMPLE_RATE,
                    opus::Channels::Stereo,
                    opus::Application::LowDelay,  // минимальная латентность
                )
                .expect("Opus encoder init failed");

                // Буфер для накопления ровно FRAME_SAMPLES * CHANNELS сэмплов.
                let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * CHANNELS as usize * 2);

                loop {
                    std::thread::sleep(Duration::from_millis(5));

                    // Дрейним накопленное из PW буфера.
                    {
                        let mut lock = sample_buf_enc.lock().unwrap();
                        pending.extend_from_slice(&lock);
                        lock.clear();
                    }

                    // Кодируем полными Opus фреймами.
                    let frame_len = FRAME_SAMPLES * CHANNELS as usize;
                    while pending.len() >= frame_len {
                        let capture_us = FrameTrace::now_us();
                        let chunk: Vec<f32> = pending.drain(..frame_len).collect();

                        let mut out = vec![0u8; 4000];
                        match encoder.encode_float(&chunk, &mut out) {
                            Ok(n) => {
                                out.truncate(n);
                                let frame = AudioFrame { payload: out.into(), capture_us };
                                // non-blocking: если канал полон — дропаем фрейм
                                let _ = sink_enc.try_send(frame);
                            }
                            Err(e) => {
                                log::warn!("[Opus] encode error: {e}");
                            }
                        }
                    }

                    // Не даём pending расти бесконечно (> 200ms буфер — дроп).
                    let max_pending = SAMPLE_RATE as usize * CHANNELS as usize / 5;
                    if pending.len() > max_pending {
                        log::warn!("[Audio] encode falling behind, dropping samples");
                        pending.clear();
                    }
                }
            })
            .map_err(|e| SenderError::CaptureInit(format!("opus encode thread: {e}")))?;
    }

    // ── PipeWire process callback ─────────────────────────────────────────────
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(|_, _, old, new| {
            log::info!("[PipeWire Audio] State: {:?} -> {:?}", old, new);
        })
        .process(move |stream, _| {
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() { return; }

            unsafe {
                if (*raw).buffer.is_null() {
                    stream.queue_raw_buffer(raw);
                    return;
                }
                let spa_buf = &*(*raw).buffer;
                if spa_buf.n_datas == 0 || spa_buf.datas.is_null() {
                    stream.queue_raw_buffer(raw);
                    return;
                }

                let data  = &*spa_buf.datas;
                let chunk = match data.chunk.as_ref() {
                    Some(c) => c,
                    None    => { stream.queue_raw_buffer(raw); return; }
                };

                let size   = chunk.size   as usize;
                let offset = chunk.offset as usize;

                if size == 0 {
                    stream.queue_raw_buffer(raw);
                    return;
                }

                // PipeWire даёт F32LE сэмплы через MemPtr.
                if data.type_ == spa_sys::SPA_DATA_MemPtr && !data.data.is_null() {
                    let byte_slice = std::slice::from_raw_parts(
                        (data.data as *const u8).add(offset),
                        size,
                    );
                    // Интерпретируем байты как f32 (little-endian, нативный порядок на x86).
                    let n_samples = size / std::mem::size_of::<f32>();
                    let float_ptr = byte_slice.as_ptr() as *const f32;
                    let floats = std::slice::from_raw_parts(float_ptr, n_samples);

                    let mut lock = sample_buf_pw.lock().unwrap();
                    lock.extend_from_slice(floats);
                }

                stream.queue_raw_buffer(raw);
            }
        })
        .register()
        .map_err(|e| SenderError::CaptureInit(format!("pw audio listener: {e}")))?;

    // ── SPA параметры: Audio/raw/F32LE/stereo/48kHz ───────────────────────────
    let spa_format = build_spa_audio_params();
    let mut params = [
        pw::spa::pod::Pod::from_bytes(&spa_format)
            .ok_or_else(|| SenderError::CaptureInit("invalid SPA audio format".into()))?,
    ];

    stream.connect(
        pw::spa::utils::Direction::Input,
        None,  // без node_id — подключаемся к дефолтному source
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )
    .map_err(|e| SenderError::CaptureInit(format!("pw audio stream connect: {e}")))?;

    log::info!("[PipeWire Audio] Capture stream started.");
    mainloop.run();
    Ok(())
}

// ── SPA audio format pod ──────────────────────────────────────────────────────

fn build_spa_audio_params() -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::sys::*;

    let value = pw::spa::pod::Value::Object(pw::spa::pod::Object {
        type_: SPA_TYPE_OBJECT_Format,
        id:    SPA_PARAM_EnumFormat,
        properties: vec![
            pw::spa::pod::Property {
                key:   SPA_FORMAT_mediaType,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_TYPE_audio)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_mediaSubtype,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_SUBTYPE_raw)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_AUDIO_format,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_AUDIO_FORMAT_F32_LE)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_AUDIO_rate,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(SAMPLE_RATE as i32),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_AUDIO_channels,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(CHANNELS as i32),
            },
        ],
    });

    let mut bytes = Vec::new();
    PodSerializer::serialize(&mut std::io::Cursor::new(&mut bytes), &value).unwrap();
    bytes
}