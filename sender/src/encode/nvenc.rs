// encode/nvenc.rs
//
// hevc_nvenc через ffmpeg-next.
// CPU-путь:  BGRA → SwsScale → NV12 (CPU) → CUDA upload → nvenc
// DMA-BUF:   fd → av_hwframe_map(CUDA←DRM_PRIME) → nvenc  (TODO: требует nvdec/cuda iop)

use std::os::unix::io::RawFd;
use std::{ffi::CString, path::Path, ptr, slice, sync::Arc};

use bytes::Bytes;
use ffmpeg_next::{self as ffmpeg, Frame};
use ffmpeg::{codec, format::Pixel, software::scaling, util::frame::video::Video};
use ffmpeg_next::ffi::*;
use libc::{c_int, mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ};
use pipewire::spa::sys as spa_sys;
use pipewire::sys as pw_sys;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use tokio::sync::mpsc as async_mpsc;

use common::{FrameTrace, NaluType};

use crate::encode::EncodedFrame;

// ─────────────────────────────────────────────────────────────────────────────
// Константы DRM fourcc / modifier
// ─────────────────────────────────────────────────────────────────────────────

/// DRM_FORMAT_ARGB8888 = "AR24" — в памяти: B G R A (т.е. BGRA).
const DRM_FORMAT_ARGB8888: u32  = 0x3432_5241;
/// DRM_FORMAT_MOD_LINEAR — линейный (не тайловый) layout.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// DRM_FORMAT_MOD_INVALID — драйвер сам определяет модификатор по GEM-хэндлу.
/// Используется как fallback, когда реальный модификатор неизвестен.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const BITRATE: i64 = 5_000_000;
// ─────────────────────────────────────────────────────────────────────────────
// Тип кадра, передаваемого в канал энкодера
// ─────────────────────────────────────────────────────────────────────────────

/// Кадр, поступающий от PipeWire в поток энкодера.
pub enum FrameData {
    /// CPU-доступный буфер BGRA (MemPtr / MemFd).
    ///
    /// Буфер взят из пула двойной буферизации и должен быть возвращён
    /// через `free_tx` после использования.
    Bgra {
        data: Vec<u8>,
        stride: u32,
    },

    /// DMA-BUF кадр — нулевое копирование на CPU.
    ///
    /// `fd` — **dup-нутый** дескриптор, владелец должен его закрыть.
    /// PipeWire-буфер уже возвращён до передачи этого значения.
    DmaBuf {
        fd:       RawFd,
        stride:   u32,
        offset:   u32,
        /// DRM-модификатор (тайлинг). Согласовывается PipeWire при открытии стрима.
        modifier: u64,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Публичный API
// ─────────────────────────────────────────────────────────────────────────────

/// VAAPI HEVC энкодер с поддержкой DMA-BUF zero-copy и CPU fallback.
pub struct NvencEncoder {
    tx:       SyncSender<(FrameData, u64)>,
    _worker:  thread::JoinHandle<()>,
    /// Пул CPU-буферов (только для пути Bgra).
    free_rx:  mpsc::Receiver<Vec<u8>>,
    idr_rx: tokio::sync::watch::Receiver<bool>,
}

impl NvencEncoder {
    /// Инициализация VAAPI HEVC энкодера.
    ///
    /// Порождает один OS-поток для цикла кодирования.
    /// Блокируется до завершения инициализации VAAPI/DRM, чтобы первый
    /// вызов `encode_bgra` / `encode_dmabuf` был немедленно готов.
    ///
    /// # Panics
    /// Паникует, если `hevc_vaapi` недоступен или VAAPI/DRM недоступны.
    pub fn new(width: u32, height: u32, sink: async_mpsc::Sender<EncodedFrame>, idr_rx: tokio::sync::watch::Receiver<bool>) -> Self {
        let (tx, rx)             = mpsc::sync_channel::<(FrameData, u64)>(4);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (free_tx, free_rx)   = mpsc::channel::<Vec<u8>>();

        // Два BGRA буфера для CPU-пути.
        let buf_size = (width * height * 4) as usize;
        free_tx.send(vec![0u8; buf_size]).unwrap();
        free_tx.send(vec![0u8; buf_size]).unwrap();

        ffmpeg::init().unwrap();
        let idr_rx_clone = idr_rx.clone();
        let worker = thread::Builder::new()
            .name("vaapi-encoder".into())
            .spawn(move || {
                run_encoder_loop(width, height, rx, free_tx, ready_tx, sink, idr_rx_clone);
            })
            .expect("failed to spawn encoder thread");

        ready_rx.recv().expect("encoder init signal");

        Self { tx, _worker: worker, free_rx, idr_rx }
    }

    /// Отправить BGRA-кадр (CPU-путь) на кодирование.
    ///
    /// Если пул буферов пуст (энкодер не успевает) — кадр молча дропается.
    /// Отправить BGRA-кадр с явным stride (bytes per row) на кодирование.
    pub fn encode_bgra(&self, frame: &[u8], stride: u32, capture_us: u64) {
        if let Ok(mut buf) = self.free_rx.try_recv() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            let _ = self.tx.try_send((FrameData::Bgra { data: buf, stride }, capture_us));
        }
    }

    /// Отправить DMA-BUF кадр (GPU zero-copy путь) на кодирование.
    ///
    /// `fd` должен быть **уже dup-нут** вызывающей стороной.
    /// `modifier` — DRM-модификатор, полученный из PipeWire при согласовании формата.
    /// Энкодер закроет fd после использования.
    pub fn encode_dmabuf(&self, fd: RawFd, stride: u32, offset: u32, modifier: u64, capture_us: u64) {
        let frame = FrameData::DmaBuf { fd, stride, offset, modifier };
        if self.tx.try_send((frame, capture_us)).is_err() {
            // Канал полон или закрыт — освободить fd.
            unsafe { libc::close(fd); }
        }
    }
}

struct SendPtr(*mut AVBufferRef);
unsafe impl Send for SendPtr {}

struct VaapiConvertGraph {
    graph: *mut AVFilterGraph,
    buffersrc_ctx: *mut AVFilterContext,
    buffersink_ctx: *mut AVFilterContext,
    scale_ctx: *mut AVFilterContext,
    hw_device_ctx: *mut AVBufferRef,
    hw_frames_ctx: *mut AVBufferRef,
}

unsafe impl Send for VaapiConvertGraph {}
unsafe impl Sync for VaapiConvertGraph {}

impl VaapiConvertGraph {
    unsafe fn free(&mut self) {
        if !self.graph.is_null() {
            avfilter_graph_free(&mut self.graph);
        }
        self.buffersrc_ctx = std::ptr::null_mut();
        self.buffersink_ctx = std::ptr::null_mut();
        self.scale_ctx = std::ptr::null_mut();
        self.hw_device_ctx = std::ptr::null_mut();
        self.hw_frames_ctx = std::ptr::null_mut();
    }
}

impl Drop for VaapiConvertGraph {
    fn drop(&mut self) {
        unsafe { self.free(); }
    }
}

fn run_encoder_loop(
    width:    u32,
    height:   u32,
    rx:       mpsc::Receiver<(FrameData, u64)>,
    free_tx:  mpsc::Sender<Vec<u8>>,
    ready_tx: mpsc::Sender<()>,
    sink:     async_mpsc::Sender<EncodedFrame>,
    mut idr_rx: tokio::sync::watch::Receiver<bool>,
) {
    // ── Codec init ─────────────────────────────────────────────────

    let codec = codec::encoder::find_by_name("hevc_nvenc")
        .expect("hevc_nvenc encoder not found");

    let mut enc_ctx = codec::context::Context::new_with_codec(codec);

    unsafe {
        let raw = enc_ctx.as_mut_ptr();
        (*raw).width          = width  as i32;
        (*raw).height         = height as i32;
        (*raw).time_base      = AVRational { num: 1, den: 60 };
        (*raw).pix_fmt        = AVPixelFormat::AV_PIX_FMT_NV12;
        (*raw).bit_rate       = BITRATE;
        (*raw).rc_max_rate    = BITRATE;
        (*raw).rc_buffer_size = BITRATE as i32 / 2;
        (*raw).max_b_frames   = 0;
        (*raw).delay          = 0;
        (*raw).flags         &= !(AV_CODEC_FLAG_GLOBAL_HEADER as i32);
        (*raw).slices       = 4;
    }

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("preset", "p3");
    opts.set("rc", "vbr");
    opts.set("profile", "main");
    opts.set("gpu", "0");
    opts.set("async_depth",   "1");
    opts.set("low_delay_brc", "1");

    // VAAPI + DRM: создаём оба контекста через единый DRM-девайс.
    // ВАЖНО: VAAPI должен быть ПРОИЗВОДНЫМ от DRM-девайса, чтобы оба контекста
    // разделяли один и тот же fd DRM-устройства. Только тогда av_hwframe_transfer_data
    // (DRM_PRIME → VAAPI) сможет шарить DMA-BUF без EINVAL (-22).
    let mut encoder = enc_ctx
        .encoder()
        .video()
        .expect("video encoder")
        .open_with(opts)
        .expect("avcodec_open2 failed");

    unsafe {
        (*encoder.as_mut_ptr()).gop_size = 300;
    }

    // ── SwsScale для CPU-пути (BGRA → NV12) ─────────────────────────────────

    let mut scaler = scaling::Context::get(
        Pixel::BGRA, width, height,
        Pixel::NV12, width, height,
        scaling::Flags::BILINEAR,
    )
    .expect("SwsContext init failed");

    // Сигнал о готовности.
    ready_tx.send(()).unwrap();

    // ── Цикл кадров ──────────────────────────────────────────────────────────

    let mut started    = false;
    let mut src_frame  = Video::new(Pixel::BGRA, width, height);
    let mut nv12_frame = Video::new(Pixel::NV12, width, height);
    let mut force_idr = false;

    let mut last_hw_frame: *mut AVFrame = std::ptr::null_mut();
    
    loop {
        let hw_frame_opt: Option<*mut AVFrame> = match rx.recv_timeout(std::time::Duration::from_millis(100)) { // Starvation = give frames at 10 FPS pace
            Ok((mut frame_data, mut capture_us)) => {
                // Дренируем канал: берём самый свежий кадр
                while let Ok(newer) = rx.try_recv() {
                    match frame_data {
                        FrameData::Bgra{data: buf, ..} => { let _ = free_tx.send(buf); }
                        FrameData::DmaBuf { fd, .. } => unsafe { libc::close(fd); },
                    }
                    frame_data  = newer.0;
                    capture_us  = newer.1;
                }

                if idr_rx.has_changed().unwrap_or(false) {
                    if *idr_rx.borrow_and_update() == true {
                        force_idr = true;
                    }
                }

                // Получаем VAAPI hw_frame
                let frame = unsafe {
                    match frame_data {
                        FrameData::Bgra{ data: ref bgra, stride } => {
                            let src_stride = if stride == 0 { (width * 4) as usize } else { stride as usize };
                            let dst_stride = src_frame.stride(0);
                            let row_bytes = (width * 4) as usize;
                            let rows = height as usize;
                            let src_plane = bgra;
                            let dst_plane = src_frame.data_mut(0);

                            if src_stride < row_bytes
                                || dst_stride < row_bytes
                                || src_plane.len() < src_stride.saturating_mul(rows)
                                || dst_plane.len() < dst_stride.saturating_mul(rows)
                            {
                                log::warn!(
                                    "[Encoder] Invalid BGRA stride/buffer: src_stride={}, dst_stride={}, row_bytes={}, src_len={}, dst_len={}",
                                    src_stride,
                                    dst_stride,
                                    row_bytes,
                                    src_plane.len(),
                                    dst_plane.len(),
                                );
                                None
                            } else {
                                for y in 0..rows {
                                    let src_off = y * src_stride;
                                    let dst_off = y * dst_stride;
                                    dst_plane[dst_off..dst_off + row_bytes]
                                        .copy_from_slice(&src_plane[src_off..src_off + row_bytes]);
                                }

                                if scaler.run(&src_frame, &mut nv12_frame).is_err() {
                                    None
                                } else {
                                    nv12_frame.set_pts(Some(capture_us as i64));
                                    clone_nv12_frame_owned(nv12_frame.as_ptr(), capture_us)
                                }
                            }
                        }
                        FrameData::DmaBuf { fd, stride, offset, modifier } => {
                            log::debug!("[Encoder] NVENC path received DMA-BUF; dropping frame (CPU copy path expected)");
                            libc::close(fd);
                            None
                            // TODO - implement DmaBuf receival by NVIDIA
                        }
                    }
                };

                // Возвращаем CPU-буфер в пул
                if let FrameData::Bgra {data: buf, stride: _} = frame_data {
                    let _ = free_tx.send(buf);
                }

                // ДОБАВЛЕНО: Сохраняем ссылку на кадр для генерации дубликатов
                if let Some(f) = frame {
                    unsafe {
                        if !last_hw_frame.is_null() {
                            av_frame_free(&mut last_hw_frame);
                        }
                        // Делаем zero-copy клон (просто инкремент refcount для AVBufferRef)
                        last_hw_frame = av_frame_clone(f);
                    }
                }

                frame
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // ДОБАВЛЕНО: WATCHDOG. Прошло 100мс без кадров (статичный экран Wayland)
                if last_hw_frame.is_null() {
                    continue; // Ещё не было ни одного успешного кадра
                }
                log::info!("[Encoder] No new frames for 100ms, sending P-Skip duplicate");
                // Проверяем запрос IDR даже во время простоя!
                if idr_rx.has_changed().unwrap_or(false) {
                    if *idr_rx.borrow_and_update() == true {
                        force_idr = true;
                    }
                }

                unsafe {
                    let dup = av_frame_clone(last_hw_frame);
                    if dup.is_null() { continue; }
                    
                    // Обновляем таймстемп
                    (*dup).pts = FrameTrace::now_us() as i64;
                    
                    // Если нет запроса на IDR, гарантируем, что это будет обычный P-Skip
                    if !force_idr {
                        (*dup).pict_type = AVPictureType::AV_PICTURE_TYPE_NONE;
                        (*dup).flags &= !(AV_FRAME_FLAG_KEY as i32);
                    }
                    
                    Some(dup)
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Отправляем hw_frame в кодек.
        if let Some(mut hw_frame) = hw_frame_opt {
            unsafe {
                if force_idr {
                    log::warn!("[Encoder] Client requested IDR. Forcing I-frame on current hardware frame.");
                    (*hw_frame).pict_type = AVPictureType::AV_PICTURE_TYPE_I;
                    (*hw_frame).flags |= AV_FRAME_FLAG_KEY as i32;
                    force_idr = false;
                } else {
                    (*hw_frame).pict_type = AVPictureType::AV_PICTURE_TYPE_NONE;
                    (*hw_frame).flags &= !(AV_FRAME_FLAG_KEY as i32);
                }
                if avcodec_send_frame(encoder.as_mut_ptr(), hw_frame) >= 0 {
                    let mut pkt = ffmpeg::Packet::empty();
                    while encoder.receive_packet(&mut pkt).is_ok() {
                        let original_capture_us = pkt.pts().unwrap_or(0) as u64;

                        let mut trace      = FrameTrace::default();
                        trace.capture_us   = original_capture_us;
                        trace.encode_us    = FrameTrace::now_us();

                        let is_key = pkt.is_key();

                        if !started {
                            if !is_key { 
                                log::info!("[ENCODER] The frame is not a KeyFrame, continuing!");
                                continue; 
                            }
                            started = true;
                            log::info!("[Encoder] First IDR frame produced");
                        }

                        if let Some(data) = pkt.data() {
                            // ── NALU slicing ────────────────────────────────────
                            let full_data = Bytes::copy_from_slice(data);
                            
                            // Поиск границ NALU
                            let mut boundaries = Vec::new();
                            let mut pos = 0;
                            while pos + 3 <= data.len() {
                                if data[pos] == 0 && data[pos+1] == 0 && data[pos+2] == 1 {
                                    boundaries.push(pos);
                                    pos += 3;
                                } else if pos + 3 < data.len() && data[pos] == 0 && data[pos+1] == 0 && data[pos+2] == 0 && data[pos+3] == 1 {
                                    boundaries.push(pos);
                                    pos += 4;
                                } else {
                                    pos += 1;
                                }
                            }

                            if boundaries.is_empty() && !data.is_empty() {
                                boundaries.push(0);
                            }

                            let num_nalus = boundaries.len();
                            let ends = boundaries.iter().skip(1).copied().chain(std::iter::once(data.len()));
                            for (idx, (&start, end)) in boundaries.iter().zip(ends).enumerate() {
                                let slice_data = full_data.slice(start..end);
                                
                                // Определяем тип NALU
                                let sc_len = if slice_data.len() > 2 && slice_data[2] == 1 { 3 } else { 4 };
                                let nalu_type = if slice_data.len() > sc_len {
                                    let nal_header = slice_data[sc_len];
                                    let raw_type = (nal_header >> 1) & 0x3F;
                                    match raw_type {
                                        32 => NaluType::VideoParamSet,
                                        33 => NaluType::SeqParamSet,
                                        34 => NaluType::PicParamSet,
                                        38 | 39 => NaluType::Sei,
                                        19 | 20 => NaluType::SliceIdr,
                                        1 => NaluType::SliceTrailing,
                                        other => NaluType::Other(other),
                                    }
                                } else {
                                    NaluType::Other(0)
                                };

                                // Трейс цепляем только к ПЕРВОМУ NALU кадра
                                let trace = if idx == 0 {
                                    let mut t = FrameTrace::default();
                                    t.capture_us = original_capture_us;
                                    t.encode_us = FrameTrace::now_us();
                                    Some(t)
                                } else {
                                    None
                                };

                                let encoded = EncodedFrame {
                                    frame_id: 0, // Назначит сериализатор
                                    data: slice_data,
                                    nalu_type,
                                    is_last: idx == num_nalus - 1,
                                    slice_idx: idx as u8,          // Вот они
                                    total_slices: num_nalus as u8,
                                    is_key,
                                    trace,
                                };

                                // Отправляем немедленно
                                if let Err(e) = sink.try_send(encoded) {
                                    match e {
                                        tokio::sync::mpsc::error::TrySendError::Full(_) => {
                                            log::debug!("[Encoder] sink full, dropping NALU");
                                        }
                                        tokio::sync::mpsc::error::TrySendError::Closed(_) => return,
                                    }
                                }
                            }
                        }
                    }
                }
                av_frame_free(&mut hw_frame);
            }
        }
    }
}

unsafe fn clone_nv12_frame_owned(src: *const AVFrame, pts: u64) -> Option<*mut AVFrame> {
    if src.is_null() {
        return None;
    }

    let mut dst = av_frame_alloc();
    if dst.is_null() {
        return None;
    }

    (*dst).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;
    (*dst).width = (*src).width;
    (*dst).height = (*src).height;

    if av_frame_get_buffer(dst, 32) < 0 {
        av_frame_free(&mut dst);
        return None;
    }

    let w = (*src).width as usize;
    let h = (*src).height as usize;

    // Plane 0: Y (h rows, w bytes each)
    let src_y_stride = (*src).linesize[0] as usize;
    let dst_y_stride = (*dst).linesize[0] as usize;
    for y in 0..h {
        let src_off = y * src_y_stride;
        let dst_off = y * dst_y_stride;
        std::ptr::copy_nonoverlapping(
            (*src).data[0].add(src_off),
            (*dst).data[0].add(dst_off),
            w,
        );
    }

    // Plane 1: interleaved UV (h/2 rows, w bytes each)
    let src_uv_stride = (*src).linesize[1] as usize;
    let dst_uv_stride = (*dst).linesize[1] as usize;
    for y in 0..(h / 2) {
        let src_off = y * src_uv_stride;
        let dst_off = y * dst_uv_stride;
        std::ptr::copy_nonoverlapping(
            (*src).data[1].add(src_off),
            (*dst).data[1].add(dst_off),
            w,
        );
    }

    (*dst).pts = pts as i64;
    Some(dst)
}