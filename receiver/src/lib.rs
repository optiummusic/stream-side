// src/lib.rs
//
// Корень крейта. Экспортирует все модули.
// На Android собирается как cdylib (JNI-библиотека).
// На десктопе используется как rlib бинарником main.rs.

use std::{cmp::Ordering, collections::{BinaryHeap, HashMap}, time::Duration};

use common::{AudioFrame, FrameTrace, VideoPacket};

pub mod backend;
pub mod network;
pub mod types;
pub mod platform;

pub(crate) enum VideoWorkerMsg {
    Push(VideoPacket),
    PollDecoder,
    Shutdown,

    #[cfg(target_os = "android")]
    InitSurface {
        window: *mut ndk_sys::ANativeWindow,
        width: i32,
        height: i32,
    },
}

unsafe impl Send for VideoWorkerMsg {}

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

pub const JITTER_TARGET_MS: u64 = 10;
 
/// Maximum number of frames held simultaneously.  When exceeded the oldest
/// frame is evicted (dropped) to bound memory use.
pub const JITTER_MAX_FRAMES: usize = 64;
 
 pub enum UserEvent {
    NewFrame,
}

pub struct AvJitterBuffer {
    video: JitterBuffer,
    audio: AudioJitterBuffer,
    target_us: u64,
    anchor:   Option<(u64, u64)>,
}

impl AvJitterBuffer {
    pub fn new(target_ms: u64) -> Self {
        Self {
            video: JitterBuffer::new(target_ms),
            audio: AudioJitterBuffer::new(target_ms),
            target_us: target_ms * 1_000,
            anchor:   None,
        }
    }

    pub fn drain_ready_with(&mut self, now_us: u64) -> (Vec<VideoPacket>, Vec<AudioFrame>) {
        (self.video.drain_ready_with(now_us), self.audio.drain_ready_with(now_us))
    }

    pub fn time_to_next_us(&self) -> Option<u64> {
        let v = self.video.next_release_us();
        let a = self.audio.next_release_us();
        match (v, a) {
            (Some(v), Some(a)) => Some(v.min(a)),
            (Some(v), None)    => Some(v),
            (None, Some(a))    => Some(a),
            (None, None)       => None,
        }
    }

    fn to_local_release(&mut self, capture_us: u64, now_us: u64) -> u64 {
        let (anchor_sender, anchor_local) = self.anchor
            .get_or_insert_with(|| (capture_us, now_us));
        
        let delta = capture_us.saturating_sub(*anchor_sender);
        anchor_local.saturating_add(delta) + self.target_us
    }

    pub fn push_video(&mut self, packet: VideoPacket, now_us: u64) {
        let capture_us = packet.trace
            .as_ref()
            .map(|t| t.capture_us)
            .unwrap_or(now_us);
        let release_us = self.to_local_release(capture_us, now_us);
        self.video.push_with_release(packet, release_us);
    }

    pub fn push_audio(&mut self, frame: AudioFrame, now_us: u64) {
        let release_us = self.to_local_release(frame.capture_us, now_us);
        self.audio.push_with_release(frame, release_us);
    }

    pub fn time_to_next(&mut self) -> Option<Duration> {
        // минимум из двух буферов
        let v = self.video.time_to_next();
        let a = self.audio.time_to_next();
        match (v, a) {
            (Some(v), Some(a)) => Some(v.min(a)),
            (Some(v), None)    => Some(v),
            (None,    Some(a)) => Some(a),
            (None,    None)    => None,
        }
    }

    pub fn drain_ready(&mut self) -> (Vec<VideoPacket>, Vec<AudioFrame>) {
        (self.video.drain_ready(), self.audio.drain_ready())
    }
}

pub struct AudioJitterBuffer {
    heap:    BinaryHeap<AudioEntry>,
    packets: HashMap<u64, AudioFrame>,  // seq → frame
    target_us: u64,
    seq: u64,
}

impl AudioJitterBuffer {
    pub fn new(target_ms: u64) -> Self {
        Self {
            heap: BinaryHeap::new(),
            packets: HashMap::new(),
            target_us: target_ms * 1_000,
            seq: 0,
        }
    }

    pub fn drain_ready_with(&mut self, now_us: u64) -> Vec<AudioFrame> {
        let mut ready = Vec::new();
        loop {
            match self.heap.peek() {
                None => break,
                Some(e) if e.release_us > now_us => break,
                Some(e) => {
                    let seq = e.seq;
                    self.heap.pop();
                    if let Some(f) = self.packets.remove(&seq) {
                        ready.push(f);
                    }
                }
            }
        }
        ready
    }

    pub fn push_with_release(&mut self, frame: AudioFrame, release_us: u64) {
        self.packets.insert(self.seq, frame);
        self.heap.push(AudioEntry { release_us, seq: self.seq });
        self.seq += 1;
    }

    pub fn drain_ready(&mut self) -> Vec<AudioFrame> {
        let now = FrameTrace::now_us();
        let mut ready = Vec::new();
        loop {
            match self.heap.peek() {
                None => break,
                Some(e) if e.release_us > now => break,
                Some(e) => {
                    let seq = e.seq;
                    self.heap.pop();
                    if let Some(f) = self.packets.remove(&seq) {
                        ready.push(f);
                    }
                }
            }
        }
        ready
    }

    pub fn time_to_next(&mut self) -> Option<Duration> {
        let now = FrameTrace::now_us();
        loop {
            match self.heap.peek() {
                None => return None,
                Some(e) if self.packets.contains_key(&e.seq) => {
                    return Some(if now >= e.release_us {
                        Duration::ZERO
                    } else {
                        Duration::from_micros(e.release_us - now)
                    });
                }
                Some(_) => { self.heap.pop(); }
            }
        }
    }

    pub fn next_release_us(&self) -> Option<u64> {
        self.heap.peek().map(|e| e.release_us)
    }
}
// ─────────────────────────────────────────────────────────────────────────────
// Jitter buffer
// ─────────────────────────────────────────────────────────────────────────────
 
/// Scheduling entry stored in the min-heap.
pub struct JitterEntry {
    pub release_us: u64,
    pub frame_id: u64,
    pub slice_idx: u8,
}

struct AudioEntry {
    release_us: u64,
    seq:        u64,
}

impl PartialEq for AudioEntry {
    fn eq(&self, o: &Self) -> bool { self.release_us == o.release_us && self.seq == o.seq }
}
impl Eq for AudioEntry {}
impl PartialOrd for AudioEntry {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) }
}
impl Ord for AudioEntry {
    fn cmp(&self, o: &Self) -> Ordering {
        o.release_us.cmp(&self.release_us).then(o.seq.cmp(&self.seq))
    }
}

// Manual Ord impl so BinaryHeap becomes a min-heap on `release_us`.
impl PartialEq for JitterEntry {
    fn eq(&self, o: &Self) -> bool {
        self.release_us == o.release_us
            && self.frame_id == o.frame_id
            && self.slice_idx == o.slice_idx
    }
}

impl Eq for JitterEntry {}

impl PartialOrd for JitterEntry {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for JitterEntry {
    fn cmp(&self, o: &Self) -> Ordering {
        // min-heap by release time
        o.release_us
            .cmp(&self.release_us)
            .then(o.frame_id.cmp(&self.frame_id))
            .then(o.slice_idx.cmp(&self.slice_idx))
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
    packets: HashMap<(u64, u8), VideoPacket>,
    /// Fixed delay added to every frame's arrival time.
    target_us: u64,
    last_scheduled_us: u64,
    inter_packet_gap_us: u64,
}
 
impl JitterBuffer {
    pub fn new(target_ms: u64) -> Self {
        Self {
            heap:      BinaryHeap::new(),
            packets:   HashMap::new(),
            target_us: target_ms * 1_000,
            last_scheduled_us: 0,
            inter_packet_gap_us: 50,
        }
    }

    pub fn drain_ready_with(&mut self, now_us: u64) -> Vec<VideoPacket> {
        let mut ready = Vec::new();
        loop {
            match self.heap.peek() {
                None => break,
                Some(e) if e.release_us > now_us => break,
                Some(e) => {
                    let key = (e.frame_id, e.slice_idx);
                    self.heap.pop();
                    if let Some(pkt) = self.packets.remove(&key) {
                        ready.push(pkt);
                    }
                }
            }
        }
        ready.sort_unstable_by_key(|p| (p.frame_id, p.slice_idx));
        ready
    }

    pub fn next_release_us(&self) -> Option<u64> {
        self.heap.peek().map(|e| e.release_us)
    }
 
    /// Insert a newly-reassembled frame.
    ///
    /// If the buffer is at capacity (`JITTER_MAX_FRAMES`), the frame with the
    /// smallest `frame_id` is evicted — it is the most stale and least likely
    /// to contribute to smooth playback.
    pub fn push_with_release(&mut self, packet: VideoPacket, release_us: u64) {
        let frame_id  = packet.frame_id;
        let slice_idx = packet.slice_idx;

        let release_us = release_us.max(self.last_scheduled_us + self.inter_packet_gap_us);
        self.last_scheduled_us = release_us;

        self.packets.insert((frame_id, slice_idx), packet);
        self.heap.push(JitterEntry { release_us, frame_id, slice_idx });

        while self.packets.len() > JITTER_MAX_FRAMES {
            if let Some((min_key, _)) = self.packets
                .iter()
                .min_by_key(|((fid, _), _)| *fid)
                .map(|(k, v)| (*k, v.clone()))
            {
                self.packets.remove(&min_key);
                log::warn!(
                    "[JitterBuf] evicted frame {} slice {}",
                    min_key.0,
                    min_key.1
                );
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
                None => break,
                Some(e) if e.release_us > now => break,
                Some(e) => {
                    let key = (e.frame_id, e.slice_idx);
                    self.heap.pop();

                    if let Some(pkt) = self.packets.remove(&key) {
                        ready.push(pkt);
                    }
                    // else orphan
                }
            }
        }

        ready.sort_unstable_by_key(|p| (p.frame_id, p.slice_idx));
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
                Some(e) if self.packets.contains_key(&(e.frame_id, e.slice_idx)) => {
                    let now = FrameTrace::now_us();

                    return Some(if now >= e.release_us {
                        Duration::ZERO
                    } else {
                        Duration::from_micros(e.release_us - now)
                    });
                }
                Some(_) => {
                    self.heap.pop(); // orphan
                }
            }
        }
    }
 
    pub fn is_empty(&self) -> bool { self.packets.is_empty() }
}