use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{ChunkMeta, DatagramChunk, EncodedSlice, TYPE_VIDEO};
// Предполагается, что NetworkStats доступен из crate
// use crate::NetworkStats; 

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

const FEC_GROUP_K_MAX: usize = 16;
const FEC_M_CRITICAL_PCT: usize = 20;
const FEC_M_FIRST_SLICE_PCT: usize = 10;
const FEC_M_NORMAL_PCT: usize = 8;

// ── Network State ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkQuality {
    /// Идеальная сеть (LAN, Wi-Fi 6 в прямой видимости): RTT < 5ms, Loss ~0.
    /// Выключаем интерливинг, снижаем FEC.
    Excellent,
    /// Обычный интернет: RTT 5-50ms, Loss < 2%.
    /// Стандартный FEC и интерливинг.
    Normal,
    /// Проблемная сеть: RTT > 50ms, Loss > 2% или есть Burst-потери.
    /// Усиленный FEC.
    Degraded,
}

impl LinkQuality {
    pub fn evaluate(rtt: u64, loss_pct: f32, burst: u32) -> Self {
        if rtt < 5 && loss_pct < 0.5 && burst == 0 {
            Self::Excellent
        } else if rtt > 50 || loss_pct > 2.0 || burst > 1 {
            Self::Degraded
        } else {
            Self::Normal
        }
    }
}

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
        link_quality: LinkQuality, // <-- Передаем состояние сети
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

        let mut total_buffer_size = 0usize;
        for group in &fec_groups {
            let k = group.len();
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let m = Self::calculate_m(k, is_critical, is_first_slice, link_quality);
            total_buffer_size += (k + m) * shard_size;
        }

        let mut all_shards_data = BytesMut::with_capacity(total_buffer_size);
        all_shards_data.resize(total_buffer_size, 0);
        let mut chunks_meta = Vec::with_capacity(raw_shards.len() * 2);

        let mut current_offset = 0;
        for (group_idx, group) in fec_groups.iter().enumerate() {
            let k          = group.len();
            let shard_size = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let m          = Self::calculate_m(k, is_critical, is_first_slice, link_quality);
            let total_shards = k + m;
            let group_len = total_shards * shard_size;
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

            for shard_idx in 0..total_shards {
                let payload_len = if shard_idx < k {
                    group[shard_idx].len() as u16
                } else {
                    shard_size as u16
                };
                chunks_meta.push(ChunkMeta {
                    offset:      current_offset + shard_idx * shard_size,
                    payload_len,
                    group_idx:   group_idx as u8,
                    shard_idx:   shard_idx as u8,
                    k:           k as u8,
                    m:           m as u8,
                });
            }

            current_offset += group_len;
        }

        // ── Adaptive Interleaving ────────────────────────────────────────────
        // Отключаем интерливинг, если сеть идеальная. 
        // В локальной сети пакеты редко теряются "пачками", а интерливинг 
        // нарушает последовательность декодирования на ресивере.
        let final_meta = if link_quality == LinkQuality::Excellent {
            chunks_meta // Возвращаем плоский массив как есть
        } else {
            let mut meta_index: HashMap<(u8, u8), usize> = HashMap::with_capacity(chunks_meta.len());
            for (pos, m) in chunks_meta.iter().enumerate() {
                meta_index.insert((m.group_idx, m.shard_idx), pos);
            }

            let group_km: Vec<(u8, u8)> = (0..total_groups as usize)
                .map(|g| {
                    chunks_meta.iter().find(|m| m.group_idx == g as u8)
                        .map(|m| (m.k, m.m)).unwrap_or((0, 0))
                }).collect();

            let max_k = group_km.iter().map(|(k, _)| *k as usize).max().unwrap_or(0);
            let max_m = group_km.iter().map(|(_, m)| *m as usize).max().unwrap_or(0);

            let mut interleaved_meta = Vec::with_capacity(chunks_meta.len());

            for round in 0..max_k.max(max_m) {
                for g in 0..total_groups as usize {
                    let (k, _) = group_km[g];
                    if round < k as usize {
                        if let Some(&pos) = meta_index.get(&(g as u8, round as u8)) {
                            interleaved_meta.push(chunks_meta[pos].clone());
                        }
                    }
                }
                for g in 0..total_groups as usize {
                    let (k, m) = group_km[g];
                    if round < m as usize {
                        let parity_shard_idx = k + round as u8;
                        if let Some(&pos) = meta_index.get(&(g as u8, parity_shard_idx)) {
                            interleaved_meta.push(chunks_meta[pos].clone());
                        }
                    }
                }
            }
            interleaved_meta
        };

        (EncodedSlice {
            frame_id,
            all_shards_data: all_shards_data.freeze(),
            chunks_meta: final_meta,
        }, total_groups)
    }

    /// Вычисляет M (количество паритета) с учетом состояния сети.
    fn calculate_m(k: usize, is_critical: bool, is_first: bool, link_quality: LinkQuality) -> usize {
        let base_pct = if is_critical {
            FEC_M_CRITICAL_PCT
        } else if is_first {
            FEC_M_FIRST_SLICE_PCT
        } else {
            FEC_M_NORMAL_PCT
        };

        // Модификатор на основе состояния сети
        let pct = match link_quality {
            LinkQuality::Excellent => base_pct / 2, // В два раза меньше избыточности на LAN
            LinkQuality::Normal => base_pct,
            LinkQuality::Degraded => (base_pct * 2) / 2,
        };

        let mut m = (k * pct + 99) / 100;

        // Smart minimum
        let min_m = match (is_critical, link_quality) {
            (true, _) => 2, // Критичные фреймы всегда защищены
            (false, LinkQuality::Excellent) => 0, // Не критичные фреймы в LAN можно не защищать паритетом
            (false, _) if k > 1 => 1,
            _ => 0,
        };

        m = m.max(min_m);
        m.min(255usize.saturating_sub(k))
    }
}