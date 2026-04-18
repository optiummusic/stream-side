//! Pluggable screen-capture + HEVC-encode abstraction for the sender side.
//!
//! This mirrors the [`VideoBackend`] pattern used on the receiver:
//!
//! | Platform | Type                   | Capture API              | Encoder      |
//! |----------|------------------------|--------------------------|--------------|
//! | Linux    | [`LinuxPipeWireSender`] | XDG Portal + PipeWire   | VAAPI HEVC   |
//! | Windows  | `WindowsSender` (TODO) | WGC / DXGI               | NVENC / QSV  |
//! | Android  | `AndroidSender` (TODO) | MediaProjection          | MediaCodec   |
//!
//! # Usage
//!
//! ```rust,ignore
//! let (server, sink) = QuicServer::new(addr).await;
//! let sender = LinuxPipeWireSender::new(1920, 1080);
//! tokio::select! {
//!     _ = tokio::signal::ctrl_c() => {},
//!     res = sender.run(sink)      => { res.expect("sender error"); },
//! }
//! ```
//!
//! [`VideoBackend`]: crate::backend::VideoBackend (receiver crate)

use std::{fmt, future::Future};
use tokio::sync::mpsc;

use crate::encode::EncodedFrame;

#[cfg(target_os = "linux")]

pub mod linux;

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
///
/// Implementors capture frames from a platform-specific source, encode them
/// to HEVC NAL units, and deliver each encoded packet through `sink` as
/// `(nal_bytes, is_keyframe)`.
///
/// # Contract
///
/// - `run` drives the capture loop and does not return until either a fatal
///   error occurs or the returned future is dropped (cancellation).
/// - Each call to `sink.send(...)` delivers one complete HEVC access unit
///   (may be an IDR / key-frame or a P-frame).
/// - The sink is a [`tokio::sync::mpsc::Sender`]; if `try_send` would block,
///   the implementation **must drop** the frame (no unbounded buffering).
///
/// # Thread safety
/// `Send + 'static` — implementors may spawn OS threads internally but must
/// not hold non-`Send` state across the async boundary.
pub trait VideoSender: Send + 'static {
    /// The concrete error type produced by this sender.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Start capturing and encoding.
    ///
    /// Drives the pipeline until cancelled or a fatal error occurs.
    /// Each encoded HEVC NAL packet is delivered via `sink`.
    fn run(
        self,
        sink: mpsc::Sender<EncodedFrame>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
