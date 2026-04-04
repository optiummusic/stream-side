use libc::{c_int, mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ};
use pipewire::spa::sys as spa_sys;
use pipewire::sys as pw_sys;
use std::net::SocketAddr;
use std::{ptr, slice};
use ffmpeg_next as ffmpeg;
use ffmpeg::codec;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::video::Video;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use ffmpeg_next::ffi::*;
use crate::quic::QuicSender;


pub struct Encoder {
    tx: SyncSender<Vec<u8>>,
    _worker: thread::JoinHandle<()>,
}

unsafe fn init_vaapi_ctx(
    codec_ctx: *mut AVCodecContext,
    width: u32,
    height: u32,
) -> *mut AVBufferRef {
    // 1. Создаём hw device
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

    // 3. Настраиваем frames context
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
    pub fn new(width: u32, height: u32, target_addr: SocketAddr) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        let worker = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            // Инициализируем QUIC
            let quic_sender = rt.block_on(async {
                QuicSender::new(target_addr).await
            });
            ffmpeg::init().unwrap();

            let codec = codec::encoder::find_by_name("hevc_vaapi")
                .expect("hevc_vaapi not found");

            // 1. Создаем контекст
            let mut enc_ctx = codec::context::Context::new_with_codec(codec);

            unsafe {
                let raw = enc_ctx.as_mut_ptr();
                (*raw).width = width as i32;
                (*raw).height = height as i32;
                (*raw).time_base = AVRational { num: 1, den: 60 };
                (*raw).pix_fmt = AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*raw).bit_rate = 8_000_000;
            }

            // 3. ПОКА enc_ctx еще жив и не перемещен, получаем raw указатель и инициализируем VAAPI
            let hw_frames_ref = unsafe {
                let raw_ctx = enc_ctx.as_mut_ptr(); // Здесь enc_ctx еще у нас!
                init_vaapi_ctx(raw_ctx, width, height)
            };

            // 4. И только ТЕПЕРЬ окончательно превращаем контекст в открытый энкодер
            // После этого вызова enc_ctx "умрет", и у нас останется только объект encoder
            let mut encoder = enc_ctx
                .encoder()
                .video()
                .unwrap()
                .open()
                .unwrap();
            unsafe {
                (*encoder.as_mut_ptr()).gop_size = 60; // keyframe каждые 60 кадров
            }
            // Scaler BGRA → NV12
            let mut scaler = scaling::Context::get(
                Pixel::BGRA, width, height,
                Pixel::NV12, width, height,
                scaling::Flags::BILINEAR,
            ).unwrap();

            ready_tx.send(()).unwrap();

            let mut frame_idx = 0i64;
            let mut output_frames = 0u64;
            let frame_duration = std::time::Duration::from_micros(16_667);
            let mut next_tick = std::time::Instant::now();

            // Последний известный кадр для повтора
            let mut last_bgra: Option<Vec<u8>> = None;

            loop {
                // Читаем новый кадр если есть
                loop {
                    match rx.try_recv() {
                        Ok(frame) => { last_bgra = Some(frame); }
                        Err(_) => break,
                    }
                }

                if let Some(ref bgra) = last_bgra {
                    let mut src = Video::new(Pixel::BGRA, width, height);
                    src.data_mut(0)[..bgra.len()].copy_from_slice(bgra);

                    let mut nv12 = Video::new(Pixel::NV12, width, height);
                    scaler.run(&src, &mut nv12).unwrap();
                    nv12.set_pts(Some(frame_idx));
                    frame_idx += 1;

                    let mut hw_frame = Video::new(Pixel::VAAPI, width, height);
                    unsafe {
                        av_hwframe_get_buffer(hw_frames_ref, hw_frame.as_mut_ptr(), 0);
                        av_hwframe_transfer_data(hw_frame.as_mut_ptr(), nv12.as_ptr(), 0);
                        (*hw_frame.as_mut_ptr()).pts = frame_idx - 1;
                    }

                    encoder.send_frame(&hw_frame).unwrap();

                    let mut pkt = ffmpeg::Packet::empty();
                    while encoder.receive_packet(&mut pkt).is_ok() {
                        // --- ОТПРАВКА В QUIC ---
                        let data = pkt.data().unwrap().to_vec();
                        quic_sender.send(data);
                    }
                }

                next_tick += frame_duration;
                let now = std::time::Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                }
            }
        });

        ready_rx.recv().unwrap();
        Self { tx, _worker: worker }
    }

    pub fn encode(&self, frame: &[u8]) {
        let _ = self.tx.try_send(frame.to_vec());
    }
}

pub unsafe fn process_frame_from_pw_buffer<F>(buffer: *mut pw_sys::pw_buffer, mut f: F) 
where F: FnMut(&[u8]) 
{
    unsafe {
        if buffer.is_null() || (*buffer).buffer.is_null() { return; }
        let spa_buf = &*(*buffer).buffer;
        if spa_buf.n_datas == 0 || spa_buf.datas.is_null() { return; }

        let data = &*spa_buf.datas;
        let chunk = data.chunk.as_ref().unwrap();
        let offset = chunk.offset as usize;
        let size = chunk.size as usize;

        if size == 0 { return; }

        match data.type_ {
            spa_sys::SPA_DATA_MemFd | spa_sys::SPA_DATA_DmaBuf => {
                let map_len = data.maxsize as usize;
                let mapped = mmap(ptr::null_mut(), map_len, PROT_READ, MAP_SHARED, data.fd as c_int, data.mapoffset as libc::off_t);
                if mapped != MAP_FAILED {
                    if offset + size <= map_len {
                        let src = slice::from_raw_parts((mapped as *const u8).add(offset), size);
                        f(src); // Передаем срез данных напрямую в замыкание (в энкодер)
                    }
                    munmap(mapped, map_len);
                }
            }
            spa_sys::SPA_DATA_MemPtr => {
                if !data.data.is_null() {
                    let src = slice::from_raw_parts((data.data as *const u8).add(offset), size);
                    f(src);
                }
            }
            _ => {}
        }
    }
}