// src/lib.rs
//
// Корень крейта. Экспортирует все модули.
// На Android собирается как cdylib (JNI-библиотека).
// На десктопе используется как rlib бинарником main.rs.

use std::{cmp::Ordering, collections::{BinaryHeap, HashMap}, time::Duration};

use common::{FrameTrace, VideoPacket};

pub mod backend;
pub mod network;
pub mod types;

// Android-инициализация логгера выполняется при загрузке библиотеки
#[cfg(target_os = "android")]
#[allow(non_snake_case)]
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(
    _vm: jni::JavaVM,
    _: *mut std::ffi::c_void,
) -> jni::sys::jint {
    // Перенаправляем log::* в Android logcat с тегом "StreamReceiver"
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("StreamReceiver"),
    );
    log::info!("Rust library loaded (JNI_OnLoad)");
    jni::sys::JNI_VERSION_1_6
}

pub const JITTER_TARGET_MS: u64 = 48;
 
/// Maximum number of frames held simultaneously.  When exceeded the oldest
/// frame is evicted (dropped) to bound memory use.
pub const JITTER_MAX_FRAMES: usize = 16;
 
// ─────────────────────────────────────────────────────────────────────────────
// Jitter buffer
// ─────────────────────────────────────────────────────────────────────────────
 
/// Scheduling entry stored in the min-heap.
pub struct JitterEntry {
    /// Absolute µs timestamp at which this frame may be forwarded.
    release_us: u64,
    /// Identifies the corresponding entry in `JitterBuffer::packets`.
    frame_id: u64,
}

// Manual Ord impl so BinaryHeap becomes a min-heap on `release_us`.
impl PartialEq  for JitterEntry { fn eq(&self, o: &Self) -> bool { self.release_us == o.release_us && self.frame_id == o.frame_id } }
impl Eq         for JitterEntry {}
impl PartialOrd for JitterEntry { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) } }
impl Ord        for JitterEntry {
    fn cmp(&self, o: &Self) -> Ordering {
        // Reverse: smaller release_us → higher priority (min-heap).
        o.release_us.cmp(&self.release_us)
            .then(o.frame_id.cmp(&self.frame_id))
    }
}
 
/// Smooths inter-frame arrival variance by imposing a fixed playout delay.
///
/// Frames are inserted with `push` as soon as they are fully reassembled.
/// The caller must periodically call `drain_ready` (driven by a timer) to
/// retrieve frames whose deadline has passed.
pub struct JitterBuffer {
    /// Min-heap of scheduled release times.
    heap: BinaryHeap<JitterEntry>,
    /// Actual packet storage; heap entries with no matching key here are stale.
    packets: HashMap<u64, VideoPacket>,
    /// Fixed delay added to every frame's arrival time.
    target_us: u64,
}
 
impl JitterBuffer {
    pub fn new(target_ms: u64) -> Self {
        Self {
            heap:      BinaryHeap::new(),
            packets:   HashMap::new(),
            target_us: target_ms * 1_000,
        }
    }
 
    /// Insert a newly-reassembled frame.
    ///
    /// If the buffer is at capacity (`JITTER_MAX_FRAMES`), the frame with the
    /// smallest `frame_id` is evicted — it is the most stale and least likely
    /// to contribute to smooth playback.
    pub fn push(&mut self, packet: VideoPacket) {
        let frame_id   = packet.frame_id;
        let release_us = FrameTrace::now_us() + self.target_us;
 
        self.packets.insert(frame_id, packet);
        self.heap.push(JitterEntry { release_us, frame_id });
 
        // Evict oldest if over capacity.
        while self.packets.len() > JITTER_MAX_FRAMES {
            if let Some(min_id) = self.packets.keys().copied().min() {
                self.packets.remove(&min_id);
                log::warn!("[JitterBuf] evicted frame #{min_id} (buffer full)");
                // The corresponding heap entry becomes an orphan and will be
                // silently skipped by drain_ready / time_to_next.
            } else {
                break;
            }
        }
    }
 
    /// Pop and return all frames whose deadline has passed, in ascending
    /// `frame_id` order (i.e. display order).
    pub fn drain_ready(&mut self) -> Vec<VideoPacket> {
        let now = FrameTrace::now_us();
        let mut ready = Vec::new();
 
        loop {
            match self.heap.peek() {
                None                                   => break,
                Some(e) if e.release_us > now          => break,
                Some(e) => {
                    let fid = e.frame_id;
                    self.heap.pop();
                    if let Some(pkt) = self.packets.remove(&fid) {
                        ready.push(pkt);
                    }
                    // else: orphaned entry (evicted earlier) — skip.
                }
            }
        }
 
        // Sort for in-order delivery to the decoder.
        ready.sort_unstable_by_key(|p| p.frame_id);
        ready
    }
 
    /// Duration until the next frame is due, suitable for a `tokio::time::sleep`.
    ///
    /// Returns `None` when the buffer is empty.
    /// Cleans orphaned heap entries as a side effect.
    pub fn time_to_next(&mut self) -> Option<Duration> {
        loop {
            match self.heap.peek() {
                None => return None,
                Some(e) if self.packets.contains_key(&e.frame_id) => {
                    let now = FrameTrace::now_us();
                    return Some(if now >= e.release_us {
                        Duration::ZERO
                    } else {
                        Duration::from_micros(e.release_us - now)
                    });
                }
                Some(_) => { self.heap.pop(); } // discard orphan, continue
            }
        }
    }
 
    pub fn is_empty(&self) -> bool { self.packets.is_empty() }
}