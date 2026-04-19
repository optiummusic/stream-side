use crate::{ControlPacket, DatagramChunk, FrameTrace, VideoPacket, fec::builder::FrameBuilder};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

const MAX_BUFFERED_FRAMES: u64 = 140;

/// Drop a FrameBuilder that has been sitting unfinished for this many µs (20 ms).
const STALE_FRAME_US: u64 = 20_000;

/// Minimum interval between NACKs for the same (frame, slice, group) triple.
/// ~50 ms — roughly one RTT on a local/LAN path.
const NACK_SUPPRESS_US: u64 = 50_000;


/// Do NOT fire a NACK for a frame whose first shard arrived less than this
/// long ago.  Shards are paced by the sender and will still be in flight.
///
/// Root cause of the "slice=6 received=0 on ALL groups" pattern in the logs:
/// the frame's earlier slices arrived (creating the FrameBuilder), but slice 6
/// — sent last by the serialiser — hadn't even been transmitted yet when the
/// NACK scan fired.  MIN_NACK_AGE silences this entire class of false alarms.
///
/// Rule of thumb: ≥ 2× one-way trip + time to transmit all groups of the
/// largest expected slice.  8 ms is safe on a sub-1 ms LAN; raise to ~20 ms
/// over WAN or when send pacing is noticeable.
const MIN_NACK_AGE_US: u64 = 8_000; // 8 ms


// ── FrameAssembler ───────────────────────────────────────────────────────────

pub struct FrameAssembler {
    frames: HashMap<u64, FrameBuilder>,
    /// Tracks the last time a NACK was emitted for a (frame_id, slice_idx, group_idx)
    /// triple so we do not flood the sender.  Pruned alongside frame eviction.
    nack_sent_at: HashMap<(u64, u8, u8), u64>,
    /// Highest frame ID that was evicted, used to prevent NACKing stale frames.
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
    /// Returns:
    /// - `Vec<VideoPacket>` — zero or more fully ordered packets if slices
    ///   completed.
    /// - `Option<ControlPacket>` — a NACK for the first stalled FEC group
    ///   found in the active window, if any.
    pub fn insert(&mut self, chunk: &DatagramChunk) -> (Vec<VideoPacket>, Option<ControlPacket>) {
        let frame_id = chunk.frame_id;
        let now_us   = FrameTrace::now_us();

        // ── Evict old / stale frames ─────────────────────────────────────────
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        let mut max_evicted = self.last_evicted_id;

        self.frames.retain(|&id, builder| {
            let keep = id >= min_frame_id
                && now_us.saturating_sub(builder.first_us) < STALE_FRAME_US;
            if !keep {
                max_evicted = Some(max_evicted.map_or(id, |m| m.max(id)));
            }
            keep
        });

        self.last_evicted_id = max_evicted;

        if let Some(cutoff) = max_evicted {
            self.nack_sent_at.retain(|key, _| key.0 > cutoff);
        }

        // ── NACK scan (before inserting so current frame is not checked) ─────
        let nack = self.maybe_generate_nack(frame_id, now_us);

        // ── Insert chunk ─────────────────────────────────────────────────────
        let assembled = {
            let builder = self
                .frames
                .entry(frame_id)
                .or_insert_with(|| FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0));
            builder.insert_chunk(chunk).unwrap_or_default()
        };

        if self.frames.get(&frame_id).is_some_and(|b| b.is_complete()) {
            self.frames.remove(&frame_id);
        }

        (assembled, nack)
    }

    // ── NACK generation ──────────────────────────────────────────────────────

    /// Scan frames behind `current_frame_id` for stalled FEC groups and emit
    /// a single NACK for the first one that passes the suppression window.
    ///
    /// NACKs are now scoped to `(frame_id, slice_idx, group_idx)` so that:
    /// - Each FEC group can be retransmitted independently.
    /// - Burst-loss patterns that destroy a single group leave adjacent groups
    ///   unaffected and do not trigger spurious NACKs for them.
    fn maybe_generate_nack(
        &mut self,
        current_frame_id: u64,
        now_us: u64,
    ) -> Option<ControlPacket> {
        let min_allowed = self.last_evicted_id.map_or(0, |id| id + 1);
        let min_id = current_frame_id
            .saturating_sub(MAX_BUFFERED_FRAMES)
            .max(min_allowed);

        for target_frame_id in min_id..current_frame_id {
            let builder = match self.frames.get(&target_frame_id) {
                Some(b) => b,
                None    => continue, // frame not yet seen — no shards to reference
            };

            if builder.is_complete() {
                continue;
            }

            // For some reason it currently supresses input FPS (meaning it reduces overall output from assembler)
            // let age_us = now_us.saturating_sub(builder.first_us);
            // if age_us < MIN_NACK_AGE_US {
            //     continue;
            // }
            
            for slice_idx in 0..builder.total_slices {
                let slice_builder = match builder.slices.get(&slice_idx) {
                    Some(sb) => sb,
                    None => continue,
                };

                if slice_builder.is_ready() {
                    continue;
                }

                // Iterate over every expected FEC group in this slice.
                for group_idx in 0..slice_builder.total_groups {
                    let group_ready = slice_builder
                        .groups
                        .get(&group_idx)
                        .map_or(false, |g| g.is_ready());

                    if group_ready {
                        continue;
                    }

                    let received_mask = slice_builder.group_received_mask(group_idx);
                    let key = (target_frame_id, slice_idx, group_idx);
                    let last_sent = self.nack_sent_at.get(&key).copied().unwrap_or(0);

                    if now_us.saturating_sub(last_sent) >= NACK_SUPPRESS_US {
                        self.nack_sent_at.insert(key, now_us);

                        log::debug!(
                            "[NACK] frame={} slice={} group={}/{} received_mask={:#066b}",
                            target_frame_id,
                            slice_idx,
                            group_idx,
                            slice_builder.total_groups,
                            received_mask,
                        );

                        return Some(ControlPacket::Nack {
                            frame_id: target_frame_id,
                            slice_idx,
                            group_idx,
                            received_mask,
                        });
                    }
                }
            }
        }

        None
    }
}