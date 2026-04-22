
use crate::fec::decode::FecDecoder;

use super::*;

// ═══════════════════════════════════════════════════════════════════════════════
// DecodeTask / DecodeResult
// ═══════════════════════════════════════════════════════════════════════════════

/// Everything a rayon worker needs to reconstruct one FEC group.
pub(crate) struct DecodeTask {
    pub(crate) frame_id:     u64,
    pub(crate) slice_idx:    u8,
    pub(crate) group_idx:    u8,
    pub(crate) k:            u8,
    pub(crate) m:            u8,
    /// Shard snapshot taken at submission time.
    pub(crate) shards:       Vec<Option<Bytes>>,
    pub(crate) payload_lens: Vec<u16>,
}

/// Result placed back into the result queue by the worker.
pub(crate) struct DecodeResult {
    pub(crate) frame_id:  u64,
    pub(crate) slice_idx: u8,
    /// `None` — RS reconstruction failed (too many erasures).
    pub(crate) data:      Option<Vec<u8>>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// ComputePool
// ═══════════════════════════════════════════════════════════════════════════════
//
// Thin wrapper around `rayon::spawn` + a shared result queue.
// `RS_CACHE` in `decode.rs` is `thread_local!` — every rayon thread owns its
// own `ReedSolomon` instance, so there is no lock contention on the hot path.

pub struct ComputePool {
    results: Arc<Mutex<Vec<DecodeResult>>>,
}

impl ComputePool {
    pub fn new() -> Self {
        Self { results: Arc::new(Mutex::new(Vec::new())) }
    }

    pub(crate) fn submit(&self, task: DecodeTask) {
        let sink = Arc::clone(&self.results);
        rayon::spawn(move || {
            let data = FecDecoder::decode(task.k, task.m, &task.shards, &task.payload_lens);
            let result = DecodeResult {
                frame_id:  task.frame_id,
                slice_idx: task.slice_idx,
                data,
            };
            if let Ok(mut guard) = sink.lock() {
                guard.push(result);
            }
        });
    }

    /// Non-blocking drain — returns everything the pool has finished so far.
    pub(crate) fn drain_results(&self) -> Vec<DecodeResult> {
        self.results
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }
}