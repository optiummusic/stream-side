// encode/nvenc.rs
//
// hevc_nvenc через ffmpeg-next.
// CPU-путь:  BGRA → SwsScale → NV12 (CPU) → CUDA upload → nvenc
// DMA-BUF:   fd → av_hwframe_map(CUDA←DRM_PRIME) → nvenc  (TODO: требует nvdec/cuda iop)

use std::ffi::c_int;
use std::os::unix::io::RawFd;
use std::sync::mpsc::{self, SyncSender};
use std::{thread};
use tokio::sync::{mpsc as async_mpsc, watch};

use super::{EncodedFrame, FrameData, HwEncoder};

pub struct NvencEncoder {
    tx:      SyncSender<(FrameData, u64)>,
    _worker: thread::JoinHandle<()>,
    free_rx: mpsc::Receiver<Vec<u8>>,
}

impl NvencEncoder {
    pub fn new(
        width:  u32,
        height: u32,
        sink:   async_mpsc::Sender<EncodedFrame>,
        idr_rx: watch::Receiver<bool>,
    ) -> Self {
        let (tx, rx)             = mpsc::sync_channel::<(FrameData, u64)>(4);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (free_tx, free_rx)   = mpsc::channel::<Vec<u8>>();

        let buf_size = (width * height * 4) as usize;
        free_tx.send(vec![0u8; buf_size]).unwrap();
        free_tx.send(vec![0u8; buf_size]).unwrap();

        ffmpeg_next::init().unwrap();

        let worker = thread::Builder::new()
            .name("nvenc-encoder".into())
            .spawn(move || {
                run_nvenc_loop(/*width, height, rx, free_tx, ready_tx, sink, idr_rx*/);
            })
            .expect("failed to spawn nvenc thread");

        ready_rx.recv().expect("nvenc init signal");
        Self { tx, _worker: worker, free_rx }
    }
}

impl HwEncoder for NvencEncoder {
    fn encode_bgra(&self, frame: &[u8], capture_us: u64) {
        if let Ok(mut buf) = self.free_rx.try_recv() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            let _ = self.tx.try_send((FrameData::Bgra(buf), capture_us));
        }
    }

    fn encode_dmabuf(
        &self, fd: RawFd, stride: u32, offset: u32, modifier: u64, capture_us: u64,
    ) {
        // TODO: realize DMA-BUF through av_hwdevice_ctx_create(CUDA) +
        //       av_hwframe_map когда будет доступен nvidia-drm.
        log::debug!("[NvencEncoder] DMA-BUF not supported, fallback to mmap");
        unsafe {
            use libc::{mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ};
            let size = (stride * /* height */ 0) as usize;
            if size == 0 { libc::close(fd); return; }
            let ptr = mmap(std::ptr::null_mut(), size, PROT_READ, MAP_SHARED, fd, offset as _);
            if ptr != MAP_FAILED {
                let slice = std::slice::from_raw_parts(ptr as *const u8, size);
                self.encode_bgra(slice, capture_us);
                munmap(ptr, size);
            }
            libc::close(fd);
        }
    }
}

fn run_nvenc_loop(/* ... */) {
    todo!("nvenc loop — similar to vaapi")
}