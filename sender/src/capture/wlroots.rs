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
//! | **DMA-BUF** | v2+, протокол linux-dmabuf | нет | GBM → VAAPI импорт (DRM_PRIME) |
//!
//! # Формат пикселей
//!
//! wlroots обычно отдаёт `XRGB8888` или `ARGB8888`. На little-endian обе
//! раскладки в памяти выглядят как `[B G R X/A]` — идентично тому, что
//! PipeWire называет `SPA_VIDEO_FORMAT_BGRA`. Поэтому буфер передаётся
//! в `encode_bgra` без дополнительной конвертации.
//!
//! # DMA-BUF (zero-copy)
//!
//! Реализованный путь через GBM:
//! ```text
//! 1. Registry: bind zwp_linux_dmabuf_v1 (v3+) + получить /dev/dri/renderD128
//! 2. Event::LinuxDmabuf { format, modifier, width, height } → DmaBufInfo
//! 3. gbm::Device::new(drm_fd) → gbm::BufferObject (fourcc + modifier)
//! 4. bo.fd() → DMA-BUF fd (OwnedFd)
//! 5. zwp_linux_buffer_params_v1::add + create_immed → wl_buffer
//! 6. frame.copy(&dmabuf_wl_buffer)     // compositor рендерит прямо в GPU-память
//! 7. AnyEncoder::encode_dmabuf(fd)     // VAAPI импортирует DRM_PRIME напрямую
//! ```
//! Требует крейты `gbm` + `drm` и фичу `dmabuf` в Cargo.toml.
//! При невозможности (нет /dev/dri, нет zwp_linux_dmabuf_v1, GBM-ошибка)
//! автоматически откатывается на SHM-путь.

use std::{
    os::{fd::{BorrowedFd, OwnedFd, FromRawFd, RawFd}, unix::io::AsRawFd},
    time::{Duration, Instant},
};
use common::FrameTrace;
use wayland_client::WEnum;
use libc::c_void;
use tokio::sync::{mpsc, watch};
use wayland_client::{
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
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
unsafe impl Sync for ShmBuf {}
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
                libc::PROT_READ | libc::PROT_WRITE,
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

// ─── DMA-BUF поддержка ───────────────────────────────────────────────────────

/// Параметры DMA-BUF буфера, полученные от compositor через Event::LinuxDmabuf.
#[derive(Clone, Copy, Debug)]
struct DmaBufInfo {
    width:    u32,
    height:   u32,
    format:   u32,   // DRM fourcc
    modifier: u64,   // DRM format modifier
}

/// Аллоцированный GBM-буфер с соответствующим wl_buffer.
struct DmaBuf {
    /// GBM buffer object — держим живым пока compositor пишет в него.
    bo:       gbm::BufferObject<()>,
    /// wl_buffer для передачи в frame.copy().
    buffer:   wl_buffer::WlBuffer,
    /// DMA-BUF file descriptor (dup'd, не заимствованный).
    fd:       OwnedFd,
    width:    u32,
    height:   u32,
    format:   u32,
    modifier: u64,
    /// Stride plane 0, байт (от GBM, нужен энкодеру).
    stride:   u32,
    /// Offset plane 0 (всегда 0 для одноплоскостных форматов).
    offset:   u32,
}

// SAFETY: bo и wl_buffer живут на одном capture-потоке.
unsafe impl Send for DmaBuf {}
unsafe impl Sync for DmaBuf {}

impl DmaBuf {
    /// Аллоцирует GBM BufferObject и создаёт wl_buffer через zwp_linux_dmabuf_v1.
    fn alloc(
        gbm:    &gbm::Device<std::fs::File>,
        dmabuf: &ZwpLinuxDmabufV1,
        qh:     &QueueHandle<AppState>,
        info:   DmaBufInfo,
    ) -> Result<Self, SenderError> {
        use gbm::BufferObjectFlags;

        // Запрашиваем scanout+rendering чтобы compositor мог писать напрямую.
        let bo = gbm.create_buffer_object_with_modifiers2::<()>(
            info.width,
            info.height,
            gbm::Format::try_from(info.format)
                .map_err(|_| SenderError::CaptureInit(
                    format!("неизвестный GBM fourcc: 0x{:08x}", info.format)
                ))?,
            std::iter::once(gbm::Modifier::from(info.modifier)),
            BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
        ).map_err(|e| SenderError::CaptureInit(format!("GBM alloc: {e}")))?;

        // Получаем DMA-BUF fd; dup чтобы bo и fd были независимы.
        let raw_fd = bo.fd()
            .map_err(|e| SenderError::CaptureInit(format!("GBM bo.fd(): {e}")))?;
        let fd = unsafe {
            OwnedFd::from_raw_fd(
                libc::dup(raw_fd.as_raw_fd())
            )
        };
        if fd.as_raw_fd() < 0 {
            return Err(SenderError::CaptureInit("dup(bo.fd()) failed".into()));
        }

        // Создаём wl_buffer через zwp_linux_buffer_params_v1.
        let stride = bo.stride();
        const OFFSET: u32 = 0; // plane 0, одноплоскостный формат

        let params = dmabuf.create_params(qh, ());
        params.add(
            unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) },
            0,       // plane index
            OFFSET,
            stride,
            (info.modifier >> 32) as u32,
            (info.modifier & 0xFFFF_FFFF) as u32,
        );
        // create_immed: synchronous — wl_buffer готов немедленно.
        let buffer = params.create_immed(
            info.width as i32,
            info.height as i32,
            info.format,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (),
        );

        Ok(Self {
            bo,
            buffer,
            fd,
            width:    info.width,
            height:   info.height,
            format:   info.format,
            modifier: info.modifier,
            stride,
            offset:   OFFSET,
        })
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// Пробует открыть первое доступное DRM render-устройство.
fn open_drm_device() -> Option<std::fs::File> {
    for i in 0..8 {
        let path = format!("/dev/dri/renderD{}", 128 + i);
        if let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            log::info!("[wlroots] DRM device: {path}");
            return Some(f);
        }
    }
    None
}

// ─── Состояние приложения ────────────────────────────────────────────────────

struct RawFrame {
    /// SHM-путь: пиксели в CPU-памяти.
    shm_buffer: Option<std::sync::Arc<ShmBuf>>,
    /// DMA-BUF путь: пиксели в GPU-памяти; fd для VAAPI-импорта.
    dma_buf:    Option<std::sync::Arc<DmaBuf>>,
    info:      BufInfo,
    dma_info:  Option<DmaBufInfo>,
    y_invert:  bool,
    ts:        u64,
}

struct AppState {
    // Глобальные объекты Wayland
    screencopy_mgr:     Option<ZwlrScreencopyManagerV1>,
    screencopy_version: u32,
    output:             Option<wl_output::WlOutput>,
    shm:                Option<wl_shm::WlShm>,
    /// zwp_linux_dmabuf_v1 — доступен только если compositor его экспортирует.
    linux_dmabuf:       Option<ZwpLinuxDmabufV1>,
    /// GBM device — инициализируется лениво при первом DMA-BUF кадре.
    gbm:                Option<gbm::Device<std::fs::File>>,

    dmabuf_modifiers:   std::collections::HashMap<u32, u64>,

    // Активный frame handle — держим живым до события ready/failed.
    current_frame: Option<ZwlrScreencopyFrameV1>,
    recycle_rx: mpsc::Receiver<std::sync::Arc<ShmBuf>>,
    recycle_dma_rx: mpsc::Receiver<std::sync::Arc<DmaBuf>>,
    // Накапливается из событий текущего кадра
    buf_info:       Option<BufInfo>,
    dma_buf_info:   Option<DmaBufInfo>,
    y_invert:       bool,
    status:         CaptureStatus,
    /// Флаг: текущий кадр закопирован через DMA-BUF (а не SHM).
    copy_used_dma:  bool,

    // Переиспользуемые буферы
    shm_buf_front:  Option<std::sync::Arc<ShmBuf>>,
    shm_buf_back:   Option<std::sync::Arc<ShmBuf>>,
    dma_buf_front:  Option<std::sync::Arc<DmaBuf>>,
    dma_buf_back:   Option<std::sync::Arc<DmaBuf>>,
    dma_pool:       Vec<std::sync::Arc<DmaBuf>>,

    // Энкодер и sink
    encode_tx: mpsc::Sender<RawFrame>,
    sink:    mpsc::Sender<EncodedFrame>,
    idr_rx:  watch::Receiver<bool>,

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
    BufferDone
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
            "zwp_linux_dmabuf_v1" if version >= 3 => {
                let v = version.min(4);
                let dmabuf = registry.bind::<ZwpLinuxDmabufV1, _, _>(name, v, qh, ());
                log::info!("[wlroots] zwp_linux_dmabuf_v1 v{v} — DMA-BUF путь доступен");
                state.linux_dmabuf = Some(dmabuf);
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

// zwp_linux_dmabuf_v1 — получаем доступные форматы (нам нужны только для
// валидации; реальный format+modifier берём из Event::LinuxDmabuf).
impl Dispatch<ZwpLinuxDmabufV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } => {
                let m = ((modifier_hi as u64) << 32) | (modifier_lo as u64);
                log::trace!("[dmabuf] format 0x{format:08x} modifier 0x{m:016x}");
                
                // Сохраняем первый пришедший модификатор или перезаписываем,
                // если пришел DRM_FORMAT_MOD_LINEAR (0)
                let entry = state.dmabuf_modifiers.entry(format).or_insert(m);
                if m == 0 {
                    *entry = 0;
                }
            }
            _ => {}
        }
    }
}

// create_immed завершается синхронно — failed не должен прийти,
// но логируем на случай несовместимого формата.
impl Dispatch<ZwpLinuxBufferParamsV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Failed => {
                log::error!("[dmabuf] params.create_immed failed — откат на SHM");
                // Сбрасываем dma_buf_info чтобы do_copy упал на SHM-путь.
                state.dma_buf_info = None;
                // Инвалидируем GBM чтобы не пытаться снова.
                state.gbm = None;
                state.linux_dmabuf = None;
            }
            zwp_linux_buffer_params_v1::Event::Created { buffer: _ } => {
                // create_immed возвращает буфер синхронно через возврат create_immed(),
                // это событие не используется в нашем flow.
            }
            _ => {}
        }
    }
}

// Release от compositor — игнорируем (буфер мы держим сами).
impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        state: &mut Self,
        _buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {}
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
            Event::Buffer { format, width, height, stride } => {
                let format = match format {
                    WEnum::Value(f) => f,
                    WEnum::Unknown(_) => return,
                };
                if state.buf_info.is_none() {
                    state.buf_info = Some(BufInfo { width, height, stride, format });
                }
                if state.screencopy_version < 2 {
                    do_copy(state, frame, qh); // v1: сразу копируем
                }
            }
            Event::LinuxDmabuf { format, width, height } => {
                // Compositor предлагает DMA-BUF буфер с этим форматом и модификатором.
                // Сохраняем — do_copy использует его в приоритете перед SHM.
                
                let modifier = state.dmabuf_modifiers.get(&format).copied().unwrap_or(0);
                state.dma_buf_info = Some(DmaBufInfo { width, height, format, modifier });
            }
            Event::BufferDone => {
                do_copy(state, frame, qh); // v2+: копируем после BufferDone
            }
            Event::Flags { flags } => {
                use zwlr_screencopy_frame_v1::Flags;
                state.y_invert = match flags {
                    WEnum::Value(f) => f.contains(Flags::YInvert),
                    WEnum::Unknown(_) => false,
                };
            }
            Event::Ready { .. } => {
                state.status = CaptureStatus::Ready;
            }
            Event::Failed => {
                log::warn!("[wlroots] Frame::Failed");
                state.status = CaptureStatus::Failed;
            }
            _ => {}
        }
    }
}

// ─── Вспомогательные функции для обработчика событий ─────────────────────────

/// Аллоцирует (или переиспользует) буфер и вызывает `frame.copy()`.
///
/// Приоритет: DMA-BUF (zero-copy) → SHM (fallback).
/// Вызывается либо из `BufferDone` (v2+), либо из `Buffer` (v1).
fn do_copy(state: &mut AppState, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<AppState>) {
    // Уже копируем — не вызывать copy() второй раз
    if state.status == CaptureStatus::Copying {
        return;
    }

    // ── DMA-BUF путь ────────────────────────────────────────────────────────
    if let (Some(dma_info), Some(dmabuf_proto)) = (state.dma_buf_info, state.linux_dmabuf.as_ref()) {
        // Лениво инициализируем GBM.
        if state.gbm.is_none() {
            match open_drm_device() {
                Some(drm) => {
                    match gbm::Device::new(drm) {
                        Ok(dev) => {
                            log::info!("[wlroots] GBM device инициализирован");
                            state.gbm = Some(dev);
                        }
                        Err(e) => {
                            log::warn!("[wlroots] GBM::new failed: {e} — откат на SHM");
                        }
                    }
                }
                None => {
                    log::warn!("[wlroots] /dev/dri/renderD* не найден — откат на SHM");
                }
            }
        }

        if let Some(gbm) = &state.gbm {
            // Забираем переработанные DMA-BUF буферы.
            while let Ok(returned_buf) = state.recycle_dma_rx.try_recv() {
                state.dma_pool.push(returned_buf);
            }

            let existing_idx = state.dma_pool.iter().position(|b| {
                b.width == dma_info.width && 
                b.height == dma_info.height && 
                b.format == dma_info.format &&
                b.modifier == dma_info.modifier
            });

            if let Some(idx) = existing_idx {
                // Нашли готовый — забираем его
                state.dma_buf_back = Some(state.dma_pool.remove(idx));
            } else if state.dma_pool.len() < 3 { 
                // Если в пуле пусто и буферов всего создано мало (< 3), создаем новый
                match DmaBuf::alloc(gbm, dmabuf_proto, qh, dma_info) {
                    Ok(buf) => {
                        log::info!("[wlroots] Добавлен новый DMA-BUF в пул ({}x{})", dma_info.width, dma_info.height);
                        state.dma_buf_back = Some(std::sync::Arc::new(buf));
                    }
                    Err(e) => { /* откат на SHM */ }
                }
            }

            if let Some(buf) = &state.dma_buf_back {
                frame.copy(&buf.buffer);
                state.status = CaptureStatus::Copying;
                state.copy_used_dma = true;
                return;
            }
        }
    }

    // ── SHM fallback ────────────────────────────────────────────────────────
    let Some(info) = state.buf_info else { return; };
    let Some(shm) = &state.shm else { return; };

    // Пробуем взять переработанный буфер
    while let Ok(buf) = state.recycle_rx.try_recv() {
        if state.shm_buf_back.is_none() {
            state.shm_buf_back = Some(buf);
        }
    }

    let needs_new = state.shm_buf_back.as_ref()
        .map(|b| b.width != info.width || b.height != info.height)
        .unwrap_or(true);

    if needs_new {
        log::info!("Failed to get old buffer!");
        state.shm_buf_back = None;
        match ShmBuf::alloc(shm, qh, info.width, info.height, info.stride, info.format) {
            Ok(buf) => state.shm_buf_back = Some(std::sync::Arc::new(buf)),
            Err(e)  => { state.fatal = Some(e); return; }
        }
    }

    if let Some(buf) = &state.shm_buf_back {
        frame.copy(&buf.buffer);
        state.status = CaptureStatus::Copying;
        state.copy_used_dma = false;
    }
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

    let (enc_tx, enc_rx) = mpsc::channel::<RawFrame>(24);

    let (recycle_tx, mut recycle_rx) = mpsc::channel::<std::sync::Arc<ShmBuf>>(24);
    let (recycle_dma_tx, mut recycle_dma_rx) = mpsc::channel::<std::sync::Arc<DmaBuf>>(24);
    let sink_clone = sink.clone();
    let idr_clone = idr_rx.clone();

    std::thread::Builder::new()
        .name("wlroots-encode".into())
        .spawn(move || run_encode_loop(width, height, sink_clone, idr_clone, enc_rx, recycle_tx, recycle_dma_tx))
        .map_err(|e| SenderError::CaptureInit(format!("encode thread: {e}")))?;

    let mut state = AppState {
        screencopy_mgr:     None,
        screencopy_version: 1,
        output:             None,
        shm:                None,
        linux_dmabuf:       None,
        gbm:                None,
        dmabuf_modifiers:   std::collections::HashMap::new(),
        current_frame:      None,
        recycle_rx,
        recycle_dma_rx,
        buf_info:           None,
        dma_buf_info:       None,
        y_invert:           false,
        status:             CaptureStatus::Idle,
        copy_used_dma:      false,
        shm_buf_front:      None,
        shm_buf_back:       None,
        dma_buf_front:      None,
        dma_buf_back:       None,
        dma_pool:           Vec::new(),
        encode_tx:          enc_tx,
        sink,
        idr_rx,
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
                let used_dma = state.copy_used_dma;

                if used_dma {
                    std::mem::swap(&mut state.dma_buf_front, &mut state.dma_buf_back);
                } else {
                    std::mem::swap(&mut state.shm_buf_front, &mut state.shm_buf_back);
                }

                let saved_info      = state.buf_info;
                let saved_dma_info  = state.dma_buf_info;
                let saved_invert    = state.y_invert;
                state.current_frame = None;
                state.buf_info      = None;
                state.dma_buf_info  = None;
                state.y_invert      = false;
                state.copy_used_dma = false;
                state.status        = CaptureStatus::Idle;

                if used_dma {
                    if let (Some(buf), Some(dma_info)) = (state.dma_buf_front.take(), saved_dma_info) {
                        let info = saved_info.unwrap_or(BufInfo {
                            width:  dma_info.width,
                            height: dma_info.height,
                            stride: dma_info.width * 4,
                            format: wl_shm::Format::Xrgb8888,
                        });
                        let frame = RawFrame {
                            shm_buffer: None,
                            dma_buf:    Some(buf),
                            info,
                            dma_info:   Some(dma_info),
                            y_invert:   saved_invert,
                            ts:         FrameTrace::now_us(),
                        };
                        if let Err(e) = state.encode_tx.try_send(frame) {
                            log::warn!("[wlroots] Encoder queue full (DMA), dropping frame");
                            // Возвращаем буфер для переиспользования.
                            if let Some(dma) = e.into_inner().dma_buf {
                                state.dma_buf_back = Some(dma);
                            }
                        }
                    }
                } else if let (Some(buf), Some(info)) = (state.shm_buf_front.take(), saved_info) {
                    let frame = RawFrame {
                        shm_buffer: Some(buf),
                        dma_buf:    None,
                        info,
                        dma_info:   None,
                        y_invert:   saved_invert,
                        ts:         FrameTrace::now_us(),
                    };
                    if let Err(e) = state.encode_tx.try_send(frame) {
                        log::warn!("[wlroots] Encoder queue full (SHM), dropping frame");
                        if let Some(shm) = e.into_inner().shm_buffer {
                            state.shm_buf_back = Some(shm);
                        }
                    }
                }
                request_next_frame(&mut state, &qh);
            }
 
            CaptureStatus::Failed => {
                log::warn!("[wlroots] Повтор захвата через 16 мс");
                state.current_frame = None;
                state.dma_buf_info  = None;
                state.copy_used_dma = false;
                state.status        = CaptureStatus::Idle;
                std::thread::sleep(Duration::from_millis(16));
                request_next_frame(&mut state, &qh);
            }
            CaptureStatus::Pending => {
            }
            CaptureStatus::Copying => {
            }
            _ => {} // ещё ждём событий
        }
    }
}

fn run_encode_loop(
    width:   u32,
    height:  u32,
    sink:    mpsc::Sender<EncodedFrame>,
    idr_rx:  watch::Receiver<bool>,
    mut rx:  mpsc::Receiver<RawFrame>,
    recycle_tx:     mpsc::Sender<std::sync::Arc<ShmBuf>>,
    recycle_dma_tx: mpsc::Sender<std::sync::Arc<DmaBuf>>,
) {
    let mut encoder: Option<AnyEncoder> = None;
    let mut flip_buf: Vec<u8> = Vec::new();

    let mut fps_n = 0u32;
    let mut fps_t = Instant::now();
    while let Some(frame) = rx.blocking_recv() {
        let enc = encoder.get_or_insert_with(|| {
            log::info!("[wlroots] Initializing encoder {}x{}", frame.info.width, frame.info.height);
            AnyEncoder::detect_and_create(
                frame.info.width,
                frame.info.height,
                sink.clone(),
                idr_rx.clone(),
            )
        });

        let ts = frame.ts;

        if let Some(ref dma) = frame.dma_buf {
            // ── Zero-copy: передаём DMA-BUF fd прямо в VAAPI ──────────────
            log::trace!(
                "[wlroots-encode] DMA-BUF fd={} stride={} offset={} mod=0x{:016x}",
                dma.fd.as_raw_fd(), dma.stride, dma.offset, dma.modifier,
            );
            enc.encode_dmabuf(
                dma.fd.as_raw_fd(),
                dma.stride,
                dma.offset,
                dma.modifier,
                ts,
            );
            let _ = recycle_dma_tx.try_send(frame.dma_buf.unwrap());
        } else if let Some(ref shm_buf) = frame.shm_buffer {
            // ── SHM fallback: CPU copy ─────────────────────────────────────
            let pixels = shm_buf.as_bytes();
            if frame.y_invert {
                flip_rows(pixels, frame.info.height, frame.info.stride, &mut flip_buf);
                enc.encode_bgra(&flip_buf, frame.info.stride, ts);
            } else {
                enc.encode_bgra(pixels, frame.info.stride, ts);
            }
            let _ = recycle_tx.try_send(frame.shm_buffer.unwrap());
        }

        fps_n += 1;
        if fps_t.elapsed() >= Duration::from_secs(1) {
            log::debug!("[wlroots-encode] FPS: {}", fps_n);
            fps_n = 0;
            fps_t = Instant::now();
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

/// Переворачивает строки для флага `Y_INVERT`.
fn flip_rows(data: &[u8], height: u32, stride: u32, out: &mut Vec<u8>) {
    let stride_usize = stride as usize;
    // Растём только при первом вызове или смене разрешения.
    out.resize(data.len(), 0);
    for row in 0..height as usize {
        let src = row * stride_usize;
        let dst = (height as usize - 1 - row) * stride_usize;
        out[dst..dst + stride_usize].copy_from_slice(&data[src..src + stride_usize]);
    }
}