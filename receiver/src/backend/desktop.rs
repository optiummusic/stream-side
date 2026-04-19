// src/backend/desktop.rs
//
// Десктопный бекенд: HEVC-декодер через ffmpeg-next + VAAPI hardware decode.
//
// ## Потоки данных
//
// ### CPU-путь (fallback: VAAPI недоступна, либо av_hwframe_map не сработал)
// ```text
//   VideoPacket.payload (HEVC NAL)
//     → avcodec (VAAPI hw decode)
//     → AVFrame(VAAPI surface)
//     → av_hwframe_transfer_data → AVFrame(NV12, CPU)   [reused each frame]
//     → copy into pooled Vec<u8>                        [no malloc at steady state]
//     → YuvFrame → WgpuState → queue.write_texture → экран
// ```
//
// ### Zero-copy DMA-BUF путь (приоритетный, Linux + VAAPI + Vulkan)
// ```text
//   VideoPacket.payload (HEVC NAL)
//     → avcodec (VAAPI hw decode)
//     → AVFrame(VAAPI surface)          — лежит в GPU-памяти
//     → av_hwframe_map(DRM_PRIME)       — ноль копий CPU
//     → AVDRMFrameDescriptor.objects[0].fd  ← DMA-BUF дескриптор
//     → dup(fd)                         — берём своё владение
//     → av_frame_free(drm_frame)        — VASurface возвращается в пул декодера
//     → DmaBufFrame { fd, modifier, planes… }
//     → Vulkan: vkImportMemoryFdKHR → VkImage → wgpu::Texture → экран
// ```
//
// ## Ключевое условие работоспособности zero-copy (зеркало encode.rs)
//
// VAAPI-устройство должно быть **производным** от DRM-устройства
// (`av_hwdevice_ctx_create_derived`), иначе DMA-BUF fd, выданный FFmpeg,
// не может быть импортирован Vulkan-ом: они ссылались бы на разные
// GEM-пространства. Именно поэтому `init_vaapi_from_drm` повторяет
// логику `init_hw_contexts` из encode.rs.
//
// ## Добавить в backend/mod.rs
// ```rust
// pub use desktop::DmaBufFrame;
//
// pub enum FrameOutput {
//     Yuv(YuvFrame),
//     DmaBuf(DmaBufFrame),   // ← new variant
//     DirectToSurface,
//     Pending,
// }
// ```

use std::{collections::HashMap, time::{Duration, Instant}};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::io::FromRawFd;
use std::ptr;

use bytes::BytesMut;
use common::{FrameTrace, GpuVendor, detect_gpu_vendor};

use crate::backend::PushStatus;
#[cfg(unix)]
use crate::types::DmaBufFrame;

use ffmpeg_next::{
    codec,
    format::Pixel,
    software::scaling,
    util::frame::video::Video,
    ffi::*,
};

use super::{BackendError, FrameOutput, VideoBackend, YuvFrame};


// ─────────────────────────────────────────────────────────────────────────────
// Struct
// ─────────────────────────────────────────────────────────────────────────────

pub struct DesktopFfmpegBackend {
    decoder:        ffmpeg_next::decoder::Video,
    scaler:         Option<scaling::Context>,
    last_fmt:       Pixel,
    pending_traces: HashMap<u64, (u64, Option<FrameTrace>)>,
    current_frame_trace: Option<FrameTrace>,
    slice_buffer:   BytesMut,
    // ── CPU-путь (fallback) ──────────────────────────────────────────────────

    /// Переиспользуемый CPU AVFrame для av_hwframe_transfer_data.
    transfer_frame: *mut AVFrame,
    scaler_out:     Option<Video>,
    y_pool:         Vec<u8>,
    uv_pool:        Vec<u8>,

    fps_counter: u32,
    last_fps_check: Instant,

    // ── Zero-copy DMA-BUF путь ───────────────────────────────────────────────

    /// DRM-устройство (не null → VAAPI производен от DRM → DMA-BUF доступен).
    /// Хранится только для удержания ссылки; не используется напрямую после init.
    _drm_dev: *mut AVBufferRef,

    /// Переиспользуемый AVFrame для av_hwframe_map.
    /// av_frame_unref() сбрасывает DRM-дескриптор между кадрами без аллокации.
    map_frame: *mut AVFrame,

    /// true — VAAPI создана производной от DRM-устройства, DMA-BUF работает.
    dmabuf_enabled: bool,
}

// SAFETY: все *mut AVBufferRef / *mut AVFrame управляются исключительно этим потоком.
unsafe impl Send for DesktopFfmpegBackend {}

// ─────────────────────────────────────────────────────────────────────────────
// impl
// ─────────────────────────────────────────────────────────────────────────────

impl DesktopFfmpegBackend {
    /// Инициализировать ffmpeg и открыть HEVC-декодер.
    ///
    /// Пытается поднять zero-copy путь (DRM-производный VAAPI + DMA-BUF).
    /// При неудаче включает прямой VAAPI с CPU-копированием (fallback).
    pub fn new() -> Result<Self, BackendError> {
        ffmpeg_next::init()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;
        ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

        let codec = codec::decoder::find(codec::Id::HEVC)
            .ok_or_else(|| BackendError::ConfigError(
                "HEVC codec not found. Install ffmpeg with HEVC support.".into(),
            ))?;

        let mut ctx = codec::context::Context::new();

        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
            (*raw).flags2 |= AV_CODEC_FLAG2_CHUNKS as i32;
            (*raw).thread_count = 1;
        }

        // ── Попытка 1: DRM → производный VAAPI (zero-copy DMA-BUF) ──────────
        //
        // Зеркало encode.rs: VAAPI должен разделять DRM fd с Vulkan,
        // чтобы DMA-BUF экспортированный FFmpeg мог быть импортирован
        // Vulkan без EINVAL.

        let (vaapi_dev, drm_dev, raw_dmabuf) = unsafe { init_vaapi_from_drm(ctx.as_mut_ptr()) };

        // env variables remained for versatility

        // NVIDIA + Vulkan DMA-BUF import can produce chroma artifacts on some stacks.
        // Default to CPU fallback there; allow override with RECEIVER_FORCE_DMABUF=1.
        let gpu = detect_gpu_vendor();
        let dmabuf_enabled = Self::evaluate_dmabuf_support(raw_dmabuf, gpu);

        unsafe {
            if !vaapi_dev.is_null() {
                let raw = ctx.as_mut_ptr();
                // hw_device_ctx берёт ref; исходный vaapi_dev мы отдадим в _drm_dev.
                (*raw).hw_device_ctx = av_buffer_ref(vaapi_dev);
                (*raw).get_format    = Some(get_hw_format);
                // vaapi_dev нам больше не нужен — декодер держит ref через codec ctx.
                // Но мы должны вернуть его в поле, чтобы освободить при Drop.
                // Поэтому НЕ делаем av_buffer_unref здесь; передаём владение в поле.
            }
        }

        let decoder = ctx
            .decoder()
            .open_as(codec)
            .map_err(|e| BackendError::ConfigError(e.to_string()))?
            .video()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;

        // Единственный CPU AVFrame за всё время жизни бекенда (fallback path).
        let transfer_frame = unsafe { av_frame_alloc() };
        if transfer_frame.is_null() {
            return Err(BackendError::ConfigError("av_frame_alloc failed".into()));
        }

        // AVFrame для av_hwframe_map (zero-copy path).
        let map_frame = unsafe { av_frame_alloc() };
        if map_frame.is_null() {
            return Err(BackendError::ConfigError("av_frame_alloc (map) failed".into()));
        }

        if dmabuf_enabled {
            log::info!("[Decoder] Zero-copy DMA-BUF path: ACTIVE");
        } else {
            log::info!("[Decoder] Zero-copy DMA-BUF path: INACTIVE (CPU copy fallback)");
        }

        Ok(Self {
            decoder,
            scaler:         None,
            last_fmt:       Pixel::None,
            pending_traces: HashMap::new(),
            slice_buffer:   BytesMut::with_capacity(1024 * 512),
            current_frame_trace: None,
            transfer_frame,
            scaler_out:     None,
            y_pool:         Vec::new(),
            uv_pool:        Vec::new(),
            _drm_dev:       drm_dev,
            map_frame,
            dmabuf_enabled,
            fps_counter: 0,
            last_fps_check: Instant::now(),
        })
    }
    fn evaluate_dmabuf_support(initial: bool, gpu: GpuVendor) -> bool {
        let force = std::env::var("RECEIVER_FORCE_DMABUF").as_deref() == Ok("1");
        let disable = std::env::var("RECEIVER_DISABLE_DMABUF").as_deref() == Ok("1");

        if force { return true; }
        if disable { return false; }
        if gpu == GpuVendor::Nvidia { 
            log::info!("Detected NVIDIA GPU, disabling DMA-BUF (for now)");
            return false; 
        }
        
        initial
    }
}

impl Drop for DesktopFfmpegBackend {
    fn drop(&mut self) {
        unsafe {
            if !self.transfer_frame.is_null() {
                av_frame_free(&mut self.transfer_frame);
            }
            if !self.map_frame.is_null() {
                av_frame_free(&mut self.map_frame);
            }
            if !self._drm_dev.is_null() {
                av_buffer_unref(&mut self._drm_dev);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VideoBackend
// ─────────────────────────────────────────────────────────────────────────────

impl VideoBackend for DesktopFfmpegBackend {
    fn get_current_trace(&mut self) -> &mut Option<FrameTrace> {
        &mut self.current_frame_trace
    }
    fn get_slice_buffer(&mut self) -> &mut BytesMut {
        &mut self.slice_buffer
    }
    fn submit_to_decoder(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>) -> Result<PushStatus, BackendError> {
        self.fps_counter += 1;

        // 2. Проверка времени (раз в секунду)
        let elapsed = self.last_fps_check.elapsed();
        if elapsed >= Duration::from_secs(1) {
            // Вычисляем FPS (с учетом возможной задержки потока)
            let fps: f64 = self.fps_counter as f64 / elapsed.as_secs_f64();
            
            // Логируем или выводим куда-нибудь
            log::info!("Current Input FPS: {:.2}", fps);

            // Сбрасываем состояние
            self.fps_counter = 0;
            self.last_fps_check = Instant::now();
        }

        let mut pkt = ffmpeg_next::Packet::new(payload.len());
        if let Some(dst) = pkt.data_mut() {
            dst.copy_from_slice(payload.as_ref());
        }
        let pts_key = FrameTrace::now_us();
        pkt.set_pts(Some(pts_key as i64));
        pkt.set_dts(Some(pts_key as i64));

        self.decoder.send_packet(&pkt)
            .map_err(|e| BackendError::DecodeError(e.to_string()))?;

        self.pending_traces.insert(pts_key, (frame_id, trace));

        // Evict entries older than ~2 seconds to bound memory use.
        // (2 000 000 µs ≈ 2 s; covers any realistic decode latency.)
        const HORIZON_US: u64 = 5_000_000;
        self.pending_traces.retain(|&k, _| k >= pts_key.saturating_sub(HORIZON_US));
        Ok(PushStatus::Accepted)
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        // ── 1. Получаем декодированный кадр ──────────────────────────────────

        let mut raw = Video::empty();
        if self.decoder.receive_frame(&mut raw).is_err() {
            return Ok(FrameOutput::Pending);
        }

        // ── 2. Метаданные ─────────────────────────────────────────────────────

        let (pts_key, fmt, w, h) = unsafe {
            let f: &AVFrame = &*raw.as_ptr();
            // f.pts is the wall-clock key we stored in push_encoded.
            // If VAAPI still loses it we fall back to 0 and miss the trace,
            // which is harmless (trace becomes Default) — but capture_us will
            // be 0 only in that degenerate case, not always.
            let key = if f.pts != AV_NOPTS_VALUE { f.pts as u64 } else { 0 };
            let fmt_sys: AVPixelFormat = std::mem::transmute(f.format);
            (key, Pixel::from(fmt_sys), f.width as u32, f.height as u32)
        };

        let (frame_id, raw_trace) = self.pending_traces
            .remove(&pts_key)
            .unwrap_or((0, None));
 
        let mut trace = raw_trace.unwrap_or_default();
        trace.decode_us = FrameTrace::now_us();

        // ── 3. Zero-copy путь: VAAPI → av_hwframe_map → DRM_PRIME ────────────
        //
        // Если VAAPI и DRM-устройство были подняты совместно (dmabuf_enabled),
        // пытаемся экспортировать VASurface как DMA-BUF без участия CPU.
        //
        // Зеркало encode.rs encode_dmabuf_to_vaapi(), но в обратную сторону:
        //   там:  DRM_PRIME(fd) → av_hwframe_map → VAAPI  (import)
        //   здесь: VAAPI → av_hwframe_map → DRM_PRIME(fd)  (export)

        #[cfg(unix)]
        if fmt == Pixel::VAAPI && self.dmabuf_enabled {
            match unsafe { try_map_vaapi_to_drm(self.map_frame, raw.as_ptr(), frame_id, trace, w, h) } {
                Ok(frame) => return Ok(FrameOutput::DmaBuf(frame)),
                Err(e) => {
                    log::warn!("[Decoder] av_hwframe_map failed ({e}); falling back to CPU copy");
                    // Сбрасываем map_frame на случай частичного заполнения.
                    unsafe { av_frame_unref(self.map_frame) };
                }
            }
        }

        // ── 4. CPU-путь (fallback): VAAPI → NV12 CPU ─────────────────────────

        let frame_ptr: *const AVFrame = if fmt == Pixel::VAAPI {
            unsafe {
                av_frame_unref(self.transfer_frame);
                (*self.transfer_frame).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;

                let ret = av_hwframe_transfer_data(
                    self.transfer_frame,
                    raw.as_ptr(),
                    0,
                );
                if ret < 0 {
                    return Err(BackendError::DecodeError(
                        format!("av_hwframe_transfer_data failed: {ret}")
                    ));
                }
                av_frame_copy_props(self.transfer_frame, raw.as_ptr());
                self.transfer_frame as *const AVFrame
            }
        } else {
            unsafe { raw.as_ptr() }
        };

        // ── 5. SW fallback: конвертируем в NV12 если нужно ───────────────────

        let nv12_ptr: *const AVFrame = if fmt == Pixel::NV12 {
            frame_ptr
        } else {
            if fmt != self.last_fmt {
                self.last_fmt = fmt;
                self.scaler = Some(
                    scaling::Context::get(
                        fmt, w, h,
                        Pixel::NV12, w, h,
                        scaling::Flags::BILINEAR,
                    )
                    .map_err(|e| BackendError::DecodeError(e.to_string()))?,
                );
                self.scaler_out = Some(Video::new(Pixel::NV12, w, h));
                log::debug!("[Decoder] Scaler created: {:?} → NV12 {}×{}", fmt, w, h);
            }

            let sc  = self.scaler.as_mut().unwrap();
            let out = self.scaler_out.as_mut().unwrap();

            let src_video = unsafe { Video::wrap(frame_ptr as *mut AVFrame) };
            sc.run(&src_video, out)
                .map_err(|e| BackendError::DecodeError(e.to_string()))?;
            std::mem::forget(src_video);
            unsafe { out.as_ptr() }
        };

        // ── 6. Копируем плоскости в пул буферов ──────────────────────────────

        let (y_stride, uv_stride, y_len, uv_len) = unsafe {
            let f = &*nv12_ptr;
            let ys  = f.linesize[0] as usize;
            let uvs = f.linesize[1] as usize;
            (ys, uvs, ys * h as usize, uvs * h as usize / 2)
        };

        self.y_pool.resize(y_len, 0);
        self.uv_pool.resize(uv_len, 0);

        unsafe {
            let f = &*nv12_ptr;
            self.y_pool[..y_len]
                .copy_from_slice(std::slice::from_raw_parts(f.data[0], y_len));
            self.uv_pool[..uv_len]
                .copy_from_slice(std::slice::from_raw_parts(f.data[1], uv_len));
        }

        let y  = std::mem::replace(&mut self.y_pool,  Vec::new());
        let uv = std::mem::replace(&mut self.uv_pool, Vec::new());

        Ok(FrameOutput::Yuv(YuvFrame {
            frame_id,
            trace,
            width:     w,
            height:    h,
            y,
            uv,
            y_stride:  y_stride  as u32,
            uv_stride: uv_stride as u32,
        }))
    }

    fn shutdown(&mut self) {
        log::info!("[Decoder] DesktopFfmpegBackend: shutdown");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Zero-copy: av_hwframe_map VAAPI → DRM_PRIME
// ─────────────────────────────────────────────────────────────────────────────

/// Экспортирует декодированный VAAPI-кадр как DMA-BUF без участия CPU.
///
/// # Что происходит
/// 1. `av_hwframe_map(AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT)` просит
///    VA-API экспортировать `VASurface` как DRM Prime FD.
/// 2. Из `AVDRMFrameDescriptor` извлекаем fd, modifier и offsets плоскостей.
/// 3. `libc::dup(fd)` — берём **своё** владение на fd. После этого можем
///    безопасно освободить `map_frame` (который разблокирует VASurface).
/// 4. Возвращаем `DmaBufFrame` с dup-нутым `OwnedFd`.
///
/// # Safety
/// `map_frame` должен быть валидным предвыделенным AVFrame (переиспользуется).
/// `src` — валидный AVFrame формата AV_PIX_FMT_VAAPI.
#[cfg(unix)]
unsafe fn try_map_vaapi_to_drm(
    map_frame: *mut AVFrame,
    src:       *const AVFrame,
    frame_id:  u64,
    trace:     FrameTrace,
    width:     u32,
    height:    u32,
) -> Result<DmaBufFrame, &'static str> {
    // Сбрасываем результат предыдущего маппинга.
    av_frame_unref(map_frame);

    // Указываем желаемый формат — FFmpeg найдёт маппер VAAPI → DRM_PRIME.
    (*map_frame).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
    (*map_frame).width  = (*src).width;
    (*map_frame).height = (*src).height;

    // AV_HWFRAME_MAP_READ    = 1  — нам нужен read-доступ (для рендеринга)
    // AV_HWFRAME_MAP_DIRECT  = 8  — прямой маппинг без промежуточных копий
    let flags = 1 | 8;
    let ret = av_hwframe_map(map_frame, src as *mut _, flags);
    if ret < 0 {
        return Err("av_hwframe_map returned error");
    }

    // data[0] → *AVDRMFrameDescriptor (alloced & owned by FFmpeg, живёт пока map_frame жив)
    let desc_ptr = (*map_frame).data[0] as *const AVDRMFrameDescriptor;
    if desc_ptr.is_null() {
        av_frame_unref(map_frame);
        return Err("AVDRMFrameDescriptor is null after mapping");
    }
    let desc = &*desc_ptr;

    if desc.nb_objects < 1 || desc.nb_layers < 1 {
        av_frame_unref(map_frame);
        return Err("unexpected DRM descriptor: nb_objects or nb_layers < 1");
    }

    let obj      = &desc.objects[0];
    let layer    = &desc.layers[0];

    // Для NV12 обычно 1 layer, 2 planes (или иногда 2 layers × 1 plane каждый).
    // Поддерживаем оба варианта.
    let (y_offset, y_pitch, uv_offset, uv_pitch) = if layer.nb_planes >= 2 {
        // 1 layer, 2 planes (Intel common case)
        (
            layer.planes[0].offset as u32,
            layer.planes[0].pitch  as u32,
            layer.planes[1].offset as u32,
            layer.planes[1].pitch  as u32,
        )
    } else if desc.nb_layers >= 2 {
        // 2 layers × 1 plane each (некоторые AMD/Mesa конфигурации)
        (
            desc.layers[0].planes[0].offset as u32,
            desc.layers[0].planes[0].pitch  as u32,
            desc.layers[1].planes[0].offset as u32,
            desc.layers[1].planes[0].pitch  as u32,
        )
    } else {
        av_frame_unref(map_frame);
        return Err("NV12 DRM descriptor: expected 2 planes or 2 layers");
    };

    let fd_orig    = obj.fd;
    let modifier   = obj.format_modifier;
    let total_size = obj.size;

    if fd_orig < 0 {
        av_frame_unref(map_frame);
        return Err("DRM object fd is invalid");
    }

    // dup fd — получаем собственный дескриптор, независимый от AVFrame.
    let dup_fd = libc::dup(fd_orig);
    if dup_fd < 0 {
        av_frame_unref(map_frame);
        return Err("libc::dup failed for DRM fd");
    }

    // Освобождаем map_frame: DRM-маппинг снимается, VASurface идёт обратно в
    // пул декодера. Наш dup_fd продолжает удерживать GEM-объект в памяти GPU.
    av_frame_unref(map_frame);

    log::trace!(
        "[DMA-BUF] frame #{frame_id} exported: fd={dup_fd} modifier={modifier:#018x} \
         y=[off={y_offset} pitch={y_pitch}] uv=[off={uv_offset} pitch={uv_pitch}] \
         total={total_size}B"
    );

    Ok(DmaBufFrame {
        frame_id,
        trace,
        width,
        height,
        fd: OwnedFd::from_raw_fd(dup_fd),
        modifier,
        total_size,
        y_offset,
        y_pitch,
        uv_offset,
        uv_pitch,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Инициализация: DRM device → производный VAAPI device
// ─────────────────────────────────────────────────────────────────────────────
//
// Зеркало init_hw_contexts из encode.rs.
// Возвращает (vaapi_dev, drm_dev, dmabuf_enabled).
//
// vaapi_dev прописывается в codec_ctx и дополнительно возвращается
// для хранения в поле `_drm_dev` (чтобы ссылка не обнулилась раньше времени).
// drm_dev — удерживается в `_drm_dev` struct-поле; обнуляется при Drop.

unsafe fn init_vaapi_from_drm(
    codec_ctx: *mut AVCodecContext,
) -> (*mut AVBufferRef, *mut AVBufferRef, bool) {
    // ── Шаг 1: открываем DRM-устройство ─────────────────────────────────────

    let drm_candidates = [
        "/dev/dri/renderD128",
        "/dev/dri/renderD129",
        "/dev/dri/card0",
        "/dev/dri/card1",
    ];

    let mut drm_dev: *mut AVBufferRef = ptr::null_mut();
    for node in &drm_candidates {
        let c = std::ffi::CString::new(*node).unwrap();
        let ret = av_hwdevice_ctx_create(
            &mut drm_dev,
            AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            c.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if ret >= 0 {
            log::info!("[Decoder] DRM device opened: {node}");
            break;
        }
    }

    if drm_dev.is_null() {
        log::warn!(
            "[Decoder] DRM device unavailable — DMA-BUF zero-copy disabled. \
             Falling back to direct VAAPI."
        );
        // ── Fallback: прямой VAAPI без DRM ───────────────────────────────────
        let mut vaapi_dev: *mut AVBufferRef = ptr::null_mut();
        let node = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        let ret = av_hwdevice_ctx_create(
            &mut vaapi_dev,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if ret >= 0 {
            log::info!("[Decoder] Direct VAAPI active (CPU copy path)");
            (*codec_ctx).hw_device_ctx = av_buffer_ref(vaapi_dev);
            (*codec_ctx).get_format    = Some(get_hw_format);
            return (vaapi_dev, ptr::null_mut(), false);
        }
        log::warn!("[Decoder] VAAPI init failed — software decode only");
        return (ptr::null_mut(), ptr::null_mut(), false);
    }

    // ── Шаг 2: производим VAAPI от DRM (shared fd — ключевое условие) ───────
    //
    // Без av_hwdevice_ctx_create_derived оба контекста использовали бы
    // разные /dev/dri/renderDxxx fd, и DMA-BUF экспортированный FFmpeg
    // не был бы импортируем Vulkan-ом (ядро не нашло бы GEM-объект в
    // другом DRM-файловом-пространстве).

    let mut vaapi_dev: *mut AVBufferRef = ptr::null_mut();
    let ret = av_hwdevice_ctx_create_derived(
        &mut vaapi_dev,
        AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        drm_dev,
        0,
    );

    if ret < 0 {
        log::warn!(
            "[Decoder] av_hwdevice_ctx_create_derived(VAAPI←DRM) failed: {ret}. \
             DMA-BUF zero-copy disabled."
        );
        av_buffer_unref(&mut drm_dev);

        // Fallback: создаём VAAPI независимо.
        let mut vaapi_fallback: *mut AVBufferRef = ptr::null_mut();
        let node = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        av_hwdevice_ctx_create(
            &mut vaapi_fallback,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if !vaapi_fallback.is_null() {
            (*codec_ctx).hw_device_ctx = av_buffer_ref(vaapi_fallback);
            (*codec_ctx).get_format    = Some(get_hw_format);
        }
        return (vaapi_fallback, ptr::null_mut(), false);
    }

    log::info!("[Decoder] VAAPI derived from DRM — DMA-BUF zero-copy ENABLED");
    // vaapi_dev и drm_dev оба нужны: codec_ctx держит ref на vaapi,
    // _drm_dev поле держит drm открытым (пока есть производные устройства —
    // не принципиально, но явно красивее).
    (vaapi_dev, drm_dev, true)
}

// ─────────────────────────────────────────────────────────────────────────────
// get_hw_format callback
// ─────────────────────────────────────────────────────────────────────────────

unsafe extern "C" fn get_hw_format(
    _ctx:     *mut AVCodecContext,
    pix_fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    unsafe {
        let mut p = pix_fmts;
        while *p != AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *p;
            }
            p = p.add(1);
        }
        AVPixelFormat::AV_PIX_FMT_NONE
    }
}