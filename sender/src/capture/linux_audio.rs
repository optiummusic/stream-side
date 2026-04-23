//! Linux audio capture: PipeWire + high-pass filter + VAD + Opus encode.
//!
//! # Архитектура
//!
//! ```text
//!   PipeWire stream #1 (system sink monitor)      PipeWire stream #2 (microphone source)
//!   STREAM_CAPTURE_SINK=true                       дефолтный input source
//!       │  F32LE, stereo, 48000 Hz                     │  F32LE, stereo, 48000 Hz
//!       ▼                                              ▼
//!   Arc<Mutex<Vec<f32>>>  system_buf          Arc<Mutex<Vec<f32>>>  mic_buf
//!              \                              /
//!               └──────── mix-and-encode ────┘
//!                    (drain каждые 5ms)
//!                         │
//!                   [mic] high-pass IIR фильтр (срез ~80Hz)
//!                         │   убирает DC, гул вентиляторов, удары по столу
//!                         │
//!                   [оба] VAD: RMS² < порога → тихо
//!                         │   оба тихих → фрейм пропускается (не кодируется)
//!                         │
//!                   mix = (sys + mic) * 0.5   — нормализованный, без клипинга
//!                         │
//!                   Opus encode (20ms фрейм = 960 сэмплов/канал)
//!                         ▼
//!                   mpsc::Sender<AudioFrame>  →  сериализатор → QUIC
//! ```
//!
//! ## VAD
//! Энергия фрейма = RMS² (среднее квадратов сэмплов).
//! Константа `VAD_ENERGY_THRESHOLD` ≈ -60 dBFS — ниже считаем тишиной.
//! Проверяется независимо для каждого источника; кодируем только когда
//! хотя бы один источник активен.
//!
//! ## High-pass фильтр
//! Однополюсный IIR: `y[n] = α·(y[n-1] + x[n] - x[n-1])`, α ≈ 0.9895 @ 80 Hz/48kHz.
//! Применяется к каждому каналу микрофона независимо (interleaved stereo).
//! Новых зависимостей не требует — pure Rust.

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

const SAMPLE_RATE:   u32   = 48_000;
const CHANNELS:      u32   = 2;
/// Opus фрейм 20ms @ 48kHz = 960 сэмплов на канал.
const FRAME_SAMPLES: usize = 960;

/// RMS² порог для VAD. 1e-6 ≈ -60 dBFS.
/// Ниже этого — считаем фрейм тишиной и не кодируем.
const VAD_ENERGY_THRESHOLD: f32 = 1e-6;

/// Коэффициент однополюсного IIR high-pass фильтра.
/// α = 1 / (1 + 2π · f_c / f_s),  f_c = 80 Hz,  f_s = 48000 Hz
/// α ≈ 0.98954
const HPF_ALPHA: f32 = 0.989_54;

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

    // ── Разделяемые буферы ────────────────────────────────────────────────────
    let system_buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let mic_buf:    Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));

    // ── Стрим #1: системный звук (sink monitor) ───────────────────────────────
    let system_buf_pw = system_buf.clone();

    let stream_system = pw::stream::StreamBox::new(&core, "audio-capture-system", properties! {
        *pw::keys::MEDIA_TYPE          => "Audio",
        *pw::keys::MEDIA_CATEGORY      => "Capture",
        *pw::keys::MEDIA_ROLE          => "Music",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
    })
    .map_err(|e| SenderError::CaptureInit(format!("pw system stream: {e}")))?;

    // ── Стрим #2: микрофон (дефолтный input source) ───────────────────────────
    let mic_buf_pw = mic_buf.clone();

    let stream_mic = pw::stream::StreamBox::new(&core, "audio-capture-mic", properties! {
        *pw::keys::MEDIA_TYPE     => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Communication",
        // STREAM_CAPTURE_SINK не выставляем → подключаемся к микрофону
    })
    .map_err(|e| SenderError::CaptureInit(format!("pw mic stream: {e}")))?;

    // ── Mix-and-encode поток ──────────────────────────────────────────────────
    {
        let system_buf_enc = system_buf.clone();
        let mic_buf_enc    = mic_buf.clone();
        let sink_enc       = sink.clone();

        std::thread::Builder::new()
            .name("opus-mix-encode".into())
            .spawn(move || {
                let mut encoder = opus::Encoder::new(
                    SAMPLE_RATE,
                    opus::Channels::Stereo,
                    opus::Application::LowDelay,
                )
                .expect("Opus encoder init failed");

                let frame_len   = FRAME_SAMPLES * CHANNELS as usize;
                let max_pending = SAMPLE_RATE as usize * CHANNELS as usize / 5; // ~200ms

                let mut sys_pending: Vec<f32> = Vec::with_capacity(frame_len * 2);
                let mut mic_pending: Vec<f32> = Vec::with_capacity(frame_len * 2);

                // Состояние IIR high-pass фильтра — по одному значению на канал.
                let mut hpf_prev_in:  [f32; 2] = [0.0; 2];
                let mut hpf_prev_out: [f32; 2] = [0.0; 2];

                loop {
                    std::thread::sleep(Duration::from_millis(5));

                    // Дрейним из PW-буферов в локальные pending.
                    {
                        let mut lock = system_buf_enc.lock().unwrap();
                        sys_pending.extend_from_slice(&lock);
                        lock.clear();
                    }
                    {
                        let mut lock = mic_buf_enc.lock().unwrap();
                        mic_pending.extend_from_slice(&lock);
                        lock.clear();
                    }

                    // Кодируем полными Opus фреймами пока оба буфера достаточно заполнены.
                    while sys_pending.len() >= frame_len && mic_pending.len() >= frame_len {
                        let capture_us = FrameTrace::now_us();

                        let sys_chunk: Vec<f32> = sys_pending.drain(..frame_len).collect();
                        let mut mic_chunk: Vec<f32> = mic_pending.drain(..frame_len).collect();

                        // ── [mic] High-pass фильтр ────────────────────────────
                        // Убираем DC offset, гул (<80 Hz) до оценки VAD,
                        // чтобы низкочастотный фон не «включал» микрофон.
                        apply_highpass(&mut mic_chunk, &mut hpf_prev_in, &mut hpf_prev_out);

                        // ── VAD: пропускаем фрейм если оба источника тихие ────
                        let sys_active = rms_squared(&sys_chunk) > VAD_ENERGY_THRESHOLD;
                        let mic_active = rms_squared(&mic_chunk) > VAD_ENERGY_THRESHOLD;

                        if !sys_active && !mic_active {
                            log::trace!("[VAD] both silent — skipping frame");
                            continue;
                        }

                        // ── Нормализованный микс (sys + mic) / 2 ─────────────
                        // Делим на 2 вместо clamp: никогда не клипируем,
                        // при этом если один источник молчит — он просто
                        // добавляет нули и итоговая громкость не режется вдвое
                        // (тихий источник после HPF+VAD ≈ 0).
                        let mixed: Vec<f32> = sys_chunk.iter()
                            .zip(mic_chunk.iter())
                            .map(|(s, m)| (s + m) * 0.5)
                            .collect();

                        // ── Opus encode ───────────────────────────────────────
                        let mut out = vec![0u8; 4000];
                        match encoder.encode_float(&mixed, &mut out) {
                            Ok(n) => {
                                out.truncate(n);
                                let frame = AudioFrame { payload: out.into(), capture_us };
                                // non-blocking: канал полон → дропаем фрейм
                                let _ = sink_enc.try_send(frame);
                            }
                            Err(e) => {
                                log::warn!("[Opus] encode error: {e}");
                            }
                        }
                    }

                    // Защита от бесконечного роста (> 200ms → дроп).
                    if sys_pending.len() > max_pending {
                        log::warn!("[Audio] system buf overflow, dropping samples");
                        sys_pending.clear();
                    }
                    if mic_pending.len() > max_pending {
                        log::warn!("[Audio] mic buf overflow, dropping samples");
                        mic_pending.clear();
                    }
                }
            })
            .map_err(|e| SenderError::CaptureInit(format!("opus encode thread: {e}")))?;
    }

    // ── PipeWire callbacks ────────────────────────────────────────────────────
    let _listener_system = stream_system
        .add_local_listener::<()>()
        .state_changed(|_, _, old, new| {
            log::info!("[PipeWire System] State: {:?} -> {:?}", old, new);
        })
        .process(move |stream, _| {
            pw_read_samples(stream, &system_buf_pw);
        })
        .register()
        .map_err(|e| SenderError::CaptureInit(format!("pw system listener: {e}")))?;

    let _listener_mic = stream_mic
        .add_local_listener::<()>()
        .state_changed(|_, _, old, new| {
            log::info!("[PipeWire Mic] State: {:?} -> {:?}", old, new);
        })
        .process(move |stream, _| {
            pw_read_samples(stream, &mic_buf_pw);
        })
        .register()
        .map_err(|e| SenderError::CaptureInit(format!("pw mic listener: {e}")))?;

    // ── SPA параметры (одинаковые для обоих стримов) ──────────────────────────
    let spa_fmt1 = build_spa_audio_params();
    let pod1 = pw::spa::pod::Pod::from_bytes(&spa_fmt1)
        .ok_or_else(|| SenderError::CaptureInit("invalid SPA audio format".into()))?;
    let mut params_system = [pod1];

    let spa_fmt2 = build_spa_audio_params();
    let pod2 = pw::spa::pod::Pod::from_bytes(&spa_fmt2)
        .ok_or_else(|| SenderError::CaptureInit("invalid SPA audio format (mic)".into()))?;
    let mut params_mic = [pod2];

    stream_system.connect(
        pw::spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params_system,
    )
    .map_err(|e| SenderError::CaptureInit(format!("pw system stream connect: {e}")))?;

    stream_mic.connect(
        pw::spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params_mic,
    )
    .map_err(|e| SenderError::CaptureInit(format!("pw mic stream connect: {e}")))?;

    log::info!("[PipeWire Audio] System + mic capture streams started.");
    mainloop.run();
    Ok(())
}

// ── DSP хелперы ───────────────────────────────────────────────────────────────

/// Однополюсный IIR high-pass фильтр для interleaved stereo буфера.
///
/// Формула: `y[n] = α · (y[n-1] + x[n] - x[n-1])`
///
/// Состояние фильтра (`prev_in`, `prev_out`) сохраняется между вызовами —
/// нет разрывов на границах фреймов.
fn apply_highpass(
    samples:  &mut [f32],
    prev_in:  &mut [f32; 2],   // prev_in[ch]  — последний вход канала ch
    prev_out: &mut [f32; 2],   // prev_out[ch] — последний выход канала ch
) {
    for (i, s) in samples.iter_mut().enumerate() {
        let ch = i % CHANNELS as usize; // 0 = L, 1 = R
        let x  = *s;
        let y  = HPF_ALPHA * (prev_out[ch] + x - prev_in[ch]);
        prev_in[ch]  = x;
        prev_out[ch] = y;
        *s = y;
    }
}

/// Вычисляет RMS² (среднее квадратов) фрейма.
/// Используется как быстрый энергетический VAD без sqrt.
#[inline]
fn rms_squared(samples: &[f32]) -> f32 {
    if samples.is_empty() { return 0.0; }
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

/// Читает F32LE сэмплы из PW буфера в разделяемый Vec.
fn pw_read_samples(
    stream: &pipewire::stream::Stream,
    buf:    &Arc<Mutex<Vec<f32>>>,
) {
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

        if data.type_ == spa_sys::SPA_DATA_MemPtr && !data.data.is_null() {
            let byte_slice = std::slice::from_raw_parts(
                (data.data as *const u8).add(offset),
                size,
            );
            let n_samples = size / std::mem::size_of::<f32>();
            let float_ptr = byte_slice.as_ptr() as *const f32;
            let floats    = std::slice::from_raw_parts(float_ptr, n_samples);

            buf.lock().unwrap().extend_from_slice(floats);
        }

        stream.queue_raw_buffer(raw);
    }
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