//! Sender crate — screen capture, HEVC encoding, and QUIC transport.

/// Pluggable screen-capture + encode pipeline.
pub mod capture;

/// VAAPI HEVC encoder (Linux only).
pub mod encode;


pub mod network;
use std::{collections::HashMap, sync::atomic::AtomicBool, time::Duration};

use bytes::Bytes;
use common::DatagramChunk;
use tokio::sync::{RwLock};

#[derive(Debug, Clone, Default)]
pub struct ClientIdentity {
    model: Option<String>,
    os: Option<String>,
    ready: bool,
}

#[derive(Clone)]
struct SerializedFrame {
    pub frame_id: u64,
    pub is_key: bool,
    pub chunks: Arc<Vec<Bytes>>,
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

#[derive(Clone)]
pub struct SerializedFrame {
    pub frame_id: u64,
    pub is_key: bool,
    /// Pre-serialized, FEC-encoded, ready-to-send QUIC datagrams.
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

// ─────────────────────────────────────────────────────────────────────────────
// Shard cache — retransmission buffer
// ─────────────────────────────────────────────────────────────────────────────
 
/// How many recent frames to keep in the retransmission cache.
/// At 60 fps this is ~500 ms of history — enough to cover any reasonable RTT
/// while not consuming much memory (most frames are a few KB after FEC).
const SHARD_CACHE_FRAMES: usize = 60;
 
/// Ring-buffer cache of recently sent shards, keyed by frame_id.
///
/// Layout: `frame_id → slice_idx → Vec<encoded shard Bytes>` (one entry per
/// shard, already wire-formatted and ready to `send_datagram`).
///
/// When the cache is full (> SHARD_CACHE_FRAMES distinct frame IDs) the oldest
/// entry is evicted, which is correct because those frames have already been
/// decoded or skipped on the receiver.
pub struct ShardCache {
    // Внутреннее состояние теперь защищено
    inner: std::sync::RwLock<CacheInner>,
}

struct CacheInner {
    order: std::collections::VecDeque<u64>,
    data: HashMap<u64, HashMap<u8, Vec<Bytes>>>,
}

impl ShardCache {
    pub fn new() -> Self {
        Self {
            inner: std::sync::RwLock::new(CacheInner {
                order: std::collections::VecDeque::with_capacity(SHARD_CACHE_FRAMES + 1),
                data: HashMap::new(),
            }),
        }
    }

    pub fn insert(&self, frame_id: u64, datagrams: &[Bytes]) {
        let mut inner = self.inner.write().unwrap(); // Эксклюзивный доступ для записи
        
        if inner.order.len() >= SHARD_CACHE_FRAMES {
            if let Some(old_id) = inner.order.pop_front() {
                inner.data.remove(&old_id);
            }
        }

        let slice_map = inner.data.entry(frame_id).or_default();
        for dgram in datagrams {
            if let Some(chunk) = DatagramChunk::decode(dgram.clone()) {
                slice_map.entry(chunk.slice_idx).or_default().push(dgram.clone());
            }
        }
        inner.order.push_back(frame_id);
    }

    pub fn retransmit(&self, frame_id: u64, slice_idx: u8, received_mask: u64) -> Vec<Bytes> {
        let inner = self.inner.read().unwrap(); // Параллельный доступ для чтения
        let mut out = Vec::new();
        
        let Some(slices) = inner.data.get(&frame_id) else { return out; };
        let Some(shards) = slices.get(&slice_idx) else { return out; };

        for dgram in shards {
            if dgram.len() < DatagramChunk::HEADER_LEN { continue; }
            let shard_idx = dgram[10]; 
            if (received_mask >> shard_idx) & 1 == 0 {
                out.push(dgram.clone());
            }
        }
        out
    }
}