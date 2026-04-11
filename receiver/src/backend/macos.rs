// src/backend/macos.rs
//
// macOS backend: software HEVC decode via ffmpeg-next.
// Produces NV12 (Y + interleaved UV) frames for the existing WGPU path.

use std::collections::HashMap;

use common::FrameTrace;
use ffmpeg_next::{
    codec,
    format::Pixel,
    software::scaling,
    util::frame::video::Video,
    ffi::*,
};

use super::{BackendError, FrameOutput, PushStatus, VideoBackend, YuvFrame};

pub struct MacosFfmpegBackend {
    decoder:        ffmpeg_next::decoder::Video,
    scaler:         Option<scaling::Context>,
    last_fmt:       Pixel,
    pending_traces: HashMap<u64, Option<FrameTrace>>,
    scaler_out:     Option<Video>,
    y_pool:         Vec<u8>,
    uv_pool:        Vec<u8>,
}

// SAFETY: ffmpeg objects are used only on the network thread.
unsafe impl Send for MacosFfmpegBackend {}

impl MacosFfmpegBackend {
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
            (*raw).thread_count = 1;
        }

        let decoder = ctx
            .decoder()
            .open_as(codec)
            .map_err(|e| BackendError::ConfigError(e.to_string()))?
            .video()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;

        Ok(Self {
            decoder,
            scaler:         None,
            last_fmt:       Pixel::None,
            pending_traces: HashMap::new(),
            scaler_out:     None,
            y_pool:         Vec::new(),
            uv_pool:        Vec::new(),
        })
    }
}

impl VideoBackend for MacosFfmpegBackend {
    fn push_encoded(
        &mut self,
        payload:  &[u8],
        frame_id: u64,
        trace:    Option<FrameTrace>,
    ) -> Result<PushStatus, BackendError> {
        let mut pkt = ffmpeg_next::Packet::new(payload.len());
        if let Some(dst) = pkt.data_mut() {
            dst.copy_from_slice(payload);
        }
        pkt.set_pts(Some(frame_id as i64));
        pkt.set_dts(Some(frame_id as i64));

        self.decoder.send_packet(&pkt)
            .map_err(|e| BackendError::DecodeError(e.to_string()))?;

        self.pending_traces.insert(frame_id, trace);

        const TRACE_HORIZON: u64 = 120;
        if frame_id > TRACE_HORIZON {
            self.pending_traces.retain(|&k, _| k >= frame_id - TRACE_HORIZON);
        }
        Ok(PushStatus::Accepted)
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        let mut raw = Video::empty();
        if self.decoder.receive_frame(&mut raw).is_err() {
            return Ok(FrameOutput::Pending);
        }

        let (frame_id, fmt, w, h) = unsafe {
            let f: &AVFrame = &*raw.as_ptr();
            let fid = if f.pts != AV_NOPTS_VALUE { f.pts as u64 } else { 0 };
            let fmt_sys: AVPixelFormat = std::mem::transmute(f.format);
            (fid, Pixel::from(fmt_sys), f.width as u32, f.height as u32)
        };

        let trace = self.pending_traces
            .remove(&frame_id)
            .flatten()
            .unwrap_or_default();
        let mut trace = trace;
        trace.decode_us = FrameTrace::now_us();

        let nv12_ptr: *const AVFrame = if fmt == Pixel::NV12 {
            unsafe { raw.as_ptr() }
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

            let src_video = unsafe { Video::wrap(raw.as_ptr() as *mut AVFrame) };
            sc.run(&src_video, out)
                .map_err(|e| BackendError::DecodeError(e.to_string()))?;
            std::mem::forget(src_video);
            unsafe { out.as_ptr() }
        };

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
        log::info!("[Decoder] MacosFfmpegBackend: shutdown");
    }
}
