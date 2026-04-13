/// /common/src/fec.rs
/// SOLOMON DATA SHARD ENCODING FOR MITIGATING PACKET LOSS


use std::collections::HashMap;
use reed_solomon_erasure::galois_8::ReedSolomon;
// Предполагаю, что DatagramChunk и VideoPacket живут в common
use crate::{DatagramChunk, FrameTrace, TYPE_VIDEO, VideoPacket};

const MAX_BUFFERED_FRAMES: u64 = 8;
/// Drop a FrameBuilder that has been sitting unfinished for this many µs (100 ms).
const STALE_FRAME_US: u64 = 100_000;

use std::cell::RefCell;

thread_local! {
    static RS_CACHE: RefCell<HashMap<(u8, u8), ReedSolomon>> = RefCell::new(HashMap::new());
}

pub struct FecEncoder;

impl FecEncoder {
    /// Нарезает слайс данных на шарды, вычисляет избыточность (FEC) 
    /// и возвращает список готовых к отправке чанков.
    pub fn encode_slice(
        frame_id: u64,
        slice_idx: u8,
        total_slices: u8,
        data: &[u8],
        max_chunk_data: usize,
        flags: u8,
    ) -> Vec<DatagramChunk> {

        // Calculale k+m
        let k = ((data.len() + max_chunk_data - 1) / max_chunk_data).max(1) as u8;

        // Adaptive parity shards
        let m = ((k as usize + 4) / 5).max(1) as u8;

        let shard_size = (data.len() + (k as usize) - 1) / (k as usize);
        
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

pub struct FrameAssembler {
    frames: HashMap<u64, FrameBuilder>,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            frames: HashMap::new(),
        }
    }

    /// Добавляет чанк. Если кадр полностью собран, возвращает VideoPacket.
    pub fn insert(&mut self, chunk: &DatagramChunk) -> Option<VideoPacket> {
        let frame_id = chunk.frame_id;
        let now_us   = FrameTrace::now_us();
 
        // ── Evict stale / old frames on every insert ──────────────────────────
        // Two eviction criteria:
        //   1. Frame ID too far behind the current one (sliding window).
        //   2. Frame has been sitting unfinished for > STALE_FRAME_US.
        // Running on every insert keeps memory bounded without relying on a
        // modulo trick that can miss cleanup if frame IDs are non-contiguous.
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        self.frames.retain(|&id, builder| {
            id >= min_frame_id && now_us.saturating_sub(builder.first_us) < STALE_FRAME_US
        });
 
        let builder = self.frames.entry(frame_id).or_insert_with(|| {
            FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0)
        });
 
        builder.insert_chunk(chunk)
    }
}

// ── Внутренние структуры (спрятаны от network.rs) ───────────────────────────

struct FrameBuilder {
    slices: HashMap<u8, SliceBuilder>,
    total_slices: u8,
    is_key: bool,
    first_us: u64,
}

impl FrameBuilder {
    fn new(total_slices: u8, is_key: bool) -> Self {
        Self {
            slices: HashMap::new(),
            total_slices,
            is_key,
            first_us: FrameTrace::now_us(),
        }
    }

    fn insert_chunk(&mut self, chunk: &DatagramChunk) -> Option<VideoPacket> {
        let slice_idx = chunk.slice_idx;
        
        let slice_builder = self.slices.entry(slice_idx).or_insert_with(|| {
            SliceBuilder::new(chunk.k, chunk.m)
        });

        slice_builder.insert(chunk);

        // Проверяем, готовы ли ВСЕ слайсы этого кадра
        if self.slices.len() as u8 == self.total_slices && 
           self.slices.values().all(|s| s.is_ready()) {
            self.assemble(chunk.frame_id)
        } else {
            None
        }
    }

    fn assemble(&mut self, frame_id: u64) -> Option<VideoPacket> {
        let mut full_payload = Vec::new();

        // Слайсы должны идти строго по порядку 0..total_slices
        for i in 0..self.total_slices {
            if let Some(slice_builder) = self.slices.get_mut(&i) {
                if let Some(mut recovered_data) = slice_builder.reconstruct() {
                    full_payload.append(&mut recovered_data);
                } else {
                    log::error!("Failed to reconstruct slice {} for frame {}", i, frame_id);
                    return None;
                }
            } else {
                return None; // Какого-то слайса вообще нет
            }
        }

        // 1. full_payload — это байты, созданные postcard на сервере! 
        // Десериализуем их обратно в структуру VideoPacket.
        let mut packet: VideoPacket = match postcard::from_bytes(&full_payload) {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to deserialize VideoPacket from FEC payload: {}", e);
                return None;
            }
        };

        // 2. Восстанавливаем метрики приемника (так как packet.trace УЖЕ содержит данные сервера)
        if let Some(ref mut trace) = packet.trace {
            trace.receive_us = self.first_us;
            trace.reassembled_us = FrameTrace::now_us();
        }

        // 3. Отдаем готовый пакет
        Some(packet)
    }
}

struct SliceBuilder {
    shards: Vec<Option<Vec<u8>>>,
    payload_lens: Vec<u16>,
    received: u8,
    k: u8,
    m: u8,
}

impl SliceBuilder {
    fn new(k: u8, m: u8) -> Self {
        Self {
            shards: vec![None; (k + m) as usize],
            payload_lens: vec![0; k as usize],
            received: 0,
            k,
            m,
        }
    }

    fn insert(&mut self, chunk: &DatagramChunk) {
        let idx = chunk.shard_idx as usize;
        if idx < self.shards.len() && self.shards[idx].is_none() {
            self.shards[idx] = Some(chunk.data.to_vec());
            if idx < self.k as usize {
                self.payload_lens[idx] = chunk.payload_len;
            }
            self.received += 1;
        }
    }

    /// Returns `true` once we have received ≥ k shards (data or parity) so
    /// that Reed-Solomon can reconstruct any missing data shards.
    fn is_ready(&self) -> bool {
        self.received >= self.k
    }

    fn reconstruct(&mut self) -> Option<Vec<u8>> {
        if !self.is_ready() { return None; }
 
        // Only invoke RS when at least one data shard is absent.
        let has_missing_data = self.shards
            .iter()
            .take(self.k as usize)
            .any(|s| s.is_none());
 
        if has_missing_data {
            let rs = RS_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                cache.entry((self.k, self.m))
                    .or_insert_with(|| ReedSolomon::new(self.k as usize, self.m as usize).unwrap())
                    .clone() // Arc-backed; cheap clone
            });
            rs.reconstruct(&mut self.shards).ok()?;
        }
 
        let total_size: usize = self.payload_lens.iter().map(|&l| l as usize).sum();
        let mut nalu_data = Vec::with_capacity(total_size);
 
        for i in 0..(self.k as usize) {
            let shard    = self.shards[i].as_ref()?;
            let real_len = self.payload_lens[i] as usize;
            nalu_data.extend_from_slice(&shard[..real_len]);
        }
 
        Some(nalu_data)
    }
}