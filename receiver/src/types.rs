#[cfg(unix)]
use std::os::fd::OwnedFd;

use common::FrameTrace;

use crate::backend::{YuvFrame};

pub enum DecodedFrame {
    Yuv(YuvFrame),
    DmaBuf(DmaBufFrame),
}

/// Декодированный кадр как DMA-BUF дескриптор.
///
/// Содержит dup-нутый fd DMA-BUF, который Vulkan импортирует как внешнюю
/// память. CPU пиксельные данные не касается вообще.
///
/// `fd` закрывается автоматически при Drop.
/// 
#[cfg(unix)]
#[derive(Debug)]
pub struct DmaBufFrame {
    pub frame_id:   u64,
    pub trace:      FrameTrace,
    pub width:      u32,
    pub height:     u32,
    /// Dup-нутый DMA-BUF fd; единственный владелец.
    /// Vulkan импортирует его через `vkImportMemoryFdKHR` (Vulkan «съедает» fd).
    pub fd:         OwnedFd,
    /// DRM format modifier (тайлинг). Линейный = 0, тайловый != 0.
    pub modifier:   u64,
    /// Полный размер DMA-BUF объекта в байтах (из AVDRMObjectDescriptor.size).
    pub total_size: usize,
    // ── Плоскость Y (luma) ───────────────────────────────────────────────────
    pub y_offset: u32,
    pub y_pitch:  u32,
    // ── Плоскость UV (chroma, interleaved NV12) ──────────────────────────────
    pub uv_offset: u32,
    pub uv_pitch:  u32,
}