//! Sender crate — screen capture, HEVC encoding, and QUIC transport.

/// Pluggable screen-capture + encode pipeline.
pub mod capture;

/// VAAPI HEVC encoder (Linux only).
pub mod encode;


pub mod network;
use std::{collections::{HashMap, VecDeque}, sync::{Arc, atomic::AtomicBool}, time::Duration};

use bytes::Bytes;
use common::{AudioFrame, ChunkMeta, DatagramChunk, NackEntry};
use tokio::sync::{RwLock};

#[derive(Debug, Clone, Default)]
pub struct ClientIdentity {
    model: Option<String>,
    os: Option<String>,
    ready: bool,
}

#[derive(Clone)]
pub struct Watchers {
    pub idr_rx: tokio::sync::watch::Receiver<bool>, 
    pub bitrate_rx: tokio::sync::watch::Receiver<u64>, 
    pub capture_fps_rx: tokio::sync::watch::Receiver<Option<u32>>,
}

#[derive(Clone)]
pub struct Senders {
    pub idr_tx: tokio::sync::watch::Sender<bool>, 
    pub bitrate_tx: tokio::sync::watch::Sender<u64>, 
    pub capture_fps_tx: tokio::sync::watch::Sender<Option<u32>>,
    pub audio_bcast_tx: tokio::sync::broadcast::Sender<Arc<AudioFrame>>,
}

#[derive(Default)]
pub struct ConnectionInfo {
    remote: String,
    label: RwLock<String>,
    ready: AtomicBool,
}

impl ConnectionInfo {
    async fn label(&self) -> String {
        let label = self.label.read().await.clone();
        if label.is_empty() {
            self.remote.clone()
        } else {
            label
        }
    }
}

pub struct SerializedFrame {
    pub frame_id: u64,
    pub is_key: bool,
    pub datagrams: Vec<Bytes>,
}

/// Token-bucket pacer — spreads the datagram chunks of a frame evenly over
/// time instead of blasting them all in a single tight loop.
///
/// # Why this reduces jitter
///
/// Without pacing, all N chunks of a frame hit the kernel socket buffer in
/// microseconds.  The NIC/switch drains that burst at line rate, but the
/// resulting queue-depth spike adds variable latency (jitter) to *later*
/// packets.  Spreading chunks at a controlled byte-rate prevents that spike.
///
/// # Target rate
///
/// Default: 100 Mbit/s.  At that rate a 1 200-byte chunk is released every
/// ~96 µs; a 20-chunk frame (~24 KB) takes ≈ 2 ms — comfortably within a
/// 16 ms frame budget at 60 fps.  Raising the rate (e.g. to 500 Mbit/s for
/// Gigabit LAN) reduces the pacing delay while still smoothing micro-bursts.
pub struct FramePacer {
    /// Accumulated token credit, in bytes.
    tokens: f64,
    /// Fill rate: bytes per microsecond.
    rate_bytes_per_us: f64,
    /// Wall-clock instant of the last token refill.
    last_refill: tokio::time::Instant,
    /// Maximum burst the pacer will absorb (tokens are capped here).
    burst_cap: f64,
}
 
impl FramePacer {
    /// Create a pacer targeting `rate_mbps` Mbit/s.
    ///
    /// `burst_cap_ms` is the maximum token accumulation during idle periods.
    /// 4 ms is a good default: it allows the very first frame to be sent
    /// without delay while still preventing sustained bursts.
    pub fn new(rate_mbps: f64, burst_cap_ms: f64) -> Self {
        let rate_bytes_per_us = rate_mbps * 1e6 / 8.0 / 1e6; // Mbit/s → bytes/µs
        let burst_cap = rate_bytes_per_us * burst_cap_ms * 1_000.0;
        Self {
            tokens: burst_cap, // start full so the first frame is not delayed
            rate_bytes_per_us,
            last_refill: tokio::time::Instant::now(),
            burst_cap,
        }
    }
 
    /// Consume `bytes` tokens and return how long the caller must wait before
    /// sending.  Returns `Duration::ZERO` when there are enough tokens.
    pub fn consume(&mut self, bytes: usize) -> Duration {
        let now = tokio::time::Instant::now();
        let elapsed_us = (now - self.last_refill).as_micros() as f64;
        self.tokens = (self.tokens + elapsed_us * self.rate_bytes_per_us).min(self.burst_cap);
        self.last_refill = now;
 
        let need = bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            Duration::ZERO
        } else {
            let deficit = need - self.tokens;
            self.tokens = 0.0;
            Duration::from_micros((deficit / self.rate_bytes_per_us) as u64)
        }
    }
}

 
pub const HDR_SLICE_IDX:    usize = 8;
pub const HDR_GROUP_IDX:    usize = 17;
pub const HDR_SHARD_IDX:    usize = 10;
pub const HEADER_LEN:       usize = 19;
 
// ── Cache parameters ──────────────────────────────────────────────────────────
 
/// How many distinct frame IDs to keep in the retransmit cache.
const SHARD_CACHE_FRAMES: usize = 256;
 
// ── ShardCache ────────────────────────────────────────────────────────────────
//
// Stores raw datagram bytes keyed by (frame_id → (slice_idx, group_idx) → shards)
// so the sender can retransmit exactly the missing shards of a specific FEC group.
 
pub struct ShardCache {
    inner: std::sync::RwLock<CacheInner>,
}
 
struct CacheInner {
    /// Insertion-order tracking for LRU eviction.
    order: VecDeque<u64>,
    /// frame_id → (slice_idx, group_idx) → raw datagram bytes
    data: HashMap<u64, HashMap<(u8, u8), Vec<Bytes>>>,
}
 
impl ShardCache {
    pub fn new() -> Self {
        Self {
            inner: std::sync::RwLock::new(CacheInner {
                order: VecDeque::with_capacity(SHARD_CACHE_FRAMES + 1),
                data: HashMap::new(),
            }),
        }
    }
 
    /// Insert a batch of raw datagram bytes for a frame.
    ///
    /// Each datagram is parsed for `slice_idx` and `group_idx` using the
    /// fixed wire-format offsets; no full deserialisation is required.
    pub fn insert(&self, frame_id: u64, datagrams: &[Bytes]) {
        let mut inner = self.inner.write().unwrap();
 
        let is_new_frame = !inner.data.contains_key(&frame_id);
        if is_new_frame {
            if inner.order.len() >= SHARD_CACHE_FRAMES {
                if let Some(old_id) = inner.order.pop_front() {
                    inner.data.remove(&old_id);
                }
            }
            inner.order.push_back(frame_id);
        }
 
        let frame_map = inner.data.entry(frame_id).or_default();
 
        for dgram in datagrams {
            if dgram.len() < HEADER_LEN {
                continue;
            }
            let slice_idx = dgram[HDR_SLICE_IDX];
            let group_idx = dgram[HDR_GROUP_IDX];
            frame_map
                .entry((slice_idx, group_idx))
                .or_default()
                .push(dgram.clone());
        }
    }
 
    /// Return the subset of shards for `(frame_id, slice_idx, group_idx)` that
    /// the receiver has **not** yet received, as indicated by `received_mask`
    /// (bit `i` set → shard `i` already arrived at the receiver).
    ///
    /// Returns an empty vec on cache miss (frame too old or never cached).
    pub fn retransmit(
        &self,
        frame_id: u64,
        slice_idx: u8,
        group_idx: u8,
        received_mask: u64,
    ) -> Vec<Bytes> {
        let inner = self.inner.read().unwrap();
        let mut out = Vec::new();
 
        let Some(frame_map) = inner.data.get(&frame_id) else {
            return out;
        };
        let Some(shards) = frame_map.get(&(slice_idx, group_idx)) else {
            return out;
        };
 
        for dgram in shards {
            if dgram.len() < HEADER_LEN {
                continue;
            }
            let shard_idx = dgram[HDR_SHARD_IDX];
            // Only retransmit shards the receiver has not acknowledged.
            if (received_mask >> shard_idx) & 1 == 0 {
                out.push(dgram.clone());
            }
        }
 
        out
    }

    pub fn retransmit_batch(
        &self,
        frame_id: u64,
        entries: &[NackEntry],
    ) -> Vec<Bytes> {
        let inner = self.inner.read().unwrap();
        let mut out = Vec::new();

        // 1. Ищем кадр в кеше (один раз на весь батч)
        let Some(frame_map) = inner.data.get(&frame_id) else {
            // Если кадра нет (уже вытеснен или еще не дошел), возвращаем пустой список
            return out;
        };

        // 2. Проходим по всем записям в батче NACK
        for entry in entries {
            // 3. Достаем список шардов для конкретной пары (slice, group)
            if let Some(shards) = frame_map.get(&(entry.slice_idx, entry.group_idx)) {
                for dgram in shards {
                    if dgram.len() < HEADER_LEN {
                        continue;
                    }

                    // 4. Проверяем маску. Шард-индекс берем из сырых байт заголовка.
                    let shard_idx = dgram[HDR_SHARD_IDX];
                    
                    // Если в маске бит i == 0, значит чанк потерян — добавляем в список на переотправку
                    if (entry.received_mask >> shard_idx) & 1 == 0 {
                        out.push(dgram.clone());
                    }
                }
            }
        }

        out
    }
}