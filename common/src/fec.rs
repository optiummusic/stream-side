/// /common/src/fec.rs
/// SOLOMON DATA SHARD ENCODING FOR MITIGATING PACKET LOSS

use std::collections::HashMap;
use std::cell::RefCell;
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{ControlPacket, DatagramChunk, FrameTrace, TYPE_VIDEO, VideoPacket};

const MAX_BUFFERED_FRAMES: u64 = 8;
/// Drop a FrameBuilder that has been sitting unfinished for this many µs (100 ms).
const STALE_FRAME_US: u64 = 100_000;

/// A NACK will not be re-sent for the same (frame_id, slice_idx) more often
/// than this interval (in µs). Set to ~one typical RTT on a LAN.
const NACK_SUPPRESS_US: u64 = 10_000; // 10 ms

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

pub struct FrameAssembler {
    frames: HashMap<u64, FrameBuilder>,
    /// Tracks the last time a NACK was emitted for a (frame_id, slice_idx)
    /// pair so we don't flood the sender. Entries are pruned alongside frames.
    nack_sent_at: HashMap<(u64, u8), u64>,
    /// Tracks the highest frame ID that was evicted to prevent NACKing old frames
    last_evicted_id: Option<u64>,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            frames: HashMap::new(),
            nack_sent_at: HashMap::new(),
            last_evicted_id: None,
        }
    }

    /// Insert a chunk and attempt reassembly.
    ///
    /// Returns a tuple of:
    /// - `Option<VideoPacket>` — a fully assembled frame, if one just completed.
    /// - `Option<ControlPacket>` — a NACK request, if the assembler detects a
    ///   slice that is stalled and not yet recoverable via FEC.
    pub fn insert(&mut self, chunk: &DatagramChunk) -> (Option<VideoPacket>, Option<ControlPacket>) {
        let frame_id = chunk.frame_id;
        let now_us   = FrameTrace::now_us();
 
        // ── Evict stale / old frames and their NACK timestamps ────────────────
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        let mut max_evicted = self.last_evicted_id;
        
        self.frames.retain(|&id, builder| {
            let keep = id >= min_frame_id && now_us.saturating_sub(builder.first_us) < STALE_FRAME_US;
            if !keep {
                max_evicted = Some(max_evicted.map_or(id, |m| m.max(id)));
            }
            keep
        });
        
        self.last_evicted_id = max_evicted;
        
        if let Some(evicted_cutoff) = max_evicted {
            self.nack_sent_at.retain(|key, _| key.0 > evicted_cutoff);
        }

        // ── Check preceding frames for NACKable slices ────────────────────────
        let nack = self.maybe_generate_nack(frame_id, now_us);

        // ── Normal insertion ──────────────────────────────────────────────────
        let builder = self.frames.entry(frame_id).or_insert_with(|| {
            FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0)
        });
 
        let assembled = builder.insert_chunk(chunk);
        (assembled, nack)
    }

    /// Look at frames behind the current one and return a NACK for the first
    /// slice that is stalled (has shards but cannot yet be recovered).
    fn maybe_generate_nack(&mut self, current_frame_id: u64, now_us: u64) -> Option<ControlPacket> {
        let min_allowed = match self.last_evicted_id {
            Some(id) => id + 1,
            None => 0,
        };
        
        // Scan all frames in the active window prior to the current arrival
        let min_id = current_frame_id.saturating_sub(MAX_BUFFERED_FRAMES).max(min_allowed);
        
        for target_frame_id in min_id..current_frame_id {
            let builder = match self.frames.get(&target_frame_id) {
                Some(b) => b,
                None => {
                    // FIX 3: Frame is completely missing. NACK slice 0 to bootstrap.
                    let key = (target_frame_id, 0);
                    let last_sent = self.nack_sent_at.get(&key).copied().unwrap_or(0);
                    if now_us.saturating_sub(last_sent) >= NACK_SUPPRESS_US {
                        self.nack_sent_at.insert(key, now_us);
                        return Some(ControlPacket::Nack {
                            frame_id: target_frame_id,
                            slice_idx: 0,
                            received_mask: 0,
                        });
                    }
                    continue;
                }
            };

            if builder.is_complete() {
                continue;
            }

            // FIX 1: Iterate over total expected slices, not just existing map keys
            for slice_idx in 0..builder.total_slices {
                let (needs_nack, mask) = match builder.slices.get(&slice_idx) {
                    Some(slice_builder) => {
                        if slice_builder.is_ready() {
                            (false, 0)
                        } else {
                            // Partially received slice needing repair
                            (true, slice_builder.received_mask())
                        }
                    }
                    None => {
                        // FIX 1/3: Slice is completely missing
                        (true, 0)
                    }
                };

                if needs_nack {
                    let key = (target_frame_id, slice_idx);
                    let last_sent = self.nack_sent_at.get(&key).copied().unwrap_or(0);
                    if now_us.saturating_sub(last_sent) >= NACK_SUPPRESS_US {
                        self.nack_sent_at.insert(key, now_us);
                        log::debug!(
                            "[NACK] frame={} slice={} received_mask={:#066b}",
                            target_frame_id, slice_idx, mask
                        );
                        return Some(ControlPacket::Nack {
                            frame_id: target_frame_id,
                            slice_idx,
                            received_mask: mask,
                        });
                    }
                }
            }
        }

        None
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

    fn is_complete(&self) -> bool {
        self.slices.len() as u8 == self.total_slices
            && self.slices.values().all(|s| s.is_ready())
    }

    fn insert_chunk(&mut self, chunk: &DatagramChunk) -> Option<VideoPacket> {
        let slice_idx = chunk.slice_idx;
        
        let slice_builder = self.slices.entry(slice_idx).or_insert_with(|| {
            SliceBuilder::new(chunk.k, chunk.m)
        });

        slice_builder.insert(chunk);

        // Проверяем, готовы ли ВСЕ слайсы этого кадра
        if self.is_complete() {
            self.assemble(chunk.frame_id)
        } else {
            None
        }
    }

    fn assemble(&mut self, frame_id: u64) -> Option<VideoPacket> {
        let mut full_payload = Vec::new();
        let mut final_trace: Option<FrameTrace> = None;

        // Slices must be processed in order 0..total_slices
        for i in 0..self.total_slices {
            let slice_builder = self.slices.get_mut(&i)?;
            let recovered_bytes = slice_builder.reconstruct()?;

            // --- THE KEY STEP: DESERIALIZE THE WRAPPER ---
            let (video_slice, _remainder): (crate::VideoSlice, _) = 
                match postcard::take_from_bytes(&recovered_bytes) {
                    Ok(res) => res,
                    Err(e) => {
                        log::error!("Postcard failure on slice {} frame {}: {}", i, frame_id, e);
                        return None;
                    }
                };

            // If this is the first slice, it contains our timing trace
            if i == 0 {
                final_trace = video_slice.trace;
                if let Some(ref mut t) = final_trace {
                    t.receive_us = self.first_us;
                    t.reassembled_us = FrameTrace::now_us();
                }
            }

            // Append only the ACTUAL video payload to the final buffer
            full_payload.extend_from_slice(&video_slice.payload);
        }

        Some(VideoPacket {
            frame_id,
            payload: full_payload,
            is_key: self.is_key,
            trace: final_trace,
        })
    }
}

struct SliceBuilder {
    shards: Vec<Option<Vec<u8>>>,
    payload_lens: Vec<u16>,
    shard_data_size: usize,
    received: u8,
    k: u8,
    m: u8,
}

impl SliceBuilder {
    fn new(k: u8, m: u8) -> Self {
        Self {
            shards: vec![None; (k + m) as usize],
            payload_lens: vec![0; k as usize],
            shard_data_size: 0,
            received: 0,
            k,
            m,
        }
    }

    fn insert(&mut self, chunk: &DatagramChunk) {
        let idx = chunk.shard_idx as usize;
        if idx < self.shards.len() && self.shards[idx].is_none() {
            // Save reference shard size
            if self.shard_data_size == 0 {
                self.shard_data_size = chunk.data.len();
            }

            self.shards[idx] = Some(chunk.data.to_vec());
            if idx < self.k as usize {
                self.payload_lens[idx] = chunk.payload_len;
            }
            self.received += 1;
        }
    }

    fn is_ready(&self) -> bool {
        self.received >= self.k
    }

    fn received_mask(&self) -> u64 {
        let mut mask = 0u64;
        for (i, shard) in self.shards.iter().enumerate() {
            if shard.is_some() {
                mask |= 1u64 << i;
            }
        }
        mask
    }

    fn reconstruct(&mut self) -> Option<Vec<u8>> {
        if !self.is_ready() { return None; }
 
        let has_missing_data = self.shards.iter().take(self.k as usize).any(|s| s.is_none());
 
        if has_missing_data {
            let rs = RS_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                cache.entry((self.k, self.m))
                    .or_insert_with(|| ReedSolomon::new(self.k as usize, self.m as usize).unwrap())
                    .clone()
            });
            rs.reconstruct(&mut self.shards).ok()?;
        }
 
        let mut total_size = 0;
        for i in 0..(self.k as usize) {
            if self.payload_lens[i] > 0 {
                total_size += self.payload_lens[i] as usize;
            } else {
                total_size += self.shard_data_size;
            }
        }

        let mut nalu_data = Vec::with_capacity(total_size);
 
        for i in 0..(self.k as usize) {
            let shard = self.shards[i].as_ref()?;
            let real_len = if self.payload_lens[i] > 0 {
                self.payload_lens[i] as usize
            } else {
                self.shard_data_size
            };
            
            let limit = real_len.min(shard.len());
            nalu_data.extend_from_slice(&shard[..limit]);
        }
 
        Some(nalu_data)
    }
}