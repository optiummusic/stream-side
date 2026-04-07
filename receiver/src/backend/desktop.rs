// src/backend/desktop.rs
//
// Десктопный бекенд: программный HEVC-декодер через ffmpeg-next.
// Используется на Linux и Windows.
//
// Поток данных:
//   VideoPacket.payload (HEVC NAL) → ffmpeg → YUV420P кадр → WgpuState → экран
use std::collections::HashMap;
use common::FrameTrace;
use ffmpeg_next::{
    codec,
    format::Pixel,
    software::scaling,
    util::frame::video::Video,
};

use super::{BackendError, FrameOutput, VideoBackend, YuvFrame};

pub struct DesktopFfmpegBackend {
    decoder:  ffmpeg_next::decoder::Video,
    scaler:   Option<scaling::Context>,
    last_fmt: Pixel,
    pending_traces: HashMap<u64, Option<FrameTrace>>,
}

impl DesktopFfmpegBackend {
    /// Инициализировать ffmpeg и открыть HEVC-декодер.
    pub fn new() -> Result<Self, BackendError> {
        ffmpeg_next::init()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;
        ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
        let codec = codec::decoder::find(codec::Id::HEVC)
            .ok_or_else(|| BackendError::ConfigError(
                "HEVC codec not found. Install ffmpeg with HEVC support.".into()
            ))?;

        let ctx = codec::context::Context::new();

        let decoder = ctx
            .decoder()
            .open_as(codec)
            .map_err(|e| BackendError::ConfigError(e.to_string()))?
            .video()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;

        log::info!("DesktopFfmpegBackend: HEVC decoder opened");
        Ok(Self {
            decoder,
            scaler:   None,
            last_fmt: Pixel::None,
            pending_traces: HashMap::new(),
        })
    }
}

unsafe impl Send for DesktopFfmpegBackend {}
impl VideoBackend for DesktopFfmpegBackend {
    fn push_encoded(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>) -> Result<(), BackendError> {
        let mut pkt = ffmpeg_next::Packet::new(payload.len());
        if let Some(dst) = pkt.data_mut() {
            dst.copy_from_slice(payload);
        }
        pkt.set_pts(Some(frame_id as i64));  // ← было None
        pkt.set_dts(Some(frame_id as i64));

        // log::info!("PAYLOAD SIZE IN DECODER FOR FRAME {}:{}", frame_id, payload.len());
        self.decoder.send_packet(&pkt)
            .map_err(|e| BackendError::DecodeError(e.to_string()))?;
        self.pending_traces.insert(frame_id, trace);

        // Защита от утечки: чистим трейсы кадров, которые декодер навсегда дропнул
        const TRACE_HORIZON: u64 = 120; // ~2 сек при 60fps
        if frame_id > TRACE_HORIZON {
            self.pending_traces.retain(|&k, _| k >= frame_id - TRACE_HORIZON);
        }
        Ok(())
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        let mut raw = Video::empty();

        // receive_frame вернёт EAGAIN если кадр ещё не готов
        if self.decoder.receive_frame(&mut raw).is_err() {
            return Ok(FrameOutput::Pending);
        }

        let frame_id = raw.pts().unwrap_or(0) as u64;

        let trace = self.pending_traces
            .remove(&frame_id)
            .unwrap_or(None); // дефолт если трейс уже вычищен горизонтом

        let mut trace = trace.unwrap_or_default();
        trace.decode_us = FrameTrace::now_us();

        let fmt = raw.format();
        let (w, h) = (raw.width(), raw.height());

        // Перестраиваем scaler только при смене пиксельного формата.
        // VAAPI обычно отдаёт NV12; software-декодер — YUV420P.
        // Наш WGSL-шейдер ожидает три раздельных плоскости (YUV420P),
        // поэтому нормализуем всё к нему.
        if fmt != self.last_fmt {
            self.last_fmt = fmt;
            self.scaler = if fmt == Pixel::YUV420P {
                None // уже в нужном формате — scaler не нужен
            } else {
                log::debug!("Creating scaler: {:?} → YUV420P", fmt);
                Some(
                    scaling::Context::get(
                        fmt, w, h,
                        Pixel::YUV420P, w, h,
                        scaling::Flags::BILINEAR,
                    )
                    .map_err(|e| BackendError::DecodeError(e.to_string()))?,
                )
            };
        }

        let yuv = if let Some(ref mut sc) = self.scaler {
            let mut out = Video::new(Pixel::YUV420P, w, h);
            sc.run(&raw, &mut out)
                .map_err(|e| BackendError::DecodeError(e.to_string()))?;
            out
        } else {
            // Уже YUV420P — копируем плоскости вручную
            let mut copy = Video::new(Pixel::YUV420P, w, h);
            for i in 0..3 {
                let stride   = raw.stride(i);
                let plane_h  = if i == 0 { h as usize } else { h as usize / 2 };
                copy.data_mut(i)[..stride * plane_h]
                    .copy_from_slice(&raw.data(i)[..stride * plane_h]);
            }
            copy
        };

        Ok(FrameOutput::Yuv(YuvFrame {
            frame_id,
            trace,
            width:    w,
            height:   h,
            y:        yuv.data(0)[..yuv.stride(0) * h as usize].to_vec(),
            u:        yuv.data(1)[..yuv.stride(1) * h as usize / 2].to_vec(),
            v:        yuv.data(2)[..yuv.stride(2) * h as usize / 2].to_vec(),
            y_stride: yuv.stride(0) as u32,
            u_stride: yuv.stride(1) as u32,
            v_stride: yuv.stride(2) as u32,
        }))
    }

    fn shutdown(&mut self) {
        // ffmpeg-next освобождает ресурсы через Drop автоматически
        log::info!("DesktopFfmpegBackend: shutdown");
    }
}