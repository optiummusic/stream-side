use std::cell::RefCell;

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{DatagramChunk, TYPE_VIDEO};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum data shards per FEC group.
///
/// Keeping k ≤ this value bounds Reed-Solomon matrix operations to
/// O(k²) ≈ O(256) per byte — vs O(33 856) when k=184 as was happening before.
/// Independent groups also allow per-group recovery under burst loss and
/// stay well within the GF(2⁸) limit of 255 total shards.
///
/// Industry references: SMPTE 2022-1 uses L×D ≤ 20×20, RFC 5109 (RTP FEC)
/// typically uses groups of 5–24 packets.
const FEC_GROUP_K_MAX: usize = 16;

// ── Thread-local RS instance cache ──────────────────────────────────────────

thread_local! {
    static RS_CACHE: RefCell<HashMap<(u8, u8), ReedSolomon>> =
        RefCell::new(HashMap::new());
}

// ── FecEncoder ───────────────────────────────────────────────────────────────

pub struct FecEncoder;

impl FecEncoder {
    /// Encode a slice of data into FEC-protected [`DatagramChunk`]s.
    ///
    /// # FEC Group design
    ///
    /// 1. `data` is divided into raw data shards of at most `max_chunk_data`
    ///    bytes each.
    /// 2. Those shards are partitioned into **FEC groups** of at most
    ///    [`FEC_GROUP_K_MAX`] shards.
    /// 3. Every group is independently Reed-Solomon encoded, producing `m`
    ///    parity shards chosen adaptively:
    ///    - 100 % parity (`m = k`) for critical slices or the first slice of a
    ///      frame (codec headers that cannot be skipped).
    ///    - 50 % parity (`m = ⌈k/2⌉`) for all other slices.
    ///
    /// Each emitted [`DatagramChunk`] carries `group_idx` / `total_groups` so
    /// the receiver can reassemble groups independently and begin decoding the
    /// first completed groups without waiting for later ones.
    pub fn encode(
        frame_id: u64,
        slice_idx: u8,
        total_slices: u8,
        data: &[u8],
        max_chunk_data: usize,
        flags: u8,
    ) -> Vec<DatagramChunk> {
        if data.is_empty() || max_chunk_data == 0 {
            return Vec::new();
        }

        let is_critical   = (flags & 2) != 0;
        let is_first_slice = slice_idx == 0;

        // Step 1 – split `data` into raw data shards of ≤ max_chunk_data bytes.
        let raw_shards: Vec<&[u8]> = data.chunks(max_chunk_data).collect();

        // Step 2 – partition into FEC groups of ≤ FEC_GROUP_K_MAX shards.
        let fec_groups: Vec<&[&[u8]]> = raw_shards.chunks(FEC_GROUP_K_MAX).collect();
        let total_groups = fec_groups.len() as u8;

        log::trace!(
            "[FEC] encode frame#{} slice#{}/{} | total_len: {}, groups: {}, k_max: {}",
            frame_id, slice_idx, total_slices,
            data.len(), total_groups, FEC_GROUP_K_MAX
        );

        let mut all_chunks = Vec::with_capacity(raw_shards.len() * 2);

        // Step 3 – encode each group independently.
        for (group_idx, group) in fec_groups.iter().enumerate() {
            let k = group.len() as u8;

            // All data shards in a group are padded to the largest shard.
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);

            // Adaptive parity selection.
            let m_raw = if is_critical || is_first_slice {
                k as usize          // 100 % — full redundancy for headers/critical
            } else {
                (k as usize + 1) / 2 // 50 %  — standard redundancy for data slices
            };
            // k + m must not exceed 255 (GF(2⁸) hard limit).
            let m = m_raw.min(255usize.saturating_sub(k as usize)) as u8;

            log::trace!(
                "[FEC]   group#{}/{} k={} m={} shard_size={} part_len={}",
                group_idx, total_groups, k, m, shard_size,
                group.iter().map(|s| s.len()).sum::<usize>()
            );

            // Build padded data shards.
            let mut shards: Vec<Vec<u8>> = group
                .iter()
                .map(|s| {
                    let mut v = s.to_vec();
                    v.resize(shard_size, 0);
                    v
                })
                .collect();

            // Append zero-initialised parity shards.
            for _ in 0..m {
                shards.push(vec![0u8; shard_size]);
            }

            // Compute parity in-place.
            if m > 0 {
                RS_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    let rs = cache
                        .entry((k, m))
                        .or_insert_with(|| {
                            ReedSolomon::new(k as usize, m as usize)
                                .expect("invalid RS parameters")
                        });
                    let _ = rs.encode(&mut shards);
                });
            }

            // Emit one DatagramChunk per shard (data + parity).
            for (shard_idx, shard_data) in shards.into_iter().enumerate() {
                // `payload_len` records the *actual* unpadded bytes for data
                // shards so the receiver can strip padding after reconstruction.
                // Parity shards record `shard_size` so the receiver knows the
                // geometry without needing extra metadata.
                let payload_len = if shard_idx < k as usize {
                    group[shard_idx].len() as u16
                } else {
                    shard_size as u16
                };

                all_chunks.push(DatagramChunk {
                    frame_id,
                    slice_idx,
                    total_slices,
                    group_idx: group_idx as u8,
                    total_groups,
                    shard_idx: shard_idx as u8,
                    k,
                    m,
                    payload_len,
                    packet_type: TYPE_VIDEO,
                    flags,
                    data: shard_data.into(),
                });
            }
        }

        all_chunks
    }
}