use std::cell::RefCell;

use bytes::{Bytes, BytesMut};
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{ChunkMeta, DatagramChunk, EncodedSlice, TYPE_VIDEO};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum data shards per FEC group.
///
/// k=32 gives good burst recovery while keeping RS matrix ops at O(k²)=O(1024).
/// Increasing this beyond 32 raises the parity shard count proportionally,
/// which inflates bandwidth and zombie rate (parity arriving after frame
/// completion) without improving worst-case recovery at typical burst lengths.
const FEC_GROUP_K_MAX: usize = 24;

/// Parity ratio for critical NALUs (SPS / PPS / VPS / IDR).
/// 75 % → for k=32 we send 24 parity shards (total 56).
/// Down from 100 % (k=32 parity): saves 8 shards while keeping >33 % erasure budget.
const FEC_M_CRITICAL_PCT: usize = 20;

/// Parity ratio for the first slice of a non-critical frame (slice_idx == 0).
/// 50 % → for k=32 we send 16 parity shards (total 48).
const FEC_M_FIRST_SLICE_PCT: usize = 10;

/// Parity ratio for regular P/B slices.
///
/// Reduced from 50 % → 25 %.  Rationale:
///   - At 20 Mbps with k=32 (≈1140 B shards), 48 shards per group take ~22 ms
///     to send — longer than one 60 fps frame period (16.7 ms).  Parity shards
///     therefore routinely arrive *after* the frame has been emitted via the
///     all-data fast-path, and get counted as zombies.
///   - 25 % gives 8 parity shards (total 40 per group), reducing transmission
///     time to ~18 ms and cutting zombie rate roughly in half.
///   - On clean links (logs show 0 % loss most of the time) the extra parity
///     was pure waste.  On lossy links the interleaving fix below distributes
///     the remaining parity more evenly across burst windows, making it more
///     effective per shard.
const FEC_M_NORMAL_PCT: usize = 8;

// ── Thread-local RS instance cache ──────────────────────────────────────────

thread_local! {
    static RS_CACHE: RefCell<HashMap<(u8, u8), ReedSolomon>> =
        RefCell::new(HashMap::new());
}

// ── FecEncoder ───────────────────────────────────────────────────────────────

pub struct FecEncoder;

impl FecEncoder {
    pub fn encode(
        frame_id: u64,
        slice_idx: u8,
        total_slices: u8,
        data: &[u8],
        max_chunk_data: usize,
        flags: u8,
    ) -> (EncodedSlice, u8) {
        if data.is_empty() || max_chunk_data == 0 {
            return (EncodedSlice {
                frame_id,
                all_shards_data: Bytes::new(),
                chunks_meta: Vec::new(),
            }, 0);
        }

        let is_critical    = (flags & 2) != 0;
        let is_first_slice = slice_idx == 0;

        let raw_shards: Vec<&[u8]> = data.chunks(max_chunk_data).collect();
        let fec_groups: Vec<&[&[u8]]> = raw_shards.chunks(FEC_GROUP_K_MAX).collect();
        let total_groups = fec_groups.len() as u8;

        // Pre-calculate total buffer size for a single allocation.
        let mut total_buffer_size = 0usize;
        for group in &fec_groups {
            let k = group.len();
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let m = Self::calculate_m(k, is_critical, is_first_slice);
            total_buffer_size += (k + m) * shard_size;
        }

        let mut all_shards_data = BytesMut::with_capacity(total_buffer_size);
        all_shards_data.resize(total_buffer_size, 0);
        // Capacity: k data + m parity per group; 2× is a safe over-estimate.
        let mut chunks_meta = Vec::with_capacity(raw_shards.len() * 2);

        let mut current_offset = 0;
        for (group_idx, group) in fec_groups.iter().enumerate() {
            let k          = group.len();
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let m          = Self::calculate_m(k, is_critical, is_first_slice);
            let total_shards = k + m;
            let group_len = total_shards * shard_size;
            // Flat buffer: [shard_0 | shard_1 | … | shard_{k+m-1}], each of
            // `shard_size` bytes.  RS operates in-place on this layout.
            let group_buffer = &mut all_shards_data[current_offset..current_offset + group_len];

            for (i, shard) in group.iter().enumerate() {
                let start = i * shard_size;
                group_buffer[start..start + shard.len()].copy_from_slice(shard);
            }

            if m > 0 {
                RS_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    let rs = cache
                        .entry((k as u8, m as u8))
                        .or_insert_with(|| ReedSolomon::new(k, m).expect("RS init failed"));

                    let mut shard_ptrs: Vec<&mut [u8]> =
                        group_buffer.chunks_exact_mut(shard_size).collect();
                    rs.encode(&mut shard_ptrs).expect("RS encode failed");
                });
            }

            // Record metadata for every shard (data + parity).
            for shard_idx in 0..total_shards {
                let payload_len = if shard_idx < k {
                    group[shard_idx].len() as u16
                } else {
                    shard_size as u16
                };
                chunks_meta.push(ChunkMeta {
                    offset:     current_offset + shard_idx * shard_size,
                    payload_len,
                    group_idx:  group_idx as u8,
                    shard_idx:  shard_idx as u8,
                    k:          k as u8,
                    m:          m as u8,
                });
            }

            current_offset += group_len;
        }

        // ── Interleaving ─────────────────────────────────────────────────────
        //
        // PROBLEM with the old ordering (all data shards → all parity shards):
        //
        //   burst at the start  → destroys the same data-shard index across
        //                          every group simultaneously (worst case).
        //   burst at the end    → destroys ALL parity before it is used.
        //                          FEC becomes completely ineffective.
        //
        // FIX — "round-robin data+parity" order:
        //
        //   For round r = 0 … max(k, m) − 1:
        //     1. Send data[r]   for each group g  (if r < k_g)
        //     2. Send parity[r] for each group g  (if r < m_g)
        //
        // Effect: any window of (2 × group_count) consecutive packet losses
        // hits ≤ 1 data shard AND ≤ 1 parity shard per group.
        // FEC recovery budget is therefore ≈ min(m, burst_len / 2) per group
        // rather than 0 when all parity arrives at the tail.
        //
        // Example, k=32 m=8, 1 group, 40 shards sent in order:
        //   OLD: d0 d1 … d31  p0 p1 … p7   (parity at slots 32-39)
        //   NEW: d0 p0 d1 p1 … d7 p7 d8 d9 … d31
        //   A burst of 8 at any position now costs ≤4 data + ≤4 parity.
        //
        // Complexity: O(n) via HashMap lookup instead of the previous
        // O(n² ) `.position()` scan inside two nested loops.

        // Build O(1) index: (group_idx, shard_idx) → position in chunks_meta.
        let mut meta_index: HashMap<(u8, u8), usize> = HashMap::with_capacity(chunks_meta.len());
        for (pos, m) in chunks_meta.iter().enumerate() {
            meta_index.insert((m.group_idx, m.shard_idx), pos);
        }

        // Per-group (k, m) — the last group may have k < FEC_GROUP_K_MAX.
        let group_km: Vec<(u8, u8)> = (0..total_groups as usize)
            .map(|g| {
                chunks_meta
                    .iter()
                    .find(|m| m.group_idx == g as u8)
                    .map(|m| (m.k, m.m))
                    .unwrap_or((0, 0))
            })
            .collect();

        let max_k = group_km.iter().map(|(k, _)| *k as usize).max().unwrap_or(0);
        let max_m = group_km.iter().map(|(_, m)| *m as usize).max().unwrap_or(0);

        let mut interleaved_meta = Vec::with_capacity(chunks_meta.len());

        for round in 0..max_k.max(max_m) {
            // ── Data shard for this round, across all groups ──────────────
            for g in 0..total_groups as usize {
                let (k, _) = group_km[g];
                if round < k as usize {
                    if let Some(&pos) = meta_index.get(&(g as u8, round as u8)) {
                        interleaved_meta.push(chunks_meta[pos].clone());
                    }
                }
            }
            // ── Parity shard for this round, across all groups ────────────
            for g in 0..total_groups as usize {
                let (k, m) = group_km[g];
                if round < m as usize {
                    // Parity shards occupy indices k .. k+m in the flat array.
                    let parity_shard_idx = k + round as u8;
                    if let Some(&pos) = meta_index.get(&(g as u8, parity_shard_idx)) {
                        interleaved_meta.push(chunks_meta[pos].clone());
                    }
                }
            }
        }

        debug_assert_eq!(
            interleaved_meta.len(),
            chunks_meta.len(),
            "interleaving dropped or duplicated shards"
        );

        (EncodedSlice {
            frame_id,
            all_shards_data: all_shards_data.freeze(),
            chunks_meta: interleaved_meta,
        }, total_groups)
    }

    /// Calculate the number of parity shards for a group.
    ///
    /// Uses ceiling division so even k=1 gets at least 1 parity shard (or the
    /// hard minimum of 2, whichever is larger), and the total never exceeds the
    /// GF(2⁸) limit of 255.
    fn calculate_m(k: usize, is_critical: bool, is_first: bool) -> usize {
        let pct = if is_critical {
            FEC_M_CRITICAL_PCT
        } else if is_first {
            FEC_M_FIRST_SLICE_PCT
        } else {
            FEC_M_NORMAL_PCT
        };

        // 1. Считаем чистый процент (ceiling division)
        let mut m = (k * pct + 99) / 100;

        // 2. Умный минимум:
        // Если это критические данные (IDR), всегда даем хотя бы 2 шарда защиты.
        // Если обычные, и k > 1, даем хотя бы 1 шард. 
        // Если k=1 и это не критично, можно вообще 0 или 1.
        let min_m = if is_critical { 
            2 
        } else if k > 1 { 
            1 
        } else { 
            0 // Для k=1 в обычном P-фрейме паритет часто избыточен
        };

        m = m.max(min_m);

        // Ограничение GF(2^8)
        m.min(255usize.saturating_sub(k))
    }
}