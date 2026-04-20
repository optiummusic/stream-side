//! Linux screen-capture sender: XDG Desktop Portal + PipeWire + VAAPI HEVC.
//!
//! # Архитектура
//!
//! ```text
//!   XDG Portal (ashpd)
//!       │  open_pipe_wire_remote()
//!       ▼
//!   PipeWire stream (SpA BGRA кадры, ~60 fps)
//!       │  process callback (синхронный OS-поток)
//!       │
//!       ├─ SPA_DATA_DmaBuf ──► dup(fd) ──► queue_raw_buffer() ──► Encoder::encode_dmabuf()
//!       │                                                              │
//!       │                                    DRM_PRIME(BGRA) frame ◄──┘
//!       │                                    av_hwframe_transfer_data (GPU/VPP)
//!       │                                    VAAPI(NV12) ──► hevc_vaapi
//!       │
//!       └─ SPA_DATA_MemPtr/MemFd ──► mmap/ptr ──► Encoder::encode_bgra()
//!                                                     │
//!                                   copy_from_slice ◄─┘
//!                                   SwsScale (CPU BGRA→NV12)
//!                                   av_hwframe_transfer_data
//!                                   hevc_vaapi
//!       ▼
//!   mpsc::Sender<EncodedFrame>  →  QuicServer serialiser task
//! ```
//!
//! ## DMA-BUF zero-copy
//!
//! При `SPA_DATA_DmaBuf` PipeWire передаёт DMA-BUF fd. Мы:
//! 1. Делаем `dup(fd)` — независимый дескриптор, держащий DMA-BUF живым.
//! 2. Немедленно возвращаем PipeWire-буфер (`queue_raw_buffer`).
//! 3. Передаём dup-нутый fd в энкодер; тот делает GPU-копию
//!    (`av_hwframe_transfer_data`: DRM_PRIME → VAAPI) и закрывает fd.
//!
//! CPU не касается пиксельных данных.
//!
//! ## Потоки
//!
//! PipeWire mainloop исполняется на выделенном OS-потоке, т.к.
//! `mainloop.run()` — блокирующий C-вызов. `oneshot`-канал пробрасывает
//! фатальные ошибки обратно в async-мир.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    os::{fd::FromRawFd, unix::io::IntoRawFd},
    sync::Arc,
    time::Duration,
};

use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode,
};
use common::GpuVendor;
use libc::c_int;
use pipewire::spa::pod::Value;
use pipewire::spa::utils::Id;
use pipewire as pw;
use pipewire::{
    context::ContextBox,
    main_loop::MainLoopBox,
    spa::pod::PropertyFlags,
};
use pipewire::spa::sys as spa_sys;
use pw::properties::properties;
use tokio::sync::{mpsc, oneshot};

use crate::Watchers;
use crate::capture::FrameGate;
use crate::encode::{self, EncodedFrame};

use super::SenderError;
use super::super::encode::{AnyEncoder, HwEncoder};

// ─────────────────────────────────────────────────────────────────────────────
// Публичный тип
// ─────────────────────────────────────────────────────────────────────────────

/// Linux screen-capture sender: XDG Portal + PipeWire + VAAPI HEVC.
pub struct LinuxPipeWireSender {
    width:  u32,
    height: u32,
    watchers: Watchers
}

impl LinuxPipeWireSender {
    /// Создать sender для захвата и кодирования в разрешении `width × height`.
    pub fn new(width: u32, height: u32, watchers: Watchers) -> Self {
        Self { width, height, watchers }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VideoSender impl
// ─────────────────────────────────────────────────────────────────────────────

impl super::VideoSender for LinuxPipeWireSender {
    type Error = SenderError;

    async fn run(
        mut self,
        sink: mpsc::Sender<EncodedFrame>,
    ) -> Result<(), SenderError> {
        let (node_id, raw_fd, portal_size) = run_portal()
            .await
            .map_err(|e| SenderError::CaptureInit(e.to_string()))?;

        let (err_tx, err_rx) = oneshot::channel::<SenderError>();

        if let Some((w, h)) = portal_size {
            log::info!("[Portal] Detected target size: {}x{}", w, h);
            self.width = w as u32;
            self.height = h as u32;
        }

        let width  = self.width;
        let height = self.height;
        let rt_handle = tokio::runtime::Handle::current();

        std::thread::Builder::new()
            .name("pipewire-capture".into())
            .spawn(move || {
                let _guard = rt_handle.enter();
                if let Err(e) = run_pipewire(node_id, raw_fd, width, height, sink, self.watchers) {
                    let _ = err_tx.send(e);
                }
            })
            .map_err(|e| SenderError::CaptureInit(format!("thread spawn: {e}")))?;

        match err_rx.await {
            Ok(e)  => Err(e),
            Err(_) => Ok(()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Portal negotiation
// ─────────────────────────────────────────────────────────────────────────────

async fn run_portal() -> ashpd::Result<(u32, i32, Option<(i32, i32)>)> {
    let proxy   = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;

    proxy.select_sources(
        &session,
        SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(SourceType::Monitor | SourceType::Window | SourceType::Virtual)
            .set_multiple(false)
            .set_persist_mode(PersistMode::DoNot),
    )
    .await?;

    let response = proxy.start(&session, None, Default::default()).await?.response()?;
    let stream = &response.streams()[0];
    let node_id  = response.streams()[0].pipe_wire_node_id();
    let fd       = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
    let size = stream.size();

    Ok((node_id, fd.into_raw_fd(), size))
}

// ─────────────────────────────────────────────────────────────────────────────
// PipeWire capture loop (blocking OS thread)
// ─────────────────────────────────────────────────────────────────────────────

fn run_pipewire(
    node_id: u32,
    raw_fd:  i32,
    width:   u32,
    height:  u32,
    sink:    mpsc::Sender<EncodedFrame>,
    watchers: Watchers
) -> Result<(), SenderError> {
    pw::init();

    let mainloop = MainLoopBox::new(None)
        .map_err(|e| SenderError::CaptureInit(format!("pw mainloop: {e}")))?;
    let context  = ContextBox::new(&mainloop.loop_(), None)
        .map_err(|e| SenderError::CaptureInit(format!("pw context: {e}")))?;

    let core = context
        .connect_fd(
            unsafe { std::os::unix::io::OwnedFd::from_raw_fd(raw_fd) },
            None,
        )
        .map_err(|e| SenderError::CaptureInit(format!("pw connect_fd: {e}")))?;

    let current_modifier = Arc::new(AtomicU64::new(0)); 
    let modifier_for_param = current_modifier.clone();
    
    let pw_width  = Rc::new(Cell::new(width));
    let pw_height = Rc::new(Cell::new(height));
    let w_clone = pw_width.clone();
    let h_clone = pw_height.clone();

    
    let stream = pw::stream::StreamBox::new(&core, "capture", properties! {
        *pw::keys::MEDIA_TYPE     => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Screen",
    })
    .map_err(|e| SenderError::CaptureInit(format!("pw stream: {e}")))?;

    let mut encoder: Option<AnyEncoder> = None;
    let mut fps_counter = 0u32;
    let mut fps_tick    = std::time::Instant::now();

    use common::FrameTrace;
    let mut fps_gate = FrameGate::new(watchers.capture_fps_rx);
    
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(|stream, _user_data, old, new| { 
        log::info!(
            "[PipeWire] Stream state changed: {:?} -> {:?}", 
            old, new
            );
        })
        .param_changed(move |_stream, _data, id, param| {
            if id != pw::spa::sys::SPA_PARAM_EnumFormat && id != pw::spa::sys::SPA_PARAM_Format {
                return;
            }

            let param = match param {
                Some(p) => p,
                None => return,
            };

            let obj = match param.as_object() {
                Ok(o) => o,
                Err(_) => return,
            };

            if let Some(prop) = obj.find_prop(Id(pw::spa::sys::SPA_FORMAT_VIDEO_size)) {
                // Используем .get_rectangle(), который возвращает Result
                if let Ok(rect) = prop.value().get_rectangle() {
                    w_clone.set(rect.width);
                    h_clone.set(rect.height);
                    log::debug!("[PipeWire] Negotiated stream size: {}x{}", rect.width, rect.height);
                }
            }

            if let Some(prop) = obj.find_prop(Id(pw::spa::sys::SPA_FORMAT_VIDEO_modifier)) {
                let val = prop.value();

                if val.is_long() {
                    if let Ok(m) = val.get_long() {
                        log::info!("[PipeWire] modifier: {:#x}", m);
                        modifier_for_param.store(m as u64, Ordering::SeqCst);
                    }
                }
        }
    })
        .process(move |stream, _| {
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() { return; }

            let capture_us = FrameTrace::now_us();

            unsafe {
                // Валидируем буфер.
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
                let offset = chunk.offset;
                let stride = chunk.stride as u32;

                if size == 0 {
                    stream.queue_raw_buffer(raw);
                    return;
                }

                if !fps_gate.allow() {
                    stream.queue_raw_buffer(raw);
                    return;
                }
                let enc = encoder.get_or_insert_with(|| {
                    log::info!("[ENCODER] Init with width {:?}, height {:?}", pw_width.get(), pw_height.get(),);
                    AnyEncoder::detect_and_create(
                        pw_width.get(), pw_height.get(), sink.clone(), watchers.idr_rx.clone(), watchers.bitrate_rx.clone()
                    )
                });

                match data.type_ {
                    // ── Zero-copy DMA-BUF путь ────────────────────────────────
                    //
                    // 1. dup(fd)           — наша независимая ссылка на DMA-BUF.
                    // 2. queue_raw_buffer  — возвращаем PW-буфер немедленно.
                    // 3. encode_dmabuf     — GPU-импорт без участия CPU.
                    spa_sys::SPA_DATA_DmaBuf => {
                        let duped = libc::dup(data.fd as c_int);
                        let modifier = current_modifier.load(Ordering::Relaxed);
                        // Возвращаем PipeWire-буфер до отправки fd энкодеру.
                        stream.queue_raw_buffer(raw);

                        if duped < 0 {
                            log::warn!("[PipeWire] dup() failed for DmaBuf fd");
                        } else {
                            enc.encode_dmabuf(duped, stride, offset, modifier, capture_us);
                        }
                    }

                    // ── CPU fallback: MemPtr / MemFd ──────────────────────────
                    spa_sys::SPA_DATA_MemPtr | spa_sys::SPA_DATA_MemFd => {
                        encode::process_frame_from_pw_buffer(raw, |src| {
                            enc.encode_bgra(src, stride, capture_us);
                        });
                        stream.queue_raw_buffer(raw);
                    }

                    other => {
                        log::debug!("[PipeWire] Неизвестный тип SPA_DATA: {other}, пропускаем");
                        stream.queue_raw_buffer(raw);
                        return; // не считаем в fps
                    }
                }

                fps_counter += 1;
                if fps_tick.elapsed() >= Duration::from_secs(1) {
                    log::debug!("[PipeWire] FPS: {fps_counter}");
                    fps_counter = 0;
                    fps_tick    = std::time::Instant::now();
                }
            }
        })
        .register()
        .map_err(|e| SenderError::CaptureInit(format!("pw listener: {e}")))?;

    // SPA params: on NVIDIA request CPU buffers only, because current NVENC path
    // drops DMA-BUF frames.
    let allow_dmabuf = !matches!(common::detect_gpu_vendor(), common::GpuVendor::Nvidia);
    if allow_dmabuf {
        log::info!("[PipeWire] DMA-BUF enabled for this GPU vendor");
    } else {
        log::info!("[PipeWire] NVIDIA detected: requesting CPU buffers (MemPtr/MemFd), DMA-BUF disabled");
    }
    let spa_format = build_spa_video_params(allow_dmabuf);
    let spa_buffers = build_spa_buffer_params(allow_dmabuf);
    
    let mut params = [
        pw::spa::pod::Pod::from_bytes(&spa_format)
            .ok_or_else(|| SenderError::CaptureInit("invalid SPA format".into()))?,
        pw::spa::pod::Pod::from_bytes(&spa_buffers)
            .ok_or_else(|| SenderError::CaptureInit("invalid SPA buffers".into()))?,
    ];

    stream.connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| SenderError::CaptureInit(format!("pw stream connect: {e}")))?;

    log::info!("[PipeWire] Capture stream started (node_id={node_id}).");
    mainloop.run();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SPA video format pod
// ─────────────────────────────────────────────────────────────────────────────

/// Строит SPA EnumFormat pod: Video/raw/BGRA.
///
/// PipeWire будет предпочитать DmaBuf если compositor его поддерживает
/// (Wayland compositor с linux-dmabuf-unstable-v1), и автоматически
/// падать back на MemFd/MemPtr если нет.
fn build_spa_video_params(allow_dmabuf: bool) -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::sys::*;

    let mut properties = vec![
        pw::spa::pod::Property {
            key:   SPA_FORMAT_mediaType,
            flags: PropertyFlags::empty(),
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_TYPE_video)),
        },
        pw::spa::pod::Property {
            key:   SPA_FORMAT_mediaSubtype,
            flags: PropertyFlags::empty(),
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_SUBTYPE_raw)),
        },
        pw::spa::pod::Property {
            key:   SPA_FORMAT_VIDEO_format,
            flags: PropertyFlags::empty(),
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_VIDEO_FORMAT_BGRA)),
        },
    ];

    if allow_dmabuf {
        properties.push(pw::spa::pod::Property {
            key:   SPA_FORMAT_VIDEO_modifier,
            flags: PropertyFlags::empty(),
            value: pw::spa::pod::Value::Long(0), // DRM_FORMAT_MOD_LINEAR
        });
    }

    let value = pw::spa::pod::Value::Object(pw::spa::pod::Object {
        type_: SPA_TYPE_OBJECT_Format,
        id:    SPA_PARAM_EnumFormat,
        properties,
    });

    let mut bytes = Vec::new();
    PodSerializer::serialize(&mut std::io::Cursor::new(&mut bytes), &value).unwrap();
    bytes
}

// НОВАЯ ФУНКЦИЯ: Жестко требуем от PipeWire присылать именно DMA-BUF
fn build_spa_buffer_params(allow_dmabuf: bool) -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::sys::*;

    let data_type_mask = if allow_dmabuf {
        (1 << (SPA_DATA_DmaBuf as i32)) |
        (1 << (SPA_DATA_MemPtr as i32)) |
        (1 << (SPA_DATA_MemFd as i32))
    } else {
        (1 << (SPA_DATA_MemPtr as i32)) |
        (1 << (SPA_DATA_MemFd as i32))
    };

    let value = pw::spa::pod::Value::Object(pw::spa::pod::Object {
        type_: SPA_TYPE_OBJECT_ParamBuffers,
        id:    SPA_PARAM_Buffers,
        properties: vec![
            pw::spa::pod::Property {
                key:   SPA_PARAM_BUFFERS_dataType,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(data_type_mask),
            },
        ],
    });

    let mut bytes = Vec::new();
    PodSerializer::serialize(&mut std::io::Cursor::new(&mut bytes), &value).unwrap();
    bytes
}