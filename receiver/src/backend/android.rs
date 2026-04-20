// src/backend/android.rs
//
// Android-бекенд: аппаратный HEVC-декодер через NDK AMediaCodec.
#[link(name = "mediandk")]
unsafe extern "C" {}

#[link(name = "android")]
unsafe extern "C" {}

use crate::VideoWorkerMsg;
use tokio::sync::watch;
use std::ffi::CString;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::net::ToSocketAddrs;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use jni::{
    objects::{JClass, JObject, JString},
    sys::{jint, jstring, jlong},
    EnvUnowned,
    Env
};
use std::sync::mpsc::SyncSender;
use once_cell::sync::OnceCell;
use bytes::BytesMut;
use std::time::Instant;
use std::time::Duration;
use super::{BackendError, FrameOutput, PushStatus, VideoBackend};
use common::FrameTrace;
// ─────────────────────────────────────────────────────────────────────────────
// Глобальный синглтон бекенда
// ─────────────────────────────────────────────────────────────────────────────
static NETWORK_RT: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);
static LATEST_LATENCY: Mutex<String> = Mutex::new(String::new());
static TRACE_TX: OnceCell<watch::Sender<Option<(u64, FrameTrace)>>> = OnceCell::new();
pub static WORKER_TX: Mutex<Option<SyncSender<VideoWorkerMsg>>> = Mutex::new(None);
pub static PENDING_SURFACE: Mutex<Option<VideoWorkerMsg>> = Mutex::new(None);

static PENDING_VSYNC_INFO: Mutex<Option<(u64, FrameTrace)>> = Mutex::new(None);

fn send_to_worker(msg: VideoWorkerMsg) {
    if let Ok(guard) = WORKER_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(msg);
            return;
        }
    }
    // Воркер ещё не запущен — сохраняем InitSurface для последующей отправки
    if matches!(msg, VideoWorkerMsg::InitSurface { .. }) {
        if let Ok(mut p) = PENDING_SURFACE.lock() {
            *p = Some(msg);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Внутреннее состояние NDK-объектов
// ─────────────────────────────────────────────────────────────────────────────
struct CodecState {
    codec:         *mut ndk_sys::AMediaCodec,
    native_window: *mut ndk_sys::ANativeWindow,
}

unsafe impl Send for CodecState {}
unsafe impl Sync for CodecState {}

impl Drop for CodecState {
    fn drop(&mut self) {
        unsafe {
            ndk_sys::AMediaCodec_stop(self.codec);
            ndk_sys::AMediaCodec_delete(self.codec);
            ndk_sys::ANativeWindow_release(self.native_window);
        }
        log::info!("[MediaCodec] Resources released");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Основная структура бекенда
// ─────────────────────────────────────────────────────────────────────────────
pub struct AndroidMediaCodecBackend {
    state:        Option<CodecState>,
    pts_to_frame: HashMap<u64, (u64, FrameTrace)>,
    current_trace: Option<FrameTrace>,
    slice_buffer: BytesMut,
    fps_counter: u64,
    last_fps_check: Instant,
}

impl AndroidMediaCodecBackend {
    const MAX_PENDING_TRACES: usize = 64;
    pub fn new() -> Self {
        Self {
            state:        None,
            pts_to_frame: HashMap::new(),
            slice_buffer:   BytesMut::with_capacity(1024 * 512),
            current_trace: None,
            fps_counter: 0,
            last_fps_check: Instant::now(),
        }
    }

    fn flush_decoder(&mut self) {
        if let Some(state) = &self.state {
            unsafe {
                ndk_sys::AMediaCodec_flush(state.codec);
            }
            self.pts_to_frame.clear();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Реализация трейта VideoBackend
// ─────────────────────────────────────────────────────────────────────────────
const MAX_AGE_PUSH: f64 = 160.0;
const MAX_AGE_POLL: f64 = 220.0; // Pre Poll
const CRITICAL_AGE_FLUSH: f64 = 500.0;

impl VideoBackend for AndroidMediaCodecBackend {
    fn submit_to_decoder(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>) -> Result<PushStatus, BackendError> {
        let Some(state) = self.state.as_ref() else {
                return Ok(PushStatus::Accumulating); // Surface ещё не пришёл — тихо ждём
        };
        // Drop old chunks
        if let Some(t) = &trace {
            let age_ms = FrameTrace::ms(t.receive_us, FrameTrace::now_us());

            if age_ms > CRITICAL_AGE_FLUSH {
                self.flush_decoder();
                return Ok(PushStatus::Dropped { age: Some(age_ms as f32) });
            }

            if age_ms > MAX_AGE_PUSH {
                return Ok(PushStatus::Dropped{age: Some(age_ms as f32)});
            }
        }

        self.fps_counter += 1;
        let mut fps = 0.0;
        let elapsed = self.last_fps_check.elapsed();
        if elapsed >= Duration::from_secs(1) {
            // Вычисляем FPS (с учетом возможной задержки потока)
            fps = self.fps_counter as f64 / elapsed.as_secs_f64();
                        
            // Сбрасываем состояние
            self.fps_counter = 0;
            self.last_fps_check = Instant::now();
        }

        let pts_us = FrameTrace::now_us(); 
        unsafe {
            let idx = ndk_sys::AMediaCodec_dequeueInputBuffer(state.codec, 2000);
            if idx < 0 { return Ok(PushStatus::Dropped { age: None }); } 

            let mut buf_size: usize = 0;
            let buf_ptr = ndk_sys::AMediaCodec_getInputBuffer(state.codec, idx as usize, &mut buf_size);
            let copy_len = payload.len().min(buf_size);
            ptr::copy_nonoverlapping(payload.as_ptr(), buf_ptr, copy_len);

            if let Some(mut t) = trace {
                t.decode_us = pts_us;
                self.pts_to_frame.insert(pts_us, (frame_id, t));
            }

            ndk_sys::AMediaCodec_queueInputBuffer(state.codec, idx as usize, 0, copy_len, pts_us, 0);
        }
        Ok(PushStatus::Accepted {fps: fps as u32})
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        let Some(state) = self.state.as_ref() else {
            return Ok(FrameOutput::Pending); // Surface ещё не пришёл — тихо ждём
        };
        
        unsafe {
            let mut info = ndk_sys::AMediaCodecBufferInfo { offset: 0, size: 0, presentationTimeUs: 0, flags: 0 };
            let idx = ndk_sys::AMediaCodec_dequeueOutputBuffer(state.codec, &mut info, 0);

            if idx >= 0 {
                let pts = info.presentationTimeUs as u64;
                if let Some((id, t)) = self.pts_to_frame.remove(&pts) {
                    let age = FrameTrace::ms(t.decode_us, FrameTrace::now_us());

                    if age > MAX_AGE_POLL {
                        ndk_sys::AMediaCodec_releaseOutputBuffer(state.codec, idx as usize, false);
                        return Ok(FrameOutput::Dropped { age: Some(age as f32) });
                    }

                    if let Ok(mut pending) = PENDING_VSYNC_INFO.lock() {
                        *pending = Some((id, t));
                    }

                    ndk_sys::AMediaCodec_releaseOutputBuffer(state.codec, idx as usize, true);
                    Ok(FrameOutput::DirectToSurface)
                } else {
                    ndk_sys::AMediaCodec_releaseOutputBuffer(state.codec, idx as usize, false);
                    Ok(FrameOutput::Dropped { age: None })
                }
            } else {
                Ok(FrameOutput::Pending)
            }
        }
    }
    fn get_current_trace(&mut self) -> &mut Option<FrameTrace> {
        &mut self.current_trace
    }
    fn get_slice_buffer(&mut self) -> &mut BytesMut {
        &mut self.slice_buffer
    }

    fn clear_buffer(&mut self) {
        self.get_slice_buffer().clear();
        self.get_current_trace().take();
    }

    fn shutdown(&mut self) {
        self.state = None;
        self.pts_to_frame.clear();
    }

        
    unsafe fn init_with_surface(
        &mut self,
        native_window: *mut ndk_sys::ANativeWindow,
        width:         i32,
        height:        i32,
    ) -> Result<(), BackendError> {
        self.shutdown();

        let mime   = CString::new("video/hevc").unwrap();
        let codec  = unsafe { ndk_sys::AMediaCodec_createDecoderByType(mime.as_ptr()) };

        if codec.is_null() {
            return Err(BackendError::ConfigError(
                "AMediaCodec_createDecoderByType(video/hevc) returned null.".into()
            ));
        } 
    
        let format = unsafe { ndk_sys::AMediaFormat_new() };

        macro_rules! fmt_str {
            ($k:expr, $v:expr) => {{
                let k = CString::new($k).unwrap();
                let v = CString::new($v).unwrap();
                unsafe {
                    ndk_sys::AMediaFormat_setString(format, k.as_ptr(), v.as_ptr());
                }
            }};
        }
        macro_rules! fmt_i32 {
            ($k:expr, $v:expr) => {{
                let k = CString::new($k).unwrap();
                unsafe {
                    ndk_sys::AMediaFormat_setInt32(format, k.as_ptr(), $v);
                }
            }};
        }
        

        fmt_str!("mime",              "video/hevc");
        fmt_i32!("width",             width);
        fmt_i32!("height",            height);
        fmt_i32!("max-input-size",    4 * 1024 * 1024);
        fmt_i32!("adaptive-playback", 1);
        fmt_i32!("low-latency",       1);
        fmt_i32!("priority",          0);
        fmt_i32!("operating-rate",    120);
        fmt_i32!("vendor.qti-ext-dec-low-latency.enable", 1);
        fmt_i32!("latency",           0);
        fmt_i32!("async-crpto",       0);


        let status = unsafe { ndk_sys::AMediaCodec_configure(
            codec, format, native_window, ptr::null_mut(), 0,
        )};
        unsafe { ndk_sys::AMediaFormat_delete(format) };

        if status != ndk_sys::media_status_t(0) {
            unsafe { ndk_sys::AMediaCodec_delete(codec) };
            return Err(BackendError::ConfigError(
                format!("AMediaCodec_configure failed with status={status:?}")
            ));
        }

        let status = unsafe { ndk_sys::AMediaCodec_start(codec) };
        if status != ndk_sys::media_status_t(0) {
            unsafe { ndk_sys::AMediaCodec_delete(codec) };
            return Err(BackendError::ConfigError(
                format!("AMediaCodec_start failed with status={status:?}")
            ));
        }

        self.state = Some(CodecState { codec, native_window });
        log::info!("[MediaCodec] Initialized: {width}x{height} HEVC decoder");
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JNI EXPORTS
// ─────────────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_streamreceiver_NativeLib_initBackend(
    env:     *mut jni::sys::JNIEnv, // ПРОСТО СЫРОЙ УКАЗАТЕЛЬ! Никаких мучений с jni-rs
    _class:  JClass,
    surface: JObject,
    width:   jint,
    height:  jint,
) {
    let _ = std::panic::catch_unwind(|| {
        log::info!("[JNI] initBackend called");
        let native_window = unsafe {
            ndk_sys::ANativeWindow_fromSurface(env as *mut _, surface.as_raw() as *mut _)
        };
        log::info!("[JNI] native_window is_null: {}", native_window.is_null());
        if native_window.is_null() { return; }
        unsafe { ndk_sys::ANativeWindow_acquire(native_window); }
        
        let worker_exists = WORKER_TX.lock().map(|g| g.is_some()).unwrap_or(false);
        log::info!("[JNI] WORKER_TX exists: {}", worker_exists);
        
        send_to_worker(VideoWorkerMsg::InitSurface { window: native_window, width, height });
        log::info!("[JNI] InitSurface sent");
    });
}

#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
pub extern "system" fn Java_com_example_streamreceiver_NativeLib_startNetworking<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass,
    host: JString,
    port: jint,
) {
    let host_str = unowned_env
        .with_env(|env: &mut Env| env.get_string(&host).map(String::from))
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>();

    std::thread::spawn(move || {
        // Останавливаем предыдущий runtime если он ещё жив
        stop_networking_inner();
        
        let addr_str = format!("{}:{}", host_str, port);
        let addr = match addr_str.to_socket_addrs() {
            Ok(mut iter) => match iter.next() {
                Some(addr) => addr,
                None => {
                    log::error!("[Network] Не найден IP для {}", addr_str);
                    return;
                }
            },
            Err(e) => {
                log::error!("[Network] Ошибка резолва {}: {}", addr_str, e);
                return;
            }
        };

        let (trace_tx, trace_rx) = watch::channel(None);
        let _ = TRACE_TX.set(trace_tx);
        
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let handle = rt.handle().clone();
 
        // Сохраняем runtime ПЕРЕД block_on — чтобы stopNetworking мог его убить
        *NETWORK_RT.lock().unwrap() = Some(rt);
 
        log::info!("[Network] Подключаемся к {}", addr);
 
        // catch_unwind: shutdown_background() прерывает block_on паникой
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.block_on(async move {
                // run_quic_receiver — бесконечный reconnect-loop
                // frame_tx = None: кадры рендерятся прямо в Surface
                if let Err(e) = crate::network::run_quic_receiver(addr, None, trace_rx, None).await {
                    log::error!("[Network] Fatal: {}", e);
                }
            });
        }));
 
        // Чистим статик после завершения (штатного или через stop)
        *NETWORK_RT.lock().unwrap() = None;
        log::info!("[Network] Networking thread exited");
    });
}
 
/// Остановить QUIC-клиент и освободить tokio runtime.
/// Вызывается из Kotlin при нажатии "Отключить" или из surfaceDestroyed.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_streamreceiver_NativeLib_stopNetworking(
    _env:   *mut jni::sys::JNIEnv,
    _class: JClass,
) {
    let _ = std::panic::catch_unwind(|| {
        stop_networking_inner();
        log::info!("[JNI] stopNetworking OK");
    });
}


#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_streamreceiver_NativeLib_shutdownBackend(
    _env:   *mut jni::sys::JNIEnv, // Тоже сырой указатель для простоты
    _class: JClass,
) {
    let _ = std::panic::catch_unwind(|| {
        send_to_worker(VideoWorkerMsg::Shutdown);
        log::info!("[JNI] Shutdown command sent to worker");
    });
}

fn stop_networking_inner() {
    if let Some(rt) = NETWORK_RT.lock().unwrap().take() {
        rt.shutdown_background(); // не блокирует вызывающий поток
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_streamreceiver_NativeLib_getLatencyStats<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass,
) -> jstring {
    let latency = LATEST_LATENCY.lock().unwrap().clone();

    unowned_env
        .with_env(|env: &mut Env| {
            env.new_string(latency)
                .or_else(|_| env.new_string(""))
                .map(|s| s.into_raw())
                
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_streamreceiver_NativeLib_onVsync(
    _env: *mut jni::sys::JNIEnv,
    _class: JClass,
    _frame_time_ns: jlong,
) {
    // [п.3] Теперь мы не лочим BACKEND вообще!
    // VSYNC просто забирает данные, если их подготовил poll_output
    if let Ok(mut pending_guard) = PENDING_VSYNC_INFO.lock() {
        if let Some((frame_id, mut trace)) = pending_guard.take() {
            let offset = common::CLOCK_OFFSET.load(Ordering::Relaxed);
            let present_us = FrameTrace::now_us();
            let capture_local_us = (trace.capture_us as i64).saturating_sub(offset);
            
            let diff_ms = (present_us as i64 - capture_local_us) as f64 / 1000.0;
            
            trace.present_us = present_us;
            
            // Отправляем трейс в логгер/сервер
            if let Some(tx) = TRACE_TX.get() {
                let _ = tx.send(Some((frame_id, trace)));
            }

            // Обновляем строку статистики
            if let Ok(mut lat) = LATEST_LATENCY.lock() {
                *lat = format!("{:.1} ms", diff_ms);
            }
        }
    }
}