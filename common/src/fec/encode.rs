use std::cell::RefCell;

use bytes::{Bytes, BytesMut};
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{ChunkMeta, DatagramChunk, EncodedSlice, TYPE_VIDEO};

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
    pub fn encode(
        frame_id: u64,
        slice_idx: u8,
        total_slices: u8,
        data: &[u8],
        max_chunk_data: usize,
        flags: u8,
    ) -> EncodedSlice {
        // Заглушка для пустых данных
        if data.is_empty() || max_chunk_data == 0 {
            return EncodedSlice {
                frame_id,
                all_shards_data: Bytes::new(),
                chunks_meta: Vec::new(),
            };
        }

        let is_critical = (flags & 2) != 0;
        let is_first_slice = slice_idx == 0;

        let raw_shards: Vec<&[u8]> = data.chunks(max_chunk_data).collect();
        let fec_groups: Vec<&[&[u8]]> = raw_shards.chunks(FEC_GROUP_K_MAX).collect();
        let total_groups = fec_groups.len() as u8;

        // Предварительно рассчитываем общий объем данных, чтобы аллоцировать BytesMut один раз
        let mut total_buffer_size = 0;
        for group in &fec_groups {
            let k = group.len();
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let m = Self::calculate_m(k, is_critical, is_first_slice);
            total_buffer_size += (k + m) * shard_size;
        }

        let mut all_shards_data = BytesMut::with_capacity(total_buffer_size);
        let mut chunks_meta = Vec::with_capacity(raw_shards.len() * 2);

        for (group_idx, group) in fec_groups.iter().enumerate() {
            let k = group.len();
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let m = Self::calculate_m(k, is_critical, is_first_slice);
            let total_shards = k + m;

            // 1. Создаем временный плоский буфер для этой группы
            // Это единственная "тяжелая" аллокация на группу, данные отсюда уйдут в BytesMut
            let mut group_buffer = vec![0u8; total_shards * shard_size];

            // 2. Копируем данные исходных шардов в буфер группы
            for (i, shard) in group.iter().enumerate() {
                let start = i * shard_size;
                group_buffer[start..start + shard.len()].copy_from_slice(shard);
            }

            // 3. Reed-Solomon кодирование
            if m > 0 {
                RS_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    let rs = cache
                        .entry((k as u8, m as u8))
                        .or_insert_with(|| ReedSolomon::new(k, m).expect("RS init failed"));

                    // Нарезаем буфер на мутабельные срезы для RS
                    let mut shard_ptrs: Vec<&mut [u8]> = group_buffer
                        .chunks_exact_mut(shard_size)
                        .collect();
                    
                    rs.encode(&mut shard_ptrs).expect("RS encode failed");
                });
            }

            // 4. Фиксируем метаданные и переносим данные в общий буфер
            let base_offset = all_shards_data.len();
            for shard_idx in 0..total_shards {
                let payload_len = if shard_idx < k {
                    group[shard_idx].len() as u16
                } else {
                    shard_size as u16
                };

                chunks_meta.push(ChunkMeta {
                    offset: base_offset + (shard_idx * shard_size),
                    payload_len,
                    group_idx: group_idx as u8,
                    shard_idx: shard_idx as u8,
                    k: k as u8,
                    m: m as u8,
                });
            }
            
            all_shards_data.extend_from_slice(&group_buffer);
        }

        EncodedSlice {
            frame_id,
            all_shards_data: all_shards_data.freeze(),
            chunks_meta,
        }
    }

    /// Вынес логику расчета M в отдельный метод для чистоты
    fn calculate_m(k: usize, is_critical: bool, is_first: bool) -> usize {
        let m_raw = if is_critical || is_first {
            k           // 100%: keyframe / slice_idx == 0
        } else {
            (k + 1) / 2 // 50%: обычный слайс
        };
        m_raw.min(255usize.saturating_sub(k))
    }
}