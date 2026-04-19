use std::{cell::RefCell, collections::HashMap};

use bytes::Bytes;
use reed_solomon_erasure::galois_8::ReedSolomon;

use super::*;

thread_local! {
    static RS_CACHE: RefCell<HashMap<(u8, u8), ReedSolomon>> =
        RefCell::new(HashMap::new());
}

pub struct FecDecoder;

impl FecDecoder {
    pub fn decode(
        k: u8,
        m: u8,
        shards: &[Option<Bytes>],
        payload_lens: &[u16],
    ) -> Option<Vec<u8>> {
        let k_usize = k as usize;
        let total = (k + m) as usize;

        if shards.len() < total || payload_lens.len() < k_usize {
            return None;
        }

        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < k_usize {
            return None;
        }

        // Fast path: all data shards are already present, no RS reconstruct needed.
        let all_data_present = shards.iter().take(k_usize).all(|s| s.is_some());
        if all_data_present {
            let mut out = Vec::with_capacity(payload_lens.iter().map(|&l| l as usize).sum());

            for i in 0..k_usize {
                let shard = shards[i].as_ref()?;
                let len = (payload_lens[i] as usize).min(shard.len());
                out.extend_from_slice(&shard[..len]);
            }

            return Some(out);
        }

        // Slow path: copy only the available shards into mutable buffers and reconstruct.
        let mut temp: Vec<Option<Vec<u8>>> = shards
            .iter()
            .map(|s| s.as_ref().map(|b| b.to_vec()))
            .collect();

        RS_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let rs = cache
                .entry((k, m))
                .or_insert_with(|| ReedSolomon::new(k as usize, m as usize).unwrap());

            rs.reconstruct(&mut temp).ok()
        })?;

        let mut out = Vec::with_capacity(payload_lens.iter().map(|&l| l as usize).sum());

        for i in 0..k_usize {
            let shard = temp[i].as_ref()?;
            let len = (payload_lens[i] as usize).min(shard.len());
            out.extend_from_slice(&shard[..len]);
        }

        Some(out)
    }
}