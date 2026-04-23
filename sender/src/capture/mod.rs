//! Pluggable screen-capture + HEVC-encode abstraction for the sender side.
//!
//! | Platform | Type               | Capture API                          | Encoder      |
//! |----------|--------------------|--------------------------------------|--------------|
//! | Linux    | [`LinuxSender`]    | wlroots screencopy **или** PipeWire  | VAAPI HEVC   |
//! | Windows  | `WindowsSender`    | WGC / DXGI                           | NVENC / QSV  |
//! | Android  | `AndroidSender`    | MediaProjection                      | MediaCodec   |
//!
//! ## Автоматический выбор бэкенда (Linux)
//!
//! [`LinuxSender`] пробует wlroots при старте: делает один Wayland-roundtrip
//! и проверяет наличие `zwlr_screencopy_manager_v1`. Если глобал найден →
//! `WlrootsSender`; если нет (X11, GNOME, не-wlroots Wayland) → `LinuxPipeWireSender`.
//!
//! ```text
//! LinuxSender::new()
//!     │
//!     ├─ wlroots::is_available() == true
//!     │       └─► WlrootsSender  (zwlr_screencopy → SHM → VAAPI HEVC)
//!     │
//!     └─ иначе
//!             └─► LinuxPipeWireSender (XDG Portal → PipeWire → VAAPI HEVC)
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! let sender = LinuxSender::new(1920, 1080, idr_rx);
//! tokio::select! {
//!     _ = tokio::signal::ctrl_c() => {},
//!     res = sender.run(sink)      => { res.expect("sender error"); },
//! }
//! ```

use std::{fmt, future::Future};
use common::AudioFrame;
use tokio::sync::mpsc;

#[cfg(target_os = "linux")]
use crate::Watchers;
use crate::encode::EncodedFrame;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "linux")]
pub mod linux_audio;

#[cfg(target_os = "linux")]
pub mod wlroots;

//
// Fps limiter

pub struct FpsLimiter {
    next_allowed: std::time::Instant,
    last_fps: Option<u32>,
}

impl FpsLimiter {
    pub fn new() -> Self {
        Self {
            next_allowed: std::time::Instant::now(),
            last_fps: None,
        }
    }

    #[inline]
    pub fn should_allow(
        &mut self,
        fps: Option<u32>,
    ) -> bool {
        let now = std::time::Instant::now();

        if fps != self.last_fps {
            self.last_fps = fps;
            self.next_allowed = now;
        }

        let Some(fps) = fps.filter(|&v| v > 0) else {
            return true;
        };

        let interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);

        if now < self.next_allowed {
            return false;
        }

        self.next_allowed = now + interval;
        true
    }
}

pub(crate) struct FrameGate {
    limiter: FpsLimiter,
    fps_rx: tokio::sync::watch::Receiver<Option<u32>>,
}

impl FrameGate {
    pub fn new(fps_rx: tokio::sync::watch::Receiver<Option<u32>>) -> Self {
        Self {
            limiter: FpsLimiter::new(),
            fps_rx,
        }
    }

    #[inline]
    pub fn allow(&mut self) -> bool {
        self.limiter.should_allow(*self.fps_rx.borrow())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SenderError {
    /// Portal / capture-API initialisation failed.
    CaptureInit(String),
    /// A fatal error occurred mid-stream (stream died, device lost, etc.).
    CaptureRuntime(String),
    /// Encoder failed to initialise or encountered a fatal codec error.
    EncodeInit(String),
    /// The encoded-frame channel was closed before the sender finished.
    SinkClosed,
}

impl fmt::Display for SenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaptureInit(m)    => write!(f, "Capture init error: {m}"),
            Self::CaptureRuntime(m) => write!(f, "Capture runtime error: {m}"),
            Self::EncodeInit(m)     => write!(f, "Encode init error: {m}"),
            Self::SinkClosed        => write!(f, "Encoded-frame sink was closed"),
        }
    }
}

impl std::error::Error for SenderError {}

// ─────────────────────────────────────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────────────────────────────────────

/// A pluggable screen-capture + HEVC-encode pipeline.
pub trait VideoSender: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn run(
        self,
        sink: mpsc::Sender<EncodedFrame>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

// ─────────────────────────────────────────────────────────────────────────────
// LinuxSender — автоматический выбор бэкенда
// ─────────────────────────────────────────────────────────────────────────────

/// Linux screen-capture sender с автоматическим выбором бэкенда.
///
/// При создании выполняет один Wayland-roundtrip чтобы определить,
/// доступен ли `zwlr_screencopy_manager_v1`. Далее делегирует в
/// [`wlroots::WlrootsSender`] или [`linux::LinuxPipeWireSender`].
#[cfg(target_os = "linux")]
pub struct LinuxSender {
    inner: LinuxSenderInner,
}

#[cfg(target_os = "linux")]
enum LinuxSenderInner {
    Wlroots(wlroots::WlrootsSender),
    PipeWire(linux::LinuxPipeWireSender),
}

#[cfg(target_os = "linux")]
impl LinuxSender {
    /// Создать sender. Автоматически выбирает wlroots или PipeWire.
    pub fn new(width: u32, height: u32, watchers: Watchers) -> Self {
        if wlroots::is_available() {
            log::info!("[LinuxSender] zwlr_screencopy_manager_v1 найден → WlrootsSender");
            Self {
                inner: LinuxSenderInner::Wlroots(
                    wlroots::WlrootsSender::new(width, height, watchers),
                ),
            }
        } else {
            log::info!("[LinuxSender] wlroots недоступен → LinuxPipeWireSender (XDG Portal)");
            Self {
                inner: LinuxSenderInner::PipeWire(
                    linux::LinuxPipeWireSender::new(width, height, watchers),
                ),
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl VideoSender for LinuxSender {
    type Error = SenderError;

    async fn run(self, sink: mpsc::Sender<EncodedFrame>) -> Result<(), SenderError> {
        match self.inner {
            LinuxSenderInner::Wlroots(s)  => s.run(sink).await,
            LinuxSenderInner::PipeWire(s) => s.run(sink).await,
        }
    }
}

pub trait AudioSender: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn run(
        self,
        sink: mpsc::Sender<AudioFrame>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub struct LinuxAudioSender;

impl LinuxAudioSender {
    pub fn new() -> Self {
        Self
    }
}