use crate::{ControlPacket, DatagramChunk, FrameTrace, VideoPacket, fec::{builder::FrameBuilder, stats::RecoveryStats}};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

const MAX_BUFFERED_FRAMES: u64 = 140;

/// Drop a FrameBuilder that has been sitting unfinished for this many µs.
///
/// This timeout serves two purposes:
///
/// 1. **Partially-received frames** (at least one chunk arrived, builder exists):
///    `retain` evicts the builder after this deadline; the HOL loop then sees
///    `was_evicted = true` and skips past it.
///
/// 2. **Completely-lost frames** (zero chunks, no builder ever created):
///    `retain` cannot evict what doesn't exist.  Instead, `hol_stall_since_us`
///    measures how long the HOL pointer has been blocked on the absent frame
///    and forces a skip after the same deadline.
///
/// Without case 2 the HOL pointer would stall until `out_of_window` fires,
/// which at 60 fps takes 140 frames ≈ 2.3 s — causing a multi-second freeze
/// rather than the intended sub-STALE_FRAME_US hiccup.
const STALE_FRAME_US: u64 = 80_000;

/// Minimum interval between NACKs for the same (frame, slice, group) triple.
const NACK_SUPPRESS_US: u64 = 2_000;

/// Do NOT fire a NACK for a frame whose first shard arrived less than this
/// long ago.  Shards are paced by the sender and will still be in flight.
const MIN_NACK_AGE_US: u64 = 8_000;


// ── FrameAssembler ───────────────────────────────────────────────────────────

pub struct FrameAssembler {
    frames: HashMap<u64, FrameBuilder>,
    /// Tracks the last time a NACK was emitted for a (frame_id, slice_idx, group_idx)
    /// triple so we do not flood the sender.  Pruned alongside frame eviction.
    nack_sent_at: HashMap<(u64, u8, u8), u64>,
    /// Highest frame ID that was evicted (timeout or window overflow).
    /// Used by the HOL flush loop to skip over partially-received frames that
    /// were dropped by `retain`.
    last_evicted_id: Option<u64>,
    /// HOL blocking pointer: the frame ID that must be pushed *next*.
    ///
    /// `None` until the very first chunk arrives; afterwards it only ever
    /// advances forward.
    next_to_push_id: Option<u64>,
    /// Timestamp (µs) when the HOL pointer *first* stalled on a frame that is
    /// absent from `self.frames` — meaning every single chunk was lost in
    /// transit and no builder was ever created.
    ///
    /// Reset to `None` whenever the pointer advances for any reason.  When the
    /// stall duration reaches `STALE_FRAME_US` the loop forcibly skips the
    /// absent frame, bounding the worst-case freeze to one `STALE_FRAME_US`
    /// interval regardless of packet-loss pattern.
    hol_stall_since_us: Option<u64>,
    stats: RecoveryStats,
    stats_every_us: u64,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            frames: HashMap::new(),
            nack_sent_at: HashMap::new(),
            last_evicted_id: None,
            next_to_push_id: None,
            hol_stall_since_us: None,
            stats: RecoveryStats::default(),
            stats_every_us: 5_000_000,
        }
    }

    /// Insert a chunk and return strictly-ordered packets.
    ///
    /// Returns:
    /// - `Vec<VideoPacket>` — zero or more packets released in frame-ID order.
    ///   Multiple frames may be returned in a single call when completing a
    ///   frame unblocks a chain of already-ready successors.
    /// - `Option<ControlPacket>` — a NACK for the first stalled FEC group
    ///   found in the active window, if any.
    pub fn insert(&mut self, chunk: &DatagramChunk) -> (Vec<VideoPacket>, Option<ControlPacket>) {
        let frame_id = chunk.frame_id;
        let now_us   = FrameTrace::now_us();

        let key = (chunk.frame_id, chunk.slice_idx, chunk.group_idx);
        let was_nacked = self.nack_sent_at.contains_key(&key);

        self.stats.note_chunk(chunk, was_nacked);
        self.stats.maybe_log(now_us, self.stats_every_us);

        // Initialise the HOL pointer on the very first packet ever received.
        // Start from this frame_id so a mid-stream connect does not stall
        // waiting for frames that will never arrive.
        self.next_to_push_id.get_or_insert(frame_id);

        // ── Evict old / stale frames ─────────────────────────────────────────
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        let mut max_evicted = self.last_evicted_id;

        // Собираем ID кадров, которые пора выкинуть
        let mut to_evict = Vec::new();
        for (&id, builder) in &self.frames {
            let keep = id >= min_frame_id
                && now_us.saturating_sub(builder.first_us) < STALE_FRAME_US;
            if !keep {
                to_evict.push(id);
                max_evicted = Some(max_evicted.map_or(id, |m| m.max(id)));
            }
        }

        self.last_evicted_id = max_evicted;

        if let Some(cutoff) = max_evicted {
            self.nack_sent_at.retain(|key, _| key.0 > cutoff);
        }

        // Удаляем кадры и логируем потери по недособранным группам
        for id in to_evict {
            if let Some(builder) = self.frames.remove(&id) {
                let (failed_groups, wasted_bytes, lost_chunks) = builder.count_wasted();
                self.stats.note_wasted(failed_groups, wasted_bytes, lost_chunks);
            }
        }

        // ── NACK scan (before inserting so current frame is not checked) ─────
        let nack = self.maybe_generate_nack(frame_id, now_us);

        // ── Insert chunk ─────────────────────────────────────────────────────
        {
            let builder = self
                .frames
                .entry(frame_id)
                .or_insert_with(|| FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0));

            let recoveries = builder.insert_chunk(chunk);

            for recovery in recoveries {
                self.stats.note_group_recovery(recovery);
            }
        }

        // ── HOL flush loop ───────────────────────────────────────────────────
        let mut output = Vec::new();
        self.flush_ordered(min_frame_id, now_us, &mut output);

        (output, nack)
    }

    // ── HOL flush loop ───────────────────────────────────────────────────────

    /// Drain as many consecutive completed frames as possible starting from
    /// `next_to_push_id`, in strict ascending order.
    ///
    /// Decision table for each candidate frame:
    ///
    /// | State                               | Action                         |
    /// |-------------------------------------|--------------------------------|
    /// | In map, `is_complete()`             | Emit, advance, continue loop   |
    /// | In map, incomplete                  | Break — wait for more chunks   |
    /// | Absent, evicted or out-of-window    | Skip (lost), advance, continue |
    /// | Absent, stall timer expired         | Skip (totally lost), advance   |
    /// | Absent, stall timer still running   | Break — still within deadline  |
    fn flush_ordered(
        &mut self,
        min_frame_id: u64,
        now_us: u64,
        output: &mut Vec<VideoPacket>,
    ) {
        loop {
            let next_id = match self.next_to_push_id {
                Some(id) => id,
                None     => return,
            };

            if let Some(builder) = self.frames.get_mut(&next_id) {
                if builder.is_complete() {
                    // ── Frame ready ──────────────────────────────────────────
                    if let Some(packets) = builder.take_packets() {
                        self.stats.note_frame_emitted(packets.len());
                        output.extend(packets);
                    }
                    self.frames.remove(&next_id);
                    self.next_to_push_id  = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                } else {
                    // ── Frame present but incomplete ─────────────────────────
                    // Wait for more chunks; STALE eviction will handle timeout.
                    break;
                }
            } else {
                let was_evicted  = self.last_evicted_id.map_or(false, |e| next_id <= e);
                let out_of_window = next_id < min_frame_id;

                if was_evicted || out_of_window {
                    // ── Partially-received frame already evicted by retain ───
                    log::debug!(
                        "[HOL] skipping lost frame={} (evicted={}, out_of_window={})",
                        next_id, was_evicted, out_of_window,
                    );
                    if was_evicted {self.stats.note_frame_lost_evicted();}
                    if out_of_window {self.stats.note_frame_lost_out_of_window();}
                    self.next_to_push_id  = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                } else {
                    // ── Completely-lost frame: enforce stall timeout ─────────
                    //
                    // No builder exists so `retain` / STALE_FRAME_US cannot
                    // fire for this frame.  We track the first moment the HOL
                    // pointer blocked here and force a skip after STALE_FRAME_US
                    // to prevent the multi-second freeze that would otherwise
                    // occur when waiting for `out_of_window` (140 frames later).
                    let stall_start = *self.hol_stall_since_us.get_or_insert(now_us);
                    let stall_us    = now_us.saturating_sub(stall_start);

                    if stall_us >= STALE_FRAME_US {
                        log::debug!(
                            "[HOL] stall timeout ({} ms) for absent frame={}, skipping",
                            stall_us / 1_000,
                            next_id,
                        );
                        self.stats.note_frame_lost_timeout();
                        self.next_to_push_id  = Some(next_id + 1);
                        self.hol_stall_since_us = None;
                        self.next_to_push_id  = Some(next_id + 1);
                        self.hol_stall_since_us = None;
                        // Continue: successor may already be complete.
                    } else {
                        // Still within the grace window — keep waiting.
                        break;
                    }
                }
            }
        }
    }

    // ── NACK generation ──────────────────────────────────────────────────────

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
                None    => continue,
            };

            if builder.is_complete() {
                continue;
            }

            let age_us = now_us.saturating_sub(builder.first_us);
            if age_us < MIN_NACK_AGE_US {
                continue;
            }

            for slice_idx in 0..builder.total_slices {
                let slice_builder = match builder.slices.get(&slice_idx) {
                    Some(sb) => sb,
                    None     => continue,
                };

                if slice_builder.is_ready() {
                    continue;
                }

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
                        self.stats.note_nack();
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