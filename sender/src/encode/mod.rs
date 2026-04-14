// encode/mod.rs

use std::os::unix::io::RawFd;
use bytes::Bytes;
use common::{FrameTrace, GpuVendor, detect_gpu_vendor};
use tokio::sync::{mpsc, watch};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod vaapi;
pub mod nvenc;

pub use vaapi::VaapiEncoder;
pub use nvenc::NvencEncoder;

use std::{ffi::c_int, ptr, slice};
use libc::{mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ};
use pipewire::spa::sys as spa_sys;
use pipewire::sys as pw_sys;
// ─── Тип кадра (общий для обоих бэкендов) ────────────────────────────────────

pub struct EncodedFrame {
    pub frame_id: u64,
    pub slices: Vec<Bytes>,
    pub is_key:  bool,
    pub trace:   Option<FrameTrace>,
}

pub enum FrameData {
    Bgra(Vec<u8>),
    DmaBuf { fd: RawFd, stride: u32, offset: u32, modifier: u64 },
}

// ─── Публичный трейт ──────────────────────────────────────────────────────────

/// Аппаратный HEVC-энкодер. Реализуется отдельно для каждого вендора.
pub trait HwEncoder: Send + 'static {
    fn encode_bgra(&self, frame: &[u8], stride: u32, capture_us: u64);

    fn encode_dmabuf(
        &self,
        fd:        RawFd,
        stride:    u32,
        offset:    u32,
        modifier:  u64,
        capture_us: u64,
    );

    #[inline]
    fn encode(&self, frame: &[u8], stride: u32, capture_us: u64) {
        self.encode_bgra(frame, stride, capture_us);
    }
}

pub enum AnyEncoder {
    Vaapi(VaapiEncoder),
    Nvenc(NvencEncoder),
}

impl AnyEncoder {
    pub fn detect_and_create(
        width:  u32,
        height: u32,
        sink:   mpsc::Sender<EncodedFrame>,
        idr_rx: watch::Receiver<bool>,
        bitrate: Arc<AtomicU64>,
    ) -> Self {
        match detect_gpu_vendor() {
            GpuVendor::Nvidia => {
                log::info!("[Encoder] NVIDIA GPU detected → hevc_nvenc");
                Self::Nvenc(NvencEncoder::new(width, height, sink, idr_rx, bitrate))
            }
            v => {
                log::info!("[Encoder] Vendor {v:?} → hevc_vaapi (VAAPI)");
                Self::Vaapi(VaapiEncoder::new(width, height, sink, idr_rx, bitrate))
            }
        }
    }
}

impl HwEncoder for AnyEncoder {
    fn encode_bgra(&self, frame: &[u8], stride: u32, capture_us: u64) {
        match self {
            Self::Vaapi(e) => e.encode_bgra(frame, stride, capture_us),
            Self::Nvenc(e) => e.encode_bgra(frame, stride, capture_us),
        }
    }

    fn encode_dmabuf(
        &self, fd: RawFd, stride: u32, offset: u32, modifier: u64, capture_us: u64,
    ) {
        match self {
            Self::Vaapi(e) => e.encode_dmabuf(fd, stride, offset, modifier, capture_us),
            Self::Nvenc(e) => e.encode_dmabuf(fd, stride, offset, modifier, capture_us),
        }
    }
}

/// Извлекает пиксельный срез из PipeWire-буфера и вызывает `f` с ним.
///
/// Обрабатывает `SPA_DATA_MemPtr` и `SPA_DATA_MemFd`.
/// `SPA_DATA_DmaBuf` **не обрабатывается** — для него используй
/// [`Encoder::encode_dmabuf`] напрямую.
///
/// # Safety
/// `buffer` должен быть валидным `pw_buffer`, полученным от
/// `stream.dequeue_raw_buffer()` и ещё не возвращённым.
pub unsafe fn process_frame_from_pw_buffer<F>(buffer: *mut pw_sys::pw_buffer, mut f: F)
where
    F: FnMut(&[u8]),
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
            spa_sys::SPA_DATA_MemFd => {
                let map_len = data.maxsize as usize;
                let mapped  = mmap(
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
            // DmaBuf обрабатывается выше, в process-коллбэке linux.rs.
            _ => {}
        }
    }
}