//! VAAPI HEVC encoder.
//!
//! Receives raw BGRA frames from the PipeWire capture thread via a bounded
//! sync channel, converts them to NV12, uploads to a VAAPI hardware surface,
//! and calls `hevc_vaapi` to produce HEVC NAL units which are forwarded to
//! the QUIC transport layer.
//!
//! # Design decisions
//!
//! - **Double-buffering**: two pre-allocated BGRA buffers rotate through the
//!   pipeline.  If the encoder falls behind, `encode()` drops the frame
//!   immediately (no blocking, no unbounded growth).
//! - **Decoupled from transport**: the encoder receives a
//!   `tokio::sync::mpsc::Sender<(Vec<u8>, bool)>` instead of `Arc<QuicServer>`,
//!   keeping this module free of network concerns.
//! - **gop_size** and low-latency codec options are set *before* `open_with`,
//!   via `av_opt_set`, to guarantee they take effect.

use libc::{c_int, mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ};
use pipewire::spa::sys as spa_sys;
use pipewire::sys as pw_sys;
use std::{ptr, slice, sync::Arc};

use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, format::Pixel, software::scaling, util::frame::video::Video};
use ffmpeg_next::ffi::*;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use tokio::sync::mpsc as async_mpsc;

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// VAAPI HEVC encoder with integrated frame-drop back-pressure.
pub struct Encoder {
    /// Send a BGRA buffer to the worker thread for encoding.
    tx: SyncSender<Vec<u8>>,
    /// Worker thread handle (kept alive for the lifetime of the encoder).
    _worker: thread::JoinHandle<()>,
    /// Return channel for recycled BGRA buffers (double-buffer pool).
    free_rx: mpsc::Receiver<Vec<u8>>,
}

impl Encoder {
    /// Initialise a VAAPI HEVC encoder that delivers encoded frames to `sink`.
    ///
    /// Spawns one OS thread for the codec loop.  Blocks briefly until the
    /// VAAPI device and codec context are initialised, so that the first
    /// call to [`encode`] is ready to accept frames immediately.
    ///
    /// # Panics
    /// Panics if `hevc_vaapi` is not available or VAAPI device creation fails.
    pub fn new(width: u32, height: u32, sink: async_mpsc::Sender<(Vec<u8>, bool)>) -> Self {
        let (tx, rx)         = mpsc::sync_channel::<Vec<u8>>(2);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (free_tx, free_rx)   = mpsc::channel::<Vec<u8>>();

        // Pre-allocate two BGRA frame buffers.
        let buf_size = (width * height * 4) as usize;
        free_tx.send(vec![0u8; buf_size]).unwrap();
        free_tx.send(vec![0u8; buf_size]).unwrap();

        ffmpeg::init().unwrap();

        let worker = thread::Builder::new()
            .name("vaapi-encoder".into())
            .spawn(move || {
                run_encoder_loop(
                    width, height,
                    rx, free_tx,
                    ready_tx,
                    sink,
                );
            })
            .expect("failed to spawn encoder thread");

        // Wait until the codec context is fully initialised before returning.
        ready_rx.recv().expect("encoder init signal");

        Self { tx, _worker: worker, free_rx }
    }

    /// Submit a raw BGRA frame for encoding.
    ///
    /// If the double-buffer pool is empty (encoder is behind), the frame is
    /// silently dropped — this is the intended behaviour for a live stream.
    pub fn encode(&self, frame: &[u8]) {
        if let Ok(mut buf) = self.free_rx.try_recv() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            let _ = self.tx.try_send(buf);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoder worker loop (runs on its own OS thread)
// ─────────────────────────────────────────────────────────────────────────────

fn run_encoder_loop(
    width:    u32,
    height:   u32,
    rx:       mpsc::Receiver<Vec<u8>>,
    free_tx:  mpsc::Sender<Vec<u8>>,
    ready_tx: mpsc::Sender<()>,
    sink:     async_mpsc::Sender<(Vec<u8>, bool)>,
) {
    // ── Codec setup ──────────────────────────────────────────────────────────

    let codec = codec::encoder::find_by_name("hevc_vaapi")
        .expect("hevc_vaapi encoder not found; ensure VA-API is available");

    let mut enc_ctx = codec::context::Context::new_with_codec(codec);

    unsafe {
        let raw = enc_ctx.as_mut_ptr();
        (*raw).width     = width as i32;
        (*raw).height    = height as i32;
        (*raw).time_base = AVRational { num: 1, den: 60 };
        (*raw).pix_fmt   = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*raw).bit_rate  = 5_000_000;
        (*raw).max_b_frames = 0;  // B-frames add latency
        (*raw).delay        = 0;  // zero-delay mode
        // Embed SPS/PPS in every IDR frame so receivers can join mid-stream.
        (*raw).flags &= !(AV_CODEC_FLAG_GLOBAL_HEADER as i32);
    }

    // Low-latency codec options — must be set BEFORE avcodec_open2.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("async_depth",   "1"); // single-frame pipeline depth
    opts.set("low_delay_brc", "1"); // rate-control without look-ahead

    let hw_frames_ref = unsafe {
        init_vaapi_ctx(enc_ctx.as_mut_ptr(), width, height)
    };

    // gop_size via av_opt_set (private option, must be before open).
    unsafe {
        let key = std::ffi::CString::new("g").unwrap();
        let val = std::ffi::CString::new("20").unwrap();
        av_opt_set(
            (*enc_ctx.as_mut_ptr()).priv_data,
            key.as_ptr(), val.as_ptr(), 0,
        );
    }

    let mut encoder = enc_ctx
        .encoder()
        .video()
        .expect("video encoder")
        .open_with(opts)
        .expect("avcodec_open2 failed");

    // ── Scaler BGRA → NV12 (CPU, runs once per frame) ────────────────────────

    let mut scaler = scaling::Context::get(
        Pixel::BGRA, width, height,
        Pixel::NV12, width, height,
        scaling::Flags::BILINEAR,
    )
    .expect("SwsContext init failed");

    // Signal that initialisation is complete.
    ready_tx.send(()).unwrap();

    // ── Frame loop ───────────────────────────────────────────────────────────

    let mut frame_idx  = 0i64;
    let mut started    = false;
    let frame_interval = Duration::from_micros(16_667); // ~60 fps
    let mut next_tick  = std::time::Instant::now();

    let mut src_frame  = Video::new(Pixel::BGRA, width, height);
    let mut nv12_frame = Video::new(Pixel::NV12, width, height);

    loop {
        // Drain the input channel, keeping only the *most recent* frame.
        // This implements "always encode the newest frame" without buffering.
        let mut last_bgra: Option<Vec<u8>> = None;
        while let Ok(frame) = rx.try_recv() {
            if let Some(old) = last_bgra.replace(frame) {
                let _ = free_tx.send(old);
            }
        }

        if let Some(bgra) = last_bgra {
            // Copy raw BGRA into the ffmpeg source frame.
            src_frame.data_mut(0)[..bgra.len()].copy_from_slice(&bgra);

            // CPU colour-space conversion: BGRA → NV12.
            scaler.run(&src_frame, &mut nv12_frame).unwrap();
            nv12_frame.set_pts(Some(frame_idx));

            unsafe {
                let mut hw_frame = av_frame_alloc();
                if av_hwframe_get_buffer(hw_frames_ref, hw_frame, 0) == 0 {
                    // Upload NV12 to the VAAPI surface.
                    av_hwframe_transfer_data(hw_frame, nv12_frame.as_ptr(), 0);
                    (*hw_frame).pts = frame_idx;

                    if avcodec_send_frame(encoder.as_mut_ptr(), hw_frame) >= 0 {
                        let mut pkt = ffmpeg::Packet::empty();
                        while encoder.receive_packet(&mut pkt).is_ok() {
                            let is_key = pkt.is_key();
                            if !started {
                                if !is_key { continue; } // skip until first IDR
                                started = true;
                                log::info!("[Encoder] First IDR frame produced");
                            }
                            if let Some(data) = pkt.data() {
                                // try_send: if the QUIC channel is full, drop.
                                match sink.try_send((data.to_vec(), is_key)) {
                                    Ok(_) => {}
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        log::debug!("[Encoder] sink full, dropping frame");
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        log::warn!("[Encoder] sink closed, shutting down");
                                        av_frame_free(&mut hw_frame);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
                av_frame_free(&mut hw_frame);
            }

            let _ = free_tx.send(bgra);
            frame_idx += 1;
        }

        // Rate limiter: sleep until the next 60 fps tick.
        // If we're already past it (slow encode), reset to avoid spiral.
        next_tick += frame_interval;
        let now = std::time::Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            next_tick = now;
        }
    }
}

// Bring Duration into scope for the worker function.
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────────
// VAAPI device + frame-context initialisation
// ─────────────────────────────────────────────────────────────────────────────

/// Create a VAAPI hardware device and frame context.
///
/// # Safety
/// `codec_ctx` must be a valid, not-yet-opened `AVCodecContext`.
/// The returned `*mut AVBufferRef` is owned by the codec context and must not
/// be freed independently.
unsafe fn init_vaapi_ctx(
    codec_ctx: *mut AVCodecContext,
    width:     u32,
    height:    u32,
) -> *mut AVBufferRef {
    let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
    let device = std::ffi::CString::new("/dev/dri/renderD128").unwrap();

    let ret = unsafe {
        av_hwdevice_ctx_create(
            &mut hw_device_ctx,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device.as_ptr(),
            ptr::null_mut(),
            0,
        )
    };
    assert!(ret >= 0, "av_hwdevice_ctx_create failed: {ret}");

    unsafe {
        let hw_frames_ref = av_hwframe_ctx_alloc(hw_device_ctx);
        assert!(!hw_frames_ref.is_null(), "av_hwframe_ctx_alloc returned null");

        let frames_ctx = &mut *((*hw_frames_ref).data as *mut AVHWFramesContext);
        frames_ctx.format           = AVPixelFormat::AV_PIX_FMT_VAAPI;
        frames_ctx.sw_format        = AVPixelFormat::AV_PIX_FMT_NV12;
        frames_ctx.width            = width as i32;
        frames_ctx.height           = height as i32;
        frames_ctx.initial_pool_size = 20;

        let ret = av_hwframe_ctx_init(hw_frames_ref);
        assert!(ret >= 0, "av_hwframe_ctx_init failed: {ret}");

        (*codec_ctx).hw_frames_ctx = av_buffer_ref(hw_frames_ref);
        hw_frames_ref
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PipeWire buffer → raw slice helper
// ─────────────────────────────────────────────────────────────────────────────

/// Extract a raw pixel slice from a PipeWire buffer and call `f` with it.
///
/// Handles both `SPA_DATA_MemPtr` (shared memory pointer) and
/// `SPA_DATA_MemFd` / `SPA_DATA_DmaBuf` (fd-mapped memory) cases.
///
/// # Safety
/// `buffer` must be a valid, non-null `pw_buffer` obtained from
/// `stream.dequeue_raw_buffer()` and not yet returned via
/// `stream.queue_raw_buffer()`.
pub unsafe fn process_frame_from_pw_buffer<F>(buffer: *mut pw_sys::pw_buffer, mut f: F)
where
    F: FnMut(&[u8]),
{
    unsafe {
        if buffer.is_null() || (*buffer).buffer.is_null() { return; }
        let spa_buf = &*(*buffer).buffer;
        if spa_buf.n_datas == 0 || spa_buf.datas.is_null() { return; }

        let data  = &*spa_buf.datas;
        let chunk = data.chunk.as_ref().unwrap();
        let offset = chunk.offset as usize;
        let size   = chunk.size   as usize;
        if size == 0 { return; }

        match data.type_ {
            spa_sys::SPA_DATA_MemFd | spa_sys::SPA_DATA_DmaBuf => {
                let map_len = data.maxsize as usize;
                let mapped  = mmap(
                    ptr::null_mut(), map_len, PROT_READ,
                    MAP_SHARED, data.fd as c_int, data.mapoffset as libc::off_t,
                );
                if mapped != MAP_FAILED {
                    if offset + size <= map_len {
                        let src = slice::from_raw_parts(
                            (mapped as *const u8).add(offset), size,
                        );
                        f(src);
                    }
                    munmap(mapped, map_len);
                }
            }
            spa_sys::SPA_DATA_MemPtr => {
                if !data.data.is_null() {
                    let src = slice::from_raw_parts(
                        (data.data as *const u8).add(offset), size,
                    );
                    f(src);
                }
            }
            _ => {}
        }
    }
}
