// src/backend/android.rs
//
// Android-бекенд: аппаратный HEVC-декодер через NDK AMediaCodec.
#[link(name = "mediandk")]
unsafe extern "C" {}

#[link(name = "android")]
unsafe extern "C" {}

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
use once_cell::sync::OnceCell;

use super::{BackendError, FrameOutput, VideoBackend};
use common::FrameTrace;
// ─────────────────────────────────────────────────────────────────────────────
// Глобальный синглтон бекенда
// ─────────────────────────────────────────────────────────────────────────────
static BACKEND: OnceCell<Arc<Mutex<AndroidMediaCodecBackend>>> = OnceCell::new();
static NETWORK_RT: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);
static LATEST_LATENCY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn get_or_create_backend() -> Arc<Mutex<AndroidMediaCodecBackend>> {
    BACKEND
        .get_or_init(|| Arc::new(Mutex::new(AndroidMediaCodecBackend::new())))
        .clone()
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
    frame_pts_us: u64,
    traces:       VecDeque<FrameTrace>,
}

impl AndroidMediaCodecBackend {
    const MAX_PENDING_TRACES: usize = 64;
    pub fn new() -> Self {
        Self {
            state:        None,
            frame_pts_us: 0,
            traces:       VecDeque::new(),
        }
    }

    pub fn on_vsync(&mut self, _frame_time_ns: u64) {
        if let Some(trace) = self.traces.pop_front() {
            let offset = crate::network::CLOCK_OFFSET.load(Ordering::Relaxed);
            
            // В локальное время телефона (i64, так как offset может быть отрицательным)
            let local_capture_us = (trace.capture_us as i64).saturating_sub(offset);
            let present_us = FrameTrace::now_us();
            
            let diff_us = present_us.saturating_sub(local_capture_us as u64);

            // Статический счетчик для логов, чтобы не захламлять консоль
            static mut LOG_THROTTLE: u64 = 0;
            unsafe {
                LOG_THROTTLE += 1;
                if LOG_THROTTLE % 60 == 0 {
                    log::info!(
                        "[LatencyDebug] Present: {} | LocalCap: {} | Offset: {} | Diff: {}us",
                        present_us, local_capture_us, offset, diff_us
                    );
                }
            }

            let total_ms = diff_us as f64 / 1000.0;

            if let Ok(mut lat) = LATEST_LATENCY.lock() {
                // Если видишь 0.0, значит diff_us после saturating_sub стал нулем
                *lat = format!("{:.1} ms", total_ms);
            }
        }
    }

    pub unsafe fn init_with_surface(
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
        fmt_i32!("operating-rate",    60);

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
// Реализация трейта VideoBackend
// ─────────────────────────────────────────────────────────────────────────────

impl VideoBackend for AndroidMediaCodecBackend {
    fn push_encoded(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>) -> Result<(), BackendError> {
        let state = self.state.as_ref().ok_or(BackendError::NotInitialized)?;

        unsafe {
            let idx = ndk_sys::AMediaCodec_dequeueInputBuffer(state.codec, 2_000);
            if idx < 0 { return Ok(()); } 

            let mut buf_size: usize = 0;
            let buf_ptr = ndk_sys::AMediaCodec_getInputBuffer(state.codec, idx as usize, &mut buf_size);
            let copy_len = payload.len().min(buf_size);
            ptr::copy_nonoverlapping(payload.as_ptr(), buf_ptr, copy_len);

            // КЛАДЕМ трейс в очередь ПЕРЕД очередью декодера
            if let Some(t) = trace {
                // Если очередь слишком разрослась (например, кадры не выходят), чистим старое
                if self.traces.len() > 64 { self.traces.pop_front(); }
                self.traces.push_back(t);
            }

            self.frame_pts_us = frame_id.saturating_mul(16_667);
            ndk_sys::AMediaCodec_queueInputBuffer(state.codec, idx as usize, 0, copy_len, self.frame_pts_us, 0);
        }
        Ok(())
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        let state = self.state.as_ref().ok_or(BackendError::NotInitialized)?;

        unsafe {
            let mut info = ndk_sys::AMediaCodecBufferInfo { offset: 0, size: 0, presentationTimeUs: 0, flags: 0 };
            // Используем таймаут 0, чтобы не блокировать сетевой поток
            let idx = ndk_sys::AMediaCodec_dequeueOutputBuffer(state.codec, &mut info, 0);

            if idx >= 0 {
                // КРИТИЧЕСКИЙ МОМЕНТ: Выводим кадр на экран. 
                // true означает "отрендерить на Surface немедленно"
                ndk_sys::AMediaCodec_releaseOutputBuffer(state.codec, idx as usize, true);
                
                // Мы НЕ удаляем из очереди traces здесь! 
                // Мы удалим его в on_vsync, когда сработает событие обновления экрана.
                Ok(FrameOutput::DirectToSurface)
            } else {
                Ok(FrameOutput::Pending)
            }
        }
    }

    fn shutdown(&mut self) {
        self.state = None;
        self.traces.clear();
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
        let native_window = unsafe {
            // Передаем сырой C-указатель напрямую в NDK
            ndk_sys::ANativeWindow_fromSurface(
                env as *mut _, 
                surface.as_raw() as *mut _,
            )
        };

        if native_window.is_null() {
            log::error!("[JNI] ANativeWindow_fromSurface returned null!");
            return;
        }

        let backend = get_or_create_backend();
        let mut guard = backend.lock().unwrap();

        match unsafe { guard.init_with_surface(native_window, width, height) } {
            Ok(())   => log::info!("[JNI] initBackend OK"),
            Err(e)   => log::error!("[JNI] initBackend failed: {e}"),
        }
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

    let backend = get_or_create_backend();
 
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
                if let Err(e) = crate::network::run_quic_receiver(backend, addr, None).await {
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
        if let Some(backend) = BACKEND.get() {
            backend.lock().unwrap().shutdown();
            log::info!("[JNI] shutdownBackend OK");
        }
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
    frame_time_ns: jlong,
) {
    if frame_time_ns <= 0 {
        return;
    }

    if let Some(backend) = BACKEND.get() {
        if let Ok(mut guard) = backend.lock() {
            guard.on_vsync(frame_time_ns as u64);
        }
    }
}