//! Sender crate — screen capture, HEVC encoding, and QUIC transport.

/// Pluggable screen-capture + encode pipeline.
pub mod capture;

/// VAAPI HEVC encoder (Linux only).
#[cfg(target_os = "linux")]
pub mod encode;

/// QUIC transport server.
pub mod quic;
