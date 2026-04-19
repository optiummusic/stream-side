use crate::{ControlPacket, DatagramChunk, FrameTrace, VideoPacket, fec::builder::FrameBuilder};

use super::*;

const MAX_BUFFERED_FRAMES: u64 = 140;
/// Drop a FrameBuilder that has been sitting unfinished for this many µs (100 ms).
const STALE_FRAME_US: u64 = 20_000;

/// A NACK will not be re-sent for the same (frame_id, slice_idx) more often
/// than this interval (in µs). Set to ~one typical RTT on a LAN.
const NACK_SUPPRESS_US: u64 = 50_000; // 10 ms

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
    pub fn insert(&mut self, chunk: &DatagramChunk) -> (Vec<VideoPacket>, Option<ControlPacket>) {
        let frame_id = chunk.frame_id;
        let now_us = FrameTrace::now_us();

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

        let nack = self.maybe_generate_nack(frame_id, now_us);
        
        let assembled = {
            let builder = self.frames.entry(frame_id).or_insert_with(|| {
                FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0)
            });
            builder.insert_chunk(chunk).unwrap_or_default()
        };

        if self.frames.get(&frame_id).is_some_and(|b| b.is_complete()) {
            self.frames.remove(&frame_id);
        }

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
                    // let key = (target_frame_id, 0);
                    // let last_sent = self.nack_sent_at.get(&key).copied().unwrap_or(0);
                    // if now_us.saturating_sub(last_sent) >= NACK_SUPPRESS_US {
                    //     self.nack_sent_at.insert(key, now_us);
                    //     return Some(ControlPacket::Nack {
                    //         frame_id: target_frame_id,
                    //         slice_idx: 0,
                    //         received_mask: 0,
                    //     });
                    // }
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
                        (false, 0)
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