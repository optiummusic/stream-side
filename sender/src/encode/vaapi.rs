//! VAAPI HEVC encoder.
//!
//! Получает кадры от PipeWire-потока двумя путями:
//!
//! ## CPU-путь (fallback: MemPtr / MemFd)
//! ```text
//!   &[u8] (BGRA, CPU) → SwsScale → NV12 (CPU) → av_hwframe_transfer_data → VAAPI → encode
//! ```
//!
//! ## Zero-copy DMA-BUF путь (SPA_DATA_DmaBuf)
//! ```text
//!   dup(fd) → AVFrame(DRM_PRIME) → av_hwframe_transfer_data(GPU/VPP) → VAAPI → encode
//! ```
//!
//! В DMA-BUF пути CPU не касается пиксельных данных вообще:
//! конвертация BGRA → NV12 выполняется через VA-VPP на GPU.
//!
//! # Потоки
//!
//! | Поток          | Роль                                                 |
//! |----------------|------------------------------------------------------|
//! | PipeWire (OS)  | dequeue/queue буферов, отправка `FrameData` в канал |
//! | vaapi-encoder  | приём `FrameData`, кодирование, отправка NAL в sink  |

use std::os::unix::io::RawFd;
use std::{ffi::CString, ptr, slice, sync::Arc};

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

use crate::encode::{EncodedFrame};

// ─────────────────────────────────────────────────────────────────────────────
// Константы DRM fourcc / modifier
// ─────────────────────────────────────────────────────────────────────────────

/// DRM_FORMAT_ARGB8888 = "AR24" — в памяти: B G R A (т.е. BGRA).
const DRM_FORMAT_ARGB8888: u32  = 0x3432_5241;
/// DRM_FORMAT_MOD_LINEAR
///  — линейный 
/// (не тайловый) layout.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// DRM_FORMAT_MOD_INVALID — драйвер сам определяет модификатор по GEM-хэндлу.
/// Используется как fallback, когда реальный модификатор неизвестен.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const BITRATE: i64 = 30_000_000;
// ─────────────────────────────────────────────────────────────────────────────
// Тип кадра, передаваемого в канал энкодера
// ─────────────────────────────────────────────────────────────────────────────

/// Кадр, поступающий от PipeWire в поток энкодера.
pub enum FrameData {
    /// CPU-доступный буфер BGRA (MemPtr / MemFd).
    ///
    /// Буфер взят из пула двойной буферизации и должен быть возвращён
    /// через `free_tx` после использования.
    Bgra{
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
pub struct VaapiEncoder {
    tx:       SyncSender<(FrameData, u64)>,
    _worker:  thread::JoinHandle<()>,
    /// Пул CPU-буферов (только для пути Bgra).
    free_rx:  mpsc::Receiver<Vec<u8>>,
    idr_rx: tokio::sync::watch::Receiver<bool>,
}

impl VaapiEncoder {
    /// Инициализация VAAPI HEVC энкодера.
    ///
    /// Порождает один OS-поток для цикла кодирования.
    /// Блокируется до завершения инициализации VAAPI/DRM, чтобы первый
    /// вызов `encode_bgra` / `encode_dmabuf` был немедленно готов.
    ///
    /// # Panics
    /// Паникует, если `hevc_vaapi` недоступен или VAAPI/DRM недоступны.
    pub fn new(
        width: u32, 
        height: u32, 
        sink: async_mpsc::Sender<EncodedFrame>, 
        idr_rx: tokio::sync::watch::Receiver<bool>,
        bitrate_rx: tokio::sync::watch::Receiver<u64>,
    ) -> Self {
        let (tx, rx)             = mpsc::sync_channel::<(FrameData, u64)>(4);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (free_tx, free_rx)   = mpsc::channel::<Vec<u8>>();

        // Два BGRA буфера для CPU-пути.
        let buf_size = (width * height * 4) as usize;
        free_tx.send(vec![0u8; buf_size]).unwrap();
        free_tx.send(vec![0u8; buf_size]).unwrap();

        ffmpeg::init().unwrap();
        let idr_rx_clone = idr_rx.clone();
        let bitrate_rx_clone = bitrate_rx.clone();
        let worker = thread::Builder::new()
            .name("vaapi-encoder".into())
            .spawn(move || {
                run_encoder_loop(width, height, rx, free_tx, ready_tx, sink, idr_rx_clone, bitrate_rx_clone);
            })
            .expect("failed to spawn encoder thread");

        ready_rx.recv().expect("encoder init signal");

        Self { tx, _worker: worker, free_rx, idr_rx }
    }

    /// Отправить BGRA-кадр (CPU-путь) на кодирование.
    ///
    /// Если пул буферов пуст (энкодер не успевает) — кадр молча дропается.
    pub fn encode_bgra(&self, frame: &[u8], stride: u32, capture_us: u64) {
        if let Ok(mut buf) = self.free_rx.try_recv() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            let _ = self.tx.try_send((FrameData::Bgra {data: buf, stride }, capture_us));
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

// ─────────────────────────────────────────────────────────────────────────────
// Вспомогательная обёртка для передачи *mut AVBufferRef через канал
// ─────────────────────────────────────────────────────────────────────────────

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

unsafe fn apply_encoder_params(
    raw: *mut ffmpeg::ffi::AVCodecContext,
    width: u32,
    height: u32,
    bitrate: u64,
) {
    unsafe {
        (*raw).width          = width  as i32;
        (*raw).height         = height as i32;
        (*raw).time_base      = AVRational { num: 1, den: 60 };
        (*raw).pix_fmt        = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*raw).bit_rate       = bitrate as i64;
        (*raw).rc_max_rate    = bitrate as i64;
        (*raw).rc_buffer_size = bitrate as i32 / 2;
        (*raw).max_b_frames   = 0;
        (*raw).delay          = 0;
        (*raw).flags         &= !(AV_CODEC_FLAG_GLOBAL_HEADER as i32);
        (*raw).slices       = 4;
    }
}

fn create_vaapi_encoder(
    codec: &ffmpeg_next::Codec,
    width: u32,
    height: u32,
    bitrate: u64,
    hw_frames_ctx: *mut ffmpeg::ffi::AVBufferRef,
) -> Result<ffmpeg::encoder::Video, ffmpeg::Error> {
    let mut enc_ctx = ffmpeg::codec::context::Context::new_with_codec(codec.clone());
    
    unsafe {
        let raw = enc_ctx.as_mut_ptr();
        apply_encoder_params(raw, width, height, bitrate);
        if !hw_frames_ctx.is_null() {
            (*raw).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(hw_frames_ctx);
        }
    }

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("async_depth", "1");
    opts.set("low_delay_brc", "1");
    opts.set("intra_refresh", "1");
    opts.set("intra_refresh_type", "both");
    opts.set("mbtree", "0");
    opts.set("tune", "zerolatency");
    opts.set("rc_mode", "CBR");

    enc_ctx.encoder().video()?.open_with(opts)
}

fn run_encoder_loop(
    width:    u32,
    height:   u32,
    rx:       mpsc::Receiver<(FrameData, u64)>,
    free_tx:  mpsc::Sender<Vec<u8>>,
    ready_tx: mpsc::Sender<()>,
    sink:     async_mpsc::Sender<EncodedFrame>,
    mut idr_rx: tokio::sync::watch::Receiver<bool>,
    mut bitrate_rx: tokio::sync::watch::Receiver<u64>,
) {
    // ── Инициализация кодека ─────────────────────────────────────────────────

    let codec = codec::encoder::find_by_name("hevc_vaapi")
        .expect("hevc_vaapi encoder not found; ensure VA-API is available");

    let (hw_enc_ref, hw_import_ref, drm_frames_ref, mut vaapi_convert_graph) = unsafe {
        // dummy ctx для получения референсов
        let mut tmp_ctx = codec::context::Context::new_with_codec(codec.clone());
        (*tmp_ctx.as_mut_ptr()).width = width as i32;
        (*tmp_ctx.as_mut_ptr()).height = height as i32;
        init_hw_contexts(tmp_ctx.as_mut_ptr(), width, height)
    };

    let mut current_bitrate = *bitrate_rx.borrow();
    let mut encoder: codec::encoder::Video = create_vaapi_encoder(&codec, width, height, current_bitrate, hw_enc_ref)
        .expect("Initial encoder open failed");

    let mut scaler = scaling::Context::get(
        Pixel::BGRA, width, height,
        Pixel::NV12, width, height,
        scaling::Flags::BILINEAR,
    )
    .expect("SwsContext init failed");

    // Сигнал о готовности.
    ready_tx.send(()).unwrap();

    unsafe {
        let mut current_rc_mode: i64 = 0;
        let rc_mode_str = std::ffi::CString::new("rc_mode").unwrap();
        
        // Пытаемся достать текущий режим из приватных данных
        ffmpeg::ffi::av_opt_get_int((*encoder.as_mut_ptr()).priv_data, rc_mode_str.as_ptr(), 0, &mut current_rc_mode);
        
        log::info!("[VAAPI-Check] Current RC Mode: {}", match current_rc_mode {
            0 => "DEFAULT (Auto)",
            1 => "CQP (Constant QP)",
            2 => "CBR (Constant Bit Rate)",
            3 => "VBR (Variable Bit Rate)",
            4 => "ICQ (Intelligent Constant Quality)",
            _ => "OTHER",
        });
    }

    // ── Цикл кадров ──────────────────────────────────────────────────────────

    let mut started    = false;
    let mut src_frame  = Video::new(Pixel::BGRA, width, height);
    let mut nv12_frame = Video::new(Pixel::NV12, width, height);
    let mut force_idr = false;

    // We add this for P-Skip frames
    let mut last_hw_frame: *mut AVFrame = std::ptr::null_mut();
    loop {
        // Получаем VAAPI hw_frame в зависимости от типа входа.
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

                if bitrate_rx.has_changed().unwrap_or(false) {
                    let new_bitrate = *bitrate_rx.borrow_and_update();
                    if new_bitrate != current_bitrate {
                        log::info!("[Encoder] Bitrate watch changed: {} -> {}", current_bitrate, new_bitrate);
                        match create_vaapi_encoder(&codec, width, height, new_bitrate, hw_enc_ref) {
                            Ok(new_enc) => {
                                encoder = new_enc;
                                current_bitrate = new_bitrate;
                            }
                            Err(e) => log::error!("Failed to switch bitrate: {:?}", e),
                        }
                    }
                }

                if idr_rx.has_changed().unwrap_or(false) {
                    if *idr_rx.borrow_and_update() == true {
                        force_idr = true;
                    }
                }

                // Получаем VAAPI hw_frame
                let frame = unsafe {
                    match frame_data {
                        FrameData::Bgra{ data: ref bgra, stride} => {
                            encode_bgra_to_vaapi(
                                bgra, capture_us,
                                width, height,
                                &mut src_frame, &mut nv12_frame,
                                &mut scaler,
                                hw_enc_ref,
                            )
                        }
                        FrameData::DmaBuf { fd, stride, offset, modifier } => {
                            let imported = encode_dmabuf_to_vaapi(
                                fd, stride, offset, modifier, capture_us,
                                width, height,
                                hw_import_ref, // ПЕРЕДАЕМ СЮДА BGRA КОНТЕКСТ
                                drm_frames_ref,
                            );
                            if let Some(mut imported) = imported {
                                if let Some(ref mut graph) = vaapi_convert_graph {
                                    vaapi_convert_frame_to_nv12(graph, imported, capture_us)
                                } else {
                                    log::warn!("[Encoder] VAAPI conversion graph unavailable; DMA-BUF frame dropped");
                                    av_frame_free(&mut imported);
                                    None
                                }
                            } else {
                                None
                            }
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
                        let is_key = pkt.is_key();

                        if !started {
                            if !is_key { 
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
    unsafe {
        if !last_hw_frame.is_null() {
            av_frame_free(&mut last_hw_frame);
        }
    }
}



unsafe fn init_vaapi_convert_graph(
    vaapi_dev: *mut AVBufferRef,
    hw_frames_ref: *mut AVBufferRef,
    width: u32,
    height: u32,
) -> Option<VaapiConvertGraph> {
    if vaapi_dev.is_null() || hw_frames_ref.is_null() {
        log::warn!("[Encoder] init_vaapi_convert_graph: null vaapi_dev or hw_frames_ref");
        return None;
    }

    let graph = avfilter_graph_alloc();
    if graph.is_null() {
        log::warn!("[Encoder] avfilter_graph_alloc failed");
        return None;
    }


    let name_buffer     = CString::new("buffer").unwrap();
    let name_scale      = CString::new("scale_vaapi").unwrap();
    let name_buffersink = CString::new("buffersink").unwrap();

    let buffer      = avfilter_get_by_name(name_buffer.as_ptr());
    let scale_vaapi = avfilter_get_by_name(name_scale.as_ptr());
    let buffersink  = avfilter_get_by_name(name_buffersink.as_ptr());

    if buffer.is_null() || scale_vaapi.is_null() || buffersink.is_null() {
        log::warn!("[Encoder] VAAPI filter components not found");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    let in_name    = CString::new("in").unwrap();
    let scale_name = CString::new("scale_vaapi_node").unwrap();
    let scale_args = CString::new("format=nv12").unwrap();
    let out_name   = CString::new("out").unwrap();

    // ── Buffer source: alloc → set params (with hw_frames_ctx) → init ────────
    let mut buffersrc_ctx: *mut AVFilterContext =
        avfilter_graph_alloc_filter(graph, buffer, in_name.as_ptr());
    if buffersrc_ctx.is_null() {
        log::warn!("[Encoder] avfilter_graph_alloc_filter(buffer) failed");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    let par = av_buffersrc_parameters_alloc();
    if par.is_null() {
        log::warn!("[Encoder] av_buffersrc_parameters_alloc failed");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }
    (*par).format              = AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
    (*par).width               = width as i32;
    (*par).height              = height as i32;
    (*par).time_base           = AVRational { num: 1, den: 60 };
    (*par).sample_aspect_ratio = AVRational { num: 1, den: 1 };
    (*par).hw_frames_ctx       = av_buffer_ref(hw_frames_ref);

    let ret = av_buffersrc_parameters_set(buffersrc_ctx, par);
    av_free(par as *mut _);
    if ret < 0 {
        log::warn!("[Encoder] av_buffersrc_parameters_set failed: {ret}");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    let ret = avfilter_init_str(buffersrc_ctx, std::ptr::null());
    if ret < 0 {
        log::warn!("[Encoder] avfilter_init_str(buffersrc) failed: {ret}");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    // ── scale_vaapi ───────────────────────────────────────────────────────────
    let mut scale_ctx: *mut AVFilterContext = std::ptr::null_mut();
    let ret = avfilter_graph_create_filter(
        &mut scale_ctx, scale_vaapi,
        scale_name.as_ptr(), scale_args.as_ptr(),
        std::ptr::null_mut(), graph,
    );
    if ret < 0 {
        log::warn!("[Encoder] Cannot create scale_vaapi filter: {ret}");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    // ── buffersink ────────────────────────────────────────────────────────────
    let mut buffersink_ctx: *mut AVFilterContext = std::ptr::null_mut();

    // Не указываем pix_fmts вообще. FFmpeg сам договорится о форматах 
    // во время вызова avfilter_graph_config.
    let ret = avfilter_graph_create_filter(
        &mut buffersink_ctx,
        buffersink,
        out_name.as_ptr(),
        std::ptr::null(), // <--- Убрали sink_args
        std::ptr::null_mut(),
        graph,
    );

    if ret < 0 {
        log::warn!("[Encoder] Cannot create buffersink: {ret}");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    if ret < 0 {
        log::warn!("[Encoder] Cannot create buffersink with args: {ret}");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    // ── Link: buffer → scale_vaapi → buffersink ───────────────────────────────
    if avfilter_link(buffersrc_ctx, 0, scale_ctx, 0) < 0 {
        log::warn!("[Encoder] avfilter_link(buffer->scale_vaapi) failed");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }
    if avfilter_link(scale_ctx, 0, buffersink_ctx, 0) < 0 {
        log::warn!("[Encoder] avfilter_link(scale_vaapi->buffersink) failed");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    let ret = avfilter_graph_config(graph, std::ptr::null_mut());
    if ret < 0 {
        log::warn!("[Encoder] avfilter_graph_config failed: {ret}");
        let mut g = graph; avfilter_graph_free(&mut g);
        return None;
    }

    unsafe {
        for ctx in [buffersrc_ctx, scale_ctx, buffersink_ctx] {
            let raw = ctx as *mut AVFilterContext;
            if (*raw).hw_device_ctx.is_null() {
                (*raw).hw_device_ctx = av_buffer_ref(vaapi_dev);
            }
        }
    }

    unsafe {
        // В AVFilterContext поле hw_device_ctx стабильно и должно быть доступно
        let raw_src = buffersrc_ctx as *mut AVFilterContext;
        if !raw_src.is_null() {
            (*raw_src).hw_device_ctx = av_buffer_ref(vaapi_dev);
        }
    }

    log::info!("[Encoder] VAAPI convert graph configured OK");
    Some(VaapiConvertGraph {
        graph,
        buffersrc_ctx,
        buffersink_ctx,
        scale_ctx,
        hw_device_ctx: std::ptr::null_mut(),
        hw_frames_ctx: std::ptr::null_mut(),
    })
}

unsafe fn vaapi_convert_frame_to_nv12(
    graph: &mut VaapiConvertGraph,
    mut src_hw_frame: *mut AVFrame,
    capture_us: u64,
) -> Option<*mut AVFrame> {
    if src_hw_frame.is_null() {
        return None;
    }

    (*src_hw_frame).pts = capture_us as i64;

    let ret = av_buffersrc_add_frame_flags(
        graph.buffersrc_ctx,
        src_hw_frame,
        AV_BUFFERSRC_FLAG_KEEP_REF as i32,
    );
    if ret < 0 {
        log::warn!("[Encoder] av_buffersrc_add_frame_flags failed: {ret}");
        av_frame_free(&mut src_hw_frame);
        return None;
    }

    // Release our local ref; the filter keeps its own reference because KEEP_REF was set.
    av_frame_free(&mut src_hw_frame);

    let mut out_frame = av_frame_alloc();
    if out_frame.is_null() {
        return None;
    }

    let ret = av_buffersink_get_frame(graph.buffersink_ctx, out_frame);
    if ret < 0 {
        log::warn!("[Encoder] av_buffersink_get_frame failed: {ret}");
        av_frame_free(&mut out_frame);
        return None;
    }

    (*out_frame).pts = capture_us as i64;
    Some(out_frame)
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU-путь: BGRA → NV12 (SwsScale) → VAAPI surface
// ─────────────────────────────────────────────────────────────────────────────

/// Загружает CPU BGRA-кадр на VAAPI surface через SwScale.
///
/// Возвращает `*mut AVFrame` (AV_PIX_FMT_VAAPI) или `None` при ошибке.
/// Вызывающий должен освободить frame через `av_frame_free`.
///
/// # Safety
/// Все указатели должны быть валидны.
unsafe fn encode_bgra_to_vaapi(
    bgra:        &[u8],
    capture_us:  u64,
    _width:       u32,
    _height:      u32,
    src_frame:   &mut Video,
    nv12_frame:  &mut Video,
    scaler:      &mut scaling::Context,
    hw_frames_ref: *mut AVBufferRef,
) -> Option<*mut AVFrame> {
    unsafe {
        src_frame.data_mut(0)[..bgra.len()].copy_from_slice(bgra);
        scaler.run(src_frame, nv12_frame).ok()?;
        nv12_frame.set_pts(Some(capture_us as i64));

        let mut hw_frame = av_frame_alloc();
        if av_hwframe_get_buffer(hw_frames_ref, hw_frame, 0) != 0 {
            av_frame_free(&mut hw_frame);
            return None;
        }
        if av_hwframe_transfer_data(hw_frame, nv12_frame.as_ptr(), 0) < 0 {
            av_frame_free(&mut hw_frame);
            return None;
        }
        (*hw_frame).pts = capture_us as i64;
        Some(hw_frame)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DMA-BUF путь: DRM_PRIME (BGRA) → VAAPI surface (zero CPU copy)
// ─────────────────────────────────────────────────────────────────────────────

/// Импортирует DMA-BUF fd как `AVFrame(AV_PIX_FMT_DRM_PRIME)` и переносит
/// на VAAPI surface через `av_hwframe_transfer_data` (VA-VPP, без участия CPU).
///
/// Возвращает `*mut AVFrame` (AV_PIX_FMT_VAAPI) или `None` при ошибке.
///
unsafe fn encode_dmabuf_to_vaapi(
    fd:            RawFd,
    stride:        u32,
    offset:        u32,
    modifier:      u64,   // DRM-модификатор, согласованный PipeWire
    capture_us:    u64,
    width:         u32,
    height:        u32,
    hw_frames_ref:  *mut AVBufferRef, // VAAPI frames ctx
    drm_frames_ref: *mut AVBufferRef, // DRM frames ctx (может быть null)
) -> Option<*mut AVFrame> {
    if drm_frames_ref.is_null() {
        log::warn!("[Encoder] DRM frames ctx не инициализирован, DmaBuf кадр дропается");
        return None;
    }

    // ── Формируем AVDRMFrameDescriptor ──────────────────────────────────────
    let safe_fd = unsafe { libc::dup(fd) };
    if safe_fd < 0 {
        return None;
    }
    let mut desc: AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
    desc.nb_objects = 1;
    desc.objects[0].fd = safe_fd;
    desc.objects[0].size = (stride * height) as usize;
    desc.objects[0].format_modifier = modifier;
    desc.nb_layers = 1;
    desc.layers[0].format = DRM_FORMAT_ARGB8888;
    desc.layers[0].nb_planes = 1;
    desc.layers[0].planes[0].object_index = 0;
    desc.layers[0].planes[0].offset = offset as isize;
    desc.layers[0].planes[0].pitch = stride as isize;

    let desc_ptr = Box::into_raw(Box::new(desc));

    // ── DRM_PRIME frame ──────────────────────────────────────────────────────
    let mut drm_frame = unsafe { av_frame_alloc() };
    if drm_frame.is_null() {
        unsafe { drop(Box::from_raw(desc_ptr)) };
        return None;
    }

    unsafe {
        (*drm_frame).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*drm_frame).width = width as i32;
        (*drm_frame).height = height as i32;
        (*drm_frame).hw_frames_ctx = av_buffer_ref(drm_frames_ref);

        (*drm_frame).buf[0] = av_buffer_create(
            desc_ptr as *mut u8,
            std::mem::size_of::<AVDRMFrameDescriptor>(),
            Some(free_drm_descriptor),
            std::ptr::null_mut(),
            0,
        );
        if (*drm_frame).buf[0].is_null() {
            drop(Box::from_raw(desc_ptr));
            av_frame_free(&mut drm_frame);
            return None;
        }
        (*drm_frame).data[0] = desc_ptr as *mut u8;
        (*drm_frame).pts = capture_us as i64;
    }

    // ── Импорт в VAAPI (surface-import) ──────────────────────────────────────
    let mut imported = av_frame_alloc();
    unsafe {
        if imported.is_null() {
            av_frame_free(&mut drm_frame);
            return None;
        }

        (*imported).format = AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*imported).width = width as i32;
        (*imported).height = height as i32;
        (*imported).hw_frames_ctx = av_buffer_ref(hw_frames_ref);

        // log::warn!(
        //     "[DMA DESC] fourcc={:#x} modifier={:#x} planes={} pitch={} offset={}",
        //     desc.layers[0].format,
        //     desc.objects[0].format_modifier,
        //     desc.layers[0].nb_planes,
        //     desc.layers[0].planes[0].pitch,
        //     desc.layers[0].planes[0].offset
        // );
        // log::warn!(
        //     "[DMA DEBUG] fd={} stride={} offset={} modifier={:#x} size={}",
        //     fd,
        //     stride,
        //     offset,
        //     modifier,
        //     (stride * height)
        // );

        let ret = av_hwframe_map(
            imported,
            drm_frame,
            (AV_HWFRAME_MAP_READ as u32 | AV_HWFRAME_MAP_DIRECT as u32) as i32,
        );
        (*imported).buf[1] = av_buffer_ref((*drm_frame).buf[0]);
        av_frame_free(&mut drm_frame);

        if ret < 0 {
            log::warn!("[Encoder] av_hwframe_map(DmaBuf→VAAPI) failed: {ret}");
            av_frame_free(&mut imported);
            return None;
        }
    }

    Some(imported)
}

/// Деструктор для AVDRMFrameDescriptor, привязанного к AVBufferRef.
unsafe extern "C" fn free_drm_descriptor(_opaque: *mut libc::c_void, data: *mut u8) {
    let desc = Box::from_raw(data as *mut AVDRMFrameDescriptor);
    for i in 0..desc.nb_objects as usize {
        if desc.objects[i].fd >= 0 {
            libc::close(desc.objects[i].fd);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Совмещённая инициализация DRM + VAAPI hw contexts
// ─────────────────────────────────────────────────────────────────────────────

/// Создаёт DRM hw device, затем **производит** от него VAAPI device.
///
/// Это ключевое условие работоспособности DMA-BUF zero-copy: оба контекста
/// должны разделять один и тот же fd DRM-устройства, иначе
/// `av_hwframe_transfer_data(DRM_PRIME → VAAPI)` вернёт EINVAL (-22).
///
/// Возвращает `(vaapi_frames_ref, drm_frames_ref)`.
/// `drm_frames_ref` может быть null если DRM недоступен (тогда DMA-BUF путь
/// отключается, кадры дропаются).
///
/// # Safety
/// `codec_ctx` должен быть валидным ещё не открытым `AVCodecContext`.
unsafe fn init_hw_contexts(
    codec_ctx: *mut AVCodecContext,
    width:     u32,
    height:    u32,
) -> (*mut AVBufferRef, *mut AVBufferRef, *mut AVBufferRef, Option<VaapiConvertGraph>) {
    // ── Шаг 1: открываем DRM-устройство ─────────────────────────────────────

    let candidates = [
        "/dev/dri/renderD128",
        "/dev/dri/renderD129",
        "/dev/dri/card0",
        "/dev/dri/card1",
    ];
    let mut drm_dev: *mut AVBufferRef = ptr::null_mut();
    for node in &candidates {
        let c = std::ffi::CString::new(*node).unwrap();
        let ret = av_hwdevice_ctx_create(
            &mut drm_dev,
            AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            c.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if ret >= 0 {
            log::info!("[Encoder] DRM device opened: {node}");
            break;
        }
    }
    if drm_dev.is_null() {
        log::warn!("[Encoder] Не удалось открыть DRM device — DMA-BUF недоступен");
        // Fallback: создаём VAAPI напрямую (без DRM shared fd).
        // DMA-BUF путь не будет работать, но CPU-путь останется.
        let mut vaapi_dev: *mut AVBufferRef = ptr::null_mut();
        let node = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        let ret = av_hwdevice_ctx_create(
            &mut vaapi_dev,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        assert!(ret >= 0, "av_hwdevice_ctx_create(VAAPI) failed: {ret}");
        let vaapi_frames = create_vaapi_frames(codec_ctx, vaapi_dev, width, height);
        let convert_graph = init_vaapi_convert_graph(vaapi_dev, vaapi_frames, width, height);
        av_buffer_unref(&mut vaapi_dev);
        return (vaapi_frames, ptr::null_mut(), ptr::null_mut(), convert_graph);
    }

    // ── Шаг 2: производим VAAPI-девайс от DRM-девайса (shared fd) ───────────
    //
    // av_hwdevice_ctx_create_derived гарантирует, что VAAPI и DRM используют
    // один и тот же /dev/dri/renderDxxx, что позволяет шарить DMA-BUF объекты.

    let mut vaapi_dev: *mut AVBufferRef = ptr::null_mut();
    let ret = av_hwdevice_ctx_create_derived(
        &mut vaapi_dev,
        AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        drm_dev,
        0,
    );
    if ret < 0 {
        log::warn!("[Encoder] av_hwdevice_ctx_create_derived(VAAPI←DRM) failed: {ret}. \
                    Fallback: создаём VAAPI независимо (DMA-BUF может не работать).");
        av_buffer_unref(&mut drm_dev);
        let mut vaapi_dev2: *mut AVBufferRef = ptr::null_mut();
        let node = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        let r2 = av_hwdevice_ctx_create(
            &mut vaapi_dev2,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        assert!(r2 >= 0, "av_hwdevice_ctx_create(VAAPI) failed: {r2}");
        let vaapi_frames = create_vaapi_frames(codec_ctx, vaapi_dev2, width, height);
        let convert_graph = init_vaapi_convert_graph(vaapi_dev2, vaapi_frames, width, height);
        av_buffer_unref(&mut vaapi_dev2);
        return (vaapi_frames, ptr::null_mut(), ptr::null_mut(), convert_graph);
    }

    // ── Шаг 3: VAAPI frames context (NV12 pool для кодировщика) ─────────────

    let vaapi_enc_frames = create_vaapi_frames(codec_ctx, vaapi_dev, width, height);
    let vaapi_import_frames = create_import_vaapi_frames(vaapi_dev, width, height);
    let drm_frames = av_hwframe_ctx_alloc(drm_dev);

    let convert_graph = init_vaapi_convert_graph(vaapi_dev, vaapi_import_frames, width, height);

    // ── Шаг 4: DRM frames context (для импорта внешних DMA-BUF) ─────────────

    if drm_frames.is_null() {
        log::warn!("[Encoder] av_hwframe_ctx_alloc(DRM) вернул null");
        av_buffer_unref(&mut drm_dev);
        av_buffer_unref(&mut vaapi_dev);
        return (vaapi_enc_frames, ptr::null_mut(), ptr::null_mut(), convert_graph);
    }
    {
        let fc          = &mut *((*drm_frames).data as *mut AVHWFramesContext);
        fc.format       = AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
        fc.sw_format    = AVPixelFormat::AV_PIX_FMT_BGRA;
        fc.width        = width  as i32;
        fc.height       = height as i32;
        fc.initial_pool_size = 0; // внешняя аллокация, пул не нужен
    }
    let ret = av_hwframe_ctx_init(drm_frames);
    if ret < 0 {
        log::warn!("[Encoder] av_hwframe_ctx_init(DRM) failed: {ret}");
        let mut r = drm_frames;
        av_buffer_unref(&mut r);
        av_buffer_unref(&mut drm_dev);
        av_buffer_unref(&mut vaapi_dev);
        return (vaapi_enc_frames, ptr::null_mut(), ptr::null_mut(), None);
    }

    // Refs на device больше не нужны — frames contexts держат их внутри.
    av_buffer_unref(&mut drm_dev);
    av_buffer_unref(&mut vaapi_dev);

    log::info!("[Encoder] DRM+VAAPI shared device context инициализирован.");
    (vaapi_enc_frames, vaapi_import_frames, drm_frames, convert_graph)
}

unsafe fn create_import_vaapi_frames(
    vaapi_dev: *mut AVBufferRef,
    width: u32, height: u32
) -> *mut AVBufferRef {
    let frames = av_hwframe_ctx_alloc(vaapi_dev);
    let fc = &mut *((*frames).data as *mut AVHWFramesContext);
    fc.format = AVPixelFormat::AV_PIX_FMT_VAAPI;
    fc.sw_format = AVPixelFormat::AV_PIX_FMT_BGRA; // <--- Важно!
    fc.width = width as i32;
    fc.height = height as i32;
    fc.initial_pool_size = 0; // Для маппинга внешних буферов пул не нужен
    av_hwframe_ctx_init(frames);
    frames
}

/// Вспомогательная: создаёт AVHWFramesContext (NV12/VAAPI) и прописывает его
/// в `codec_ctx->hw_frames_ctx`.
unsafe fn create_vaapi_frames(
    codec_ctx: *mut AVCodecContext,
    vaapi_dev: *mut AVBufferRef,
    width:     u32,
    height:    u32,
) -> *mut AVBufferRef {
    let frames = av_hwframe_ctx_alloc(vaapi_dev);
    assert!(!frames.is_null(), "av_hwframe_ctx_alloc(VAAPI) returned null");
    {
        let fc               = &mut *((*frames).data as *mut AVHWFramesContext);
        fc.format            = AVPixelFormat::AV_PIX_FMT_VAAPI;
        fc.sw_format         = AVPixelFormat::AV_PIX_FMT_NV12;
        fc.width             = width  as i32;
        fc.height            = height as i32;
        fc.initial_pool_size = 20;
    }
    let ret = av_hwframe_ctx_init(frames);
    assert!(ret >= 0, "av_hwframe_ctx_init(VAAPI) failed: {ret}");
    (*codec_ctx).hw_frames_ctx = av_buffer_ref(frames);
    frames
}