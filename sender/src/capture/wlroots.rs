//! Linux screen-capture via `zwlr_screencopy_manager_v1`.
//!
//! Работает на Sway, Hyprland и любом wlroots-композиторе.
//! Compositor сам пейсит кадры через vblank → 60/120/144 FPS без таймеров.
//!
//! # Пути захвата
//!
//! | Путь     | Условие                     | CPU  | Описание                        |
//! |----------|-----------------------------|------|---------------------------------|
//! | **SHM**  | всегда                      | да   | mmap → encode_bgra              |
//! | **DMA-BUF** | v2+, протокол linux-dmabuf | нет | GBM → VAAPI импорт (см. TODO)  |
//!
//! # Формат пикселей
//!
//! wlroots обычно отдаёт `XRGB8888` или `ARGB8888`. На little-endian обе
//! раскладки в памяти выглядят как `[B G R X/A]` — идентично тому, что
//! PipeWire называет `SPA_VIDEO_FORMAT_BGRA`. Поэтому буфер передаётся
//! в `encode_bgra` без дополнительной конвертации.
//!
//! # DMA-BUF (TODO: zero-copy)
//!
//! Путь к zero-copy через GBM:
//! ```text
//! 1. Получить Event::LinuxDmabuf { format, modifier, width, height }
//! 2. gbm::Device::new(drm_fd) → gbm::BufferObject с тем же fourcc+modifier
//! 3. bo.fd() → DMA-BUF fd
//! 4. zwp_linux_buffer_params_v1::create_immed(fd, format, modifier) → wl_buffer
//! 5. frame.copy(&dmabuf_wl_buffer)       // compositor рендерит в GPU-память
//! 6. AnyEncoder::encode_dmabuf(fd, ...)  // VAAPI импортирует DRM_PRIME напрямую
//! ```
//! Требует крейты `gbm` + `drm` и фичу `dmabuf` в Cargo.toml.

use std::{
    os::{fd::BorrowedFd, unix::io::AsRawFd},
    time::{Duration, Instant},
};
use wayland_client::WEnum;
use libc::c_void;
use tokio::sync::{mpsc, watch};
use wayland_client::{
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::encode::{AnyEncoder, EncodedFrame, HwEncoder};
use super::SenderError;

// ─── Detection ───────────────────────────────────────────────────────────────

/// Возвращает `true`, если compositor экспортирует `zwlr_screencopy_manager_v1`.
///
/// Выполняет один roundtrip к Wayland-серверу и немедленно отключается.
/// Безопасно вызывать из async-контекста (не блокирует event loop).
pub fn is_available() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let display = conn.display();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut state = DetectState::default();
    display.get_registry(&qh, ());
    queue.roundtrip(&mut state).is_ok() && state.found
}

#[derive(Default)]
struct DetectState {
    found: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for DetectState {
    fn event(
        state: &mut Self,
        _reg: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            if interface == "zwlr_screencopy_manager_v1" {
                state.found = true;
            }
        }
    }
}

// ─── Публичный тип ───────────────────────────────────────────────────────────

/// Screen-capture sender на основе `zwlr_screencopy_manager_v1`.
pub struct WlrootsSender {
    width:  u32,
    height: u32,
    idr_rx: watch::Receiver<bool>,
}

impl WlrootsSender {
    pub fn new(width: u32, height: u32, idr_rx: watch::Receiver<bool>) -> Self {
        Self { width, height, idr_rx }
    }
}

impl super::VideoSender for WlrootsSender {
    type Error = SenderError;

    async fn run(self, sink: mpsc::Sender<EncodedFrame>) -> Result<(), SenderError> {
        let (err_tx, err_rx) = tokio::sync::oneshot::channel::<SenderError>();
        let rt = tokio::runtime::Handle::current();

        std::thread::Builder::new()
            .name("wlroots-capture".into())
            .spawn(move || {
                let _g = rt.enter();
                if let Err(e) = run_capture_loop(self.width, self.height, sink, self.idr_rx) {
                    let _ = err_tx.send(e);
                }
            })
            .map_err(|e| SenderError::CaptureInit(format!("thread spawn: {e}")))?;

        match err_rx.await {
            Ok(e) => Err(e),
            Err(_) => Ok(()),
        }
    }
}

// ─── SHM-буфер ───────────────────────────────────────────────────────────────

struct ShmBuf {
    _fd:    memfd::Memfd,
    mmap:   *mut c_void,
    size:   usize,
    buffer: wl_buffer::WlBuffer,
    width:  u32,
    height: u32,
    stride: u32,
}

// SAFETY: mmap и WlBuffer живут на одном потоке; Send нужен для AppState.
unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn alloc(
        shm:    &wl_shm::WlShm,
        qh:     &QueueHandle<AppState>,
        width:  u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<Self, SenderError> {
        let size = (stride * height) as usize;

        let fd = memfd::MemfdOptions::default()
            .allow_sealing(true)
            .create("wlr-screencopy-shm")
            .map_err(|e| SenderError::CaptureInit(format!("memfd create: {e}")))?;
        fd.as_file()
            .set_len(size as u64)
            .map_err(|e| SenderError::CaptureInit(format!("memfd truncate: {e}")))?;

        let mmap = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mmap == libc::MAP_FAILED {
            return Err(SenderError::CaptureInit("mmap failed".into()));
        }

        // wl_shm_pool можно уничтожить сразу после create_buffer —
        // буфер продолжает жить независимо.
        let pool = unsafe {
            shm.create_pool(BorrowedFd::borrow_raw(fd.as_raw_fd()), size as i32, qh, ())
        };
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        pool.destroy();

        Ok(Self { _fd: fd, mmap, size, buffer, width, height, stride })
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.mmap as *const u8, self.size) }
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mmap, self.size); }
        self.buffer.destroy();
    }
}

// ─── Состояние приложения ────────────────────────────────────────────────────

struct AppState {
    // Глобальные объекты Wayland
    screencopy_mgr:     Option<ZwlrScreencopyManagerV1>,
    screencopy_version: u32,
    output:             Option<wl_output::WlOutput>,
    shm:                Option<wl_shm::WlShm>,

    // Активный frame handle — держим живым до события ready/failed.
    current_frame: Option<ZwlrScreencopyFrameV1>,

    // Накапливается из событий текущего кадра
    buf_info:  Option<BufInfo>,
    y_invert:  bool,
    status:    CaptureStatus,

    // Переиспользуемый SHM-буфер (перевыделяется при смене размера)
    shm_buf: Option<ShmBuf>,

    // Энкодер и sink
    encoder: Option<AnyEncoder>,
    sink:    mpsc::Sender<EncodedFrame>,
    idr_rx:  watch::Receiver<bool>,

    // Диагностика
    fps_n: u32,
    fps_t: Instant,

    // Фатальная ошибка из обработчика событий
    fatal: Option<SenderError>,
}

/// Параметры SHM-буфера, полученные от compositor через Event::Buffer.
#[derive(Clone, Copy, Debug)]
struct BufInfo {
    width:  u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum CaptureStatus {
    Idle,
    Pending,  // capture_output вызван, ждём Buffer/LinuxDmabuf
    Copying,  // copy() вызван, ждём Ready/Failed
    Ready,
    Failed,
}

// ─── Dispatch-реализации ─────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "zwlr_screencopy_manager_v1" => {
                let v = version.min(3);
                let mgr = registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, v, qh, ());
                log::info!("[wlroots] zwlr_screencopy_manager_v1 v{v}");
                state.screencopy_version = v;
                state.screencopy_mgr = Some(mgr);
            }
            "wl_output" if state.output.is_none() => {
                // Захватываем первый монитор; для multi-monitor нужен selection UI.
                let out = registry.bind::<wl_output::WlOutput, _, _>(name, 1, qh, ());
                log::debug!("[wlroots] bound first wl_output");
                state.output = Some(out);
            }
            "wl_shm" => {
                let shm = registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ());
                log::debug!("[wlroots] bound wl_shm");
                state.shm = Some(shm);
            }
            _ => {}
        }
    }
}

// Manager не генерирует событий — пустая реализация.
impl Dispatch<ZwlrScreencopyManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// pool.destroy() сразу после create_buffer → событий не будет.
impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// Release от compositor — игнорируем (буфер мы держим сами).
impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;

        match event {
            // ── Compositor объявляет SHM-формат буфера ────────────────────────
            Event::Buffer { format, width, height, stride } => {
                let format = match format {
                    WEnum::Value(f) => f,
                    WEnum::Unknown(_) => {
                        log::warn!("[wlroots] unknown wl_shm format");
                        return;
                    }
                };
                // Compositor может прислать несколько вариантов буфера;
                // берём первый (обычно единственный SHM-формат).
                if state.buf_info.is_none() {
                    state.buf_info = Some(BufInfo { width, height, stride, format });
                }

                // Протокол v1 не имеет события BufferDone —
                // нужно вызвать copy() сразу после Buffer.
                if state.screencopy_version < 2 {
                    do_copy(state, frame, qh);
                }
            }

            // ── DMA-BUF альтернатива (v2+) ───────────────────────────────────
            //
            // Compositor предлагает zero-copy через DMA-BUF.
            // TODO: аллоцировать GBM-буфер, импортировать через VAAPI.
            // Пока падаем back на SHM (установленный выше в Event::Buffer).
            Event::LinuxDmabuf { format, width, height } => {
                log::debug!(
                    "[wlroots] DMA-BUF: fourcc={:#010x} {width}×{height} (fallback to SHM)",
                    format,
                );
            }

            // ── Все типы буферов объявлены (v2+) — выбираем и копируем ───────
            Event::BufferDone => {
                do_copy(state, frame, qh);
            }

            // ── Опциональные флаги (напр. Y-инверсия) ────────────────────────
            Event::Flags { flags } => {
                use zwlr_screencopy_frame_v1::Flags;
                state.y_invert = match flags {
                    WEnum::Value(f) => f.contains(Flags::YInvert),
                    WEnum::Unknown(_) => false,
                };
            }

            // ── Кадр готов ───────────────────────────────────────────────────
            Event::Ready { .. } => {
                state.status = CaptureStatus::Ready;
            }

            // ── Compositor не смог захватить кадр ────────────────────────────
            Event::Failed => {
                log::warn!("[wlroots] Frame::Failed — compositor отклонил захват");
                state.status = CaptureStatus::Failed;
            }

            _ => {}
        }
    }
}

// ─── Вспомогательные функции для обработчика событий ─────────────────────────

/// Аллоцирует (или переиспользует) SHM-буфер и вызывает `frame.copy()`.
/// Вызывается либо из `BufferDone` (v2+), либо из `Buffer` (v1).
fn do_copy(state: &mut AppState, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<AppState>) {
    let Some(info) = state.buf_info else {
        state.fatal = Some(SenderError::CaptureInit(
            "copy triggered before any Buffer event".into(),
        ));
        return;
    };
    let Some(shm) = &state.shm else {
        state.fatal = Some(SenderError::CaptureInit("wl_shm недоступен".into()));
        return;
    };

    // Переаллоцируем только при изменении размера / страйда.
    let needs_new = state
        .shm_buf
        .as_ref()
        .map(|b| b.width != info.width || b.height != info.height || b.stride != info.stride)
        .unwrap_or(true);

    if needs_new {
        match ShmBuf::alloc(shm, qh, info.width, info.height, info.stride, info.format) {
            Ok(buf) => {
                log::info!("[wlroots] ShmBuf {}×{} stride={}", info.width, info.height, info.stride);
                state.shm_buf = Some(buf);
            }
            Err(e) => {
                state.fatal = Some(e);
                return;
            }
        }
    }

    let buf = state.shm_buf.as_ref().unwrap();
    frame.copy(&buf.buffer);
    state.status = CaptureStatus::Copying;
}

// ─── Основной цикл захвата ───────────────────────────────────────────────────

fn run_capture_loop(
    width:  u32,
    height: u32,
    sink:   mpsc::Sender<EncodedFrame>,
    idr_rx: watch::Receiver<bool>,
) -> Result<(), SenderError> {
    let conn = Connection::connect_to_env()
        .map_err(|e| SenderError::CaptureInit(format!("Wayland connect: {e}")))?;

    let display = conn.display();
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();

    let mut state = AppState {
        screencopy_mgr:     None,
        screencopy_version: 1,
        output:             None,
        shm:                None,
        current_frame:      None,
        buf_info:           None,
        y_invert:           false,
        status:             CaptureStatus::Idle,
        shm_buf:            None,
        encoder:            None,
        sink,
        idr_rx,
        fps_n:              0,
        fps_t:              Instant::now(),
        fatal:              None,
    };

    // Один roundtrip для получения глобальных объектов
    display.get_registry(&qh, ());
    queue
        .roundtrip(&mut state)
        .map_err(|e| SenderError::CaptureInit(format!("roundtrip: {e}")))?;

    if state.screencopy_mgr.is_none() {
        return Err(SenderError::CaptureInit(
            "zwlr_screencopy_manager_v1 не найден — не wlroots-compositor".into(),
        ));
    }
    if state.output.is_none() {
        return Err(SenderError::CaptureInit("wl_output не найден".into()));
    }
    if state.shm.is_none() {
        return Err(SenderError::CaptureInit("wl_shm не найден".into()));
    }

    log::info!("[wlroots] Старт screencopy-петли (protocol v{})", state.screencopy_version);

    // Запускаем первый кадр
    request_next_frame(&mut state, &qh);

    // Главный event loop — blocking_dispatch блокируется до прихода событий,
    // благодаря чему мы получаем frame-pacing от compositor (vblank).
    loop {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| SenderError::CaptureRuntime(format!("dispatch: {e}")))?;

        if let Some(e) = state.fatal.take() {
            return Err(e);
        }

        match state.status {
            CaptureStatus::Ready => {
                encode_current_frame(&mut state);
                // Уничтожаем frame-объект (посылает destroy) и сразу просим следующий
                state.current_frame = None;
                state.buf_info      = None;
                state.y_invert      = false;
                request_next_frame(&mut state, &qh);
            }

            CaptureStatus::Failed => {
                log::warn!("[wlroots] Повтор захвата через 16 мс");
                state.current_frame = None;
                state.status        = CaptureStatus::Idle;
                // Минимальный backoff чтобы не спинить при недоступном захвате
                std::thread::sleep(Duration::from_millis(16));
                request_next_frame(&mut state, &qh);
            }

            _ => {} // ещё ждём событий
        }
    }
}

fn request_next_frame(state: &mut AppState, qh: &QueueHandle<AppState>) {
    let mgr    = state.screencopy_mgr.as_ref().unwrap();
    let output = state.output.as_ref().unwrap();
    // overlay_cursor=0: не включать курсор в захват
    let frame = mgr.capture_output(0, output, qh, ());
    state.current_frame = Some(frame);
    state.status        = CaptureStatus::Pending;
}

fn encode_current_frame(state: &mut AppState) {
    use common::FrameTrace;

    let (Some(buf), Some(info)) = (&state.shm_buf, state.buf_info) else {
        return;
    };

    let pixels = buf.as_bytes();

    // Y-инверсия: wlroots её обычно не выставляет, но на всякий случай
    let flipped: Vec<u8>;
    let src: &[u8] = if state.y_invert {
        flipped = flip_rows(pixels, info.height, info.stride);
        &flipped
    } else {
        pixels
    };

    let enc = state.encoder.get_or_insert_with(|| {
        log::info!("[wlroots] Init encoder {}×{}", info.width, info.height);
        AnyEncoder::detect_and_create(
            info.width,
            info.height,
            state.sink.clone(),
            state.idr_rx.clone(),
        )
    });

    let ts = FrameTrace::now_us();
    // XRGB8888 / ARGB8888 на LE → байты [B G R X/A] = идентично BGRA для FFmpeg
    enc.encode_bgra(src, info.stride, ts);

    state.fps_n += 1;
    if state.fps_t.elapsed() >= Duration::from_secs(1) {
        log::debug!("[wlroots] FPS: {}", state.fps_n);
        state.fps_n = 0;
        state.fps_t = Instant::now();
    }
}

/// Переворачивает строки для флага `Y_INVERT`.
fn flip_rows(data: &[u8], height: u32, stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = vec![0u8; data.len()];
    for row in 0..height as usize {
        let src = row * stride;
        let dst = (height as usize - 1 - row) * stride;
        out[dst..dst + stride].copy_from_slice(&data[src..src + stride]);
    }
    out
}