use libc::{c_int, mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ};
use pipewire::spa::sys as spa_sys;
use pipewire::sys as pw_sys;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{ptr, slice};
use ffmpeg_next as ffmpeg;
use ffmpeg::codec;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::video::Video;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use ffmpeg_next::ffi::*;
use crate::quic::QuicServer;

pub struct Encoder {
    tx: SyncSender<Vec<u8>>,
    _worker: thread::JoinHandle<()>,
    free_rx: std::sync::mpsc::Receiver<Vec<u8>>,
}

unsafe fn init_vaapi_ctx(
    codec_ctx: *mut AVCodecContext,
    width: u32,
    height: u32,
) -> *mut AVBufferRef {
    let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
    let device = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
    unsafe {
        let ret = av_hwdevice_ctx_create(
            &mut hw_device_ctx,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device.as_ptr(),
            ptr::null_mut(),
            0,
        );
        assert!(ret >= 0, "Failed to create VAAPI device: {}", ret);
    }

    unsafe {
        let hw_frames_ref = av_hwframe_ctx_alloc(hw_device_ctx);
        assert!(!hw_frames_ref.is_null());
        let frames_ctx = &mut *((*hw_frames_ref).data as *mut AVHWFramesContext);
        frames_ctx.format    = AVPixelFormat::AV_PIX_FMT_VAAPI;
        frames_ctx.sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
        frames_ctx.width     = width as i32;
        frames_ctx.height    = height as i32;
        frames_ctx.initial_pool_size = 20;

        let ret = av_hwframe_ctx_init(hw_frames_ref);
        assert!(ret >= 0, "Failed to init hw frames ctx: {}", ret);
        (*codec_ctx).hw_frames_ctx = av_buffer_ref(hw_frames_ref);
        hw_frames_ref
    }
}

impl Encoder {
    pub fn new(width: u32, height: u32, server: Arc<QuicServer>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(2);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (free_tx, free_rx) = mpsc::channel::<Vec<u8>>();

        // Пул из двух pre-allocated буферов (double buffering)
        let buf_size = (width * height * 4) as usize;
        free_tx.send(vec![0u8; buf_size]).unwrap();
        free_tx.send(vec![0u8; buf_size]).unwrap();
        ffmpeg::init().unwrap();


        let worker = thread::spawn(move || {
            let codec = codec::encoder::find_by_name("hevc_vaapi")
                .expect("hevc_vaapi not found");

            let mut enc_ctx = codec::context::Context::new_with_codec(codec);

            unsafe {
                let raw = enc_ctx.as_mut_ptr();
                (*raw).width     = width as i32;
                (*raw).height    = height as i32;
                (*raw).time_base = AVRational { num: 1, den: 60 };
                (*raw).pix_fmt   = AVPixelFormat::AV_PIX_FMT_VAAPI;

                let br = 5_000_000i64;
                (*raw).bit_rate        = br;
                (*raw).max_b_frames = 0;
                (*raw).delay = 0;

                (*raw).flags &= !ffmpeg_next::ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
                let key = std::ffi::CString::new("extradata_idr").unwrap();
                let val = std::ffi::CString::new("1").unwrap();
                ffmpeg_next::ffi::av_opt_set((*raw).priv_data, key.as_ptr(), val.as_ptr(), 0);
            }

            let hw_frames_ref = unsafe {
                let raw_ctx = enc_ctx.as_mut_ptr();
                init_vaapi_ctx(raw_ctx, width, height)
            };

            let mut opts = ffmpeg::Dictionary::new();
            opts.set("async_depth", "1");
            opts.set("low_delay_brc", "1");

            let mut encoder = enc_ctx
                .encoder()
                .video()
                .unwrap()
                .open_with(opts)
                .unwrap();

            unsafe {
                (*encoder.as_mut_ptr()).gop_size = 20;
            }

            // Scaler BGRA → NV12
            let mut scaler = scaling::Context::get(
                Pixel::BGRA, width, height,
                Pixel::NV12, width, height,
                scaling::Flags::BILINEAR,
            ).unwrap();

            ready_tx.send(()).unwrap();

            let mut frame_idx = 0i64;
            let frame_duration = std::time::Duration::from_micros(16_667); // ~60 fps
            let mut next_tick  = std::time::Instant::now();

            let mut src_frame  = Video::new(Pixel::BGRA, width, height);
            let mut nv12_frame = Video::new(Pixel::NV12, width, height);

            let mut started = false;
            loop {
                // Дренируем канал — берём только последний кадр, остальные возвращаем в пул
                let mut last_bgra: Option<Vec<u8>> = None;

                while let Ok(frame) = rx.try_recv() {
                    if let Some(old) = last_bgra.take() {
                        let _ = free_tx.send(old);
                    }
                    last_bgra = Some(frame);
                }

                if let Some(bgra) = last_bgra {
                    src_frame.data_mut(0)[..bgra.len()].copy_from_slice(&bgra);

                    // 3. Цветокоррекция (всё еще CPU, но без аллокаций контекста)
                    scaler.run(&src_frame, &mut nv12_frame).unwrap();
                    nv12_frame.set_pts(Some(frame_idx));
                    
                    unsafe {
                        let mut hw_frame_raw = av_frame_alloc(); // Или Video::empty()
                        if av_hwframe_get_buffer(hw_frames_ref, hw_frame_raw, 0) == 0 {
            
                            // Копируем NV12 в HW кадр
                            av_hwframe_transfer_data(hw_frame_raw, nv12_frame.as_ptr(), 0);
                            
                            (*hw_frame_raw).pts = frame_idx;

                            if avcodec_send_frame(encoder.as_mut_ptr(), hw_frame_raw) >= 0 {
                                let mut pkt = ffmpeg::Packet::empty();
                                while encoder.receive_packet(&mut pkt).is_ok() {
                                    // ДОБАВИТЬ BYTES ВМЕСТО TO_VEC
                                    let keyframe = pkt.is_key();
                                    if !started {
                                        if !keyframe {
                                            continue;
                                        }
                                        started = true;
                                        log::info!("[Encoder] Первый I-frame найден");
                                    }
                                    if let Some(data) = pkt.data() {
                                        // Передаем флаг в сервер
                                        server.send(data.to_vec(), keyframe);
                                    }
                                }
                            }
                        }
                        av_frame_free(&mut hw_frame_raw);
                    }
                    frame_idx += 1;
                    let _ = free_tx.send(bgra);
                }

                // Tick-регулятор: выдерживаем ~60 fps, не допуская накопления задержки
                next_tick += frame_duration;
                let now = std::time::Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                } else {
                    next_tick = now;
                }
            }
        });

        ready_rx.recv().unwrap();
        Self { tx, _worker: worker, free_rx }
    }

    /// Принять кадр из PipeWire и поставить в очередь кодирования.
    ///
    /// Если пул буферов пуст (энкодер перегружен) — кадр пропускается.
    pub fn encode(&self, frame: &[u8]) {
        if let Ok(mut buf) = self.free_rx.try_recv() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            let _ = self.tx.try_send(buf);
        }
    }
}

pub unsafe fn process_frame_from_pw_buffer<F>(buffer: *mut pw_sys::pw_buffer, mut f: F)
where F: FnMut(&[u8])
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
                let mapped = mmap(
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
