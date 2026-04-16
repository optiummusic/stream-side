use std::cell::RefCell;

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{DatagramChunk, TYPE_VIDEO};

use super::*;

pub struct FecEncoder;
thread_local! {
    static RS_CACHE: RefCell<HashMap<(u8, u8), ReedSolomon>> = RefCell::new(HashMap::new());
}
impl FecEncoder {
    /// Нарезает слайс данных на шарды, вычисляет избыточность (FEC) 
    /// и возвращает список готовых к отправке чанков.
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
        let is_critical = (flags & 2) != 0;
        let is_first_slice = slice_idx == 0;
        
        let k = ((data.len() + max_chunk_data - 1) / max_chunk_data).max(1) as u8;

        // Adaptive parity shards
        let mut m_raw = if is_critical || is_first_slice { k as usize } else { k as usize / 2 + 1 };
        if (k as usize + m_raw) > 255 {
            m_raw = 255 - k as usize;
        }
        let m = m_raw.min(4) as u8;

        let shard_size = (data.len() + (k as usize) - 1) / (k as usize);
        let shard_size = shard_size.max(1);
        
        // Padding
        let mut shards: Vec<Vec<u8>> = data
            .chunks(shard_size)
            .map(|chunk| {
                let mut s = chunk.to_vec();
                s.resize(shard_size, 0); 
                s
            })
            .collect();

        // Calculate parity
        for _ in 0..m {
            shards.push(vec![0u8; shard_size]);
        }
        
        RS_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let rs = cache
                .entry((k, m))
                .or_insert_with(|| ReedSolomon::new(k as usize, m as usize).unwrap());
            let _ = rs.encode(&mut shards);
        });

        //Generate shards
        shards.into_iter().enumerate().map(|(shard_idx, shard_data)| {
            // payload_len tracks the *actual* bytes contributed by this data
            // shard (< shard_size for the last one); parity shards carry the
            // full shard_size so the receiver can strip padding correctly.
            let payload_len = if shard_idx < k as usize {
                let start = shard_idx * shard_size;
                let end   = (start + shard_size).min(data.len());
                (end - start) as u16
            } else {
                shard_size as u16
            };
 
            DatagramChunk {
                frame_id,
                slice_idx,
                total_slices,
                shard_idx: shard_idx as u8,
                k,
                m,
                payload_len,
                packet_type: TYPE_VIDEO,
                flags,
                data: shard_data.into(),
            }
        }).collect()
    }
}