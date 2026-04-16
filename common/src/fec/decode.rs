use std::cell::RefCell;

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::*;

thread_local! {
    static RS_CACHE: RefCell<HashMap<(u8, u8), ReedSolomon>> = RefCell::new(HashMap::new());
}

pub struct FecDecoder;

impl FecDecoder {
    pub fn decode(
        k: u8,
        m: u8,
        mut shards: Vec<Option<Vec<u8>>>,
        payload_lens: &[u16],
    ) -> Option<Vec<u8>> {
        if shards.len() < k as usize { return None; }

        let has_missing_data = shards.iter().take(k as usize).any(|s| s.is_none());

        if has_missing_data {
            let rs = RS_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                cache.entry((k, m))
                    .or_insert_with(|| ReedSolomon::new(k as usize, m as usize).unwrap())
                    .clone()
            });
            rs.reconstruct(&mut shards).ok()?;
        }

        // Определяем общий размер данных, отсекая padding
        let mut total_size = 0;
        for i in 0..(k as usize) {
            total_size += payload_lens[i] as usize;
        }

        let mut recovered_data = Vec::with_capacity(total_size);
        for i in 0..(k as usize) {
            let shard = shards[i].as_ref()?;
            let len = payload_lens[i] as usize;
            let limit = len.min(shard.len());
            recovered_data.extend_from_slice(&shard[..limit]);
        }

        Some(recovered_data)
    }
}