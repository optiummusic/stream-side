use std::collections::VecDeque;

use crate::{
    ControlPacket, DatagramChunk, FrameTrace, NackEntry, VideoPacket, fec::{
        builder::{ComputePool, FrameBuilder, GroupRecovery, GroupState},
        stats::RecoveryStats,
    }
};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

const MAX_BUFFERED_FRAMES: u64 = 140;
const NACK_BATCH_SIZE: u32 = 64;
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
const STALE_FRAME_US: u64 = 80_000;

// ── FrameAssembler ───────────────────────────────────────────────────────────

pub struct FrameAssembler {
    frames: HashMap<u64, FrameBuilder>,
    nack_queue: VecDeque<(u64, u8, u8)>,
    /// Highest frame ID that was evicted (timeout or window overflow).
    last_evicted_id: Option<u64>,
    /// HOL blocking pointer.
    next_to_push_id: Option<u64>,
    /// Timestamp (µs) when the HOL pointer first stalled on a completely-absent frame.
    hol_stall_since_us: Option<u64>,
    /// Async FEC compute pool — RS reconstruction runs off the hot path.
    compute_pool: ComputePool,
    stats: RecoveryStats,
    stats_every_us: u64,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            frames: HashMap::new(),
            nack_queue: VecDeque::new(),
            last_evicted_id: None,
            next_to_push_id: None,
            hol_stall_since_us: None,
            compute_pool: ComputePool::new(),
            stats: RecoveryStats::default(),
            stats_every_us: 5_000_000,
        }
    }

    /// Insert a chunk and return strictly-ordered packets.
    ///
    /// Returns:
    /// - `Vec<VideoPacket>` — zero or more packets released in frame-ID order.
    /// - `Option<ControlPacket>` — a NACK if any group is in a `Stalled` /
    ///   expired-`Requested` state (Lazy NACK: no heavy scan, each
    ///   `FrameBuilder` exposes candidates directly).
    pub fn insert(&mut self, chunk: &DatagramChunk) -> (Vec<VideoPacket>, Vec<ControlPacket>) {
        let frame_id = chunk.frame_id;
        let now_us   = FrameTrace::now_us();

        // ── Stats ────────────────────────────────────────────────────────────

        // Initialise the HOL pointer on the very first packet ever received.
        self.next_to_push_id.get_or_insert(frame_id);

        // Если пакет пришел для кадра, который мы уже отдали на декодер (или скипнули),
        // просто игнорируем его, чтобы не создавать пустой FrameBuilder.
        if let Some(hol_id) = self.next_to_push_id {
            if frame_id < hol_id {
                // Пакет безнадежно опоздал (или это дубликат)
                self.stats.note_chunk(chunk, false, true);
                return (Vec::new(), Vec::new(),);
            }
        }

        // ── Evict old / stale frames ─────────────────────────────────────────
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        let mut max_evicted = self.last_evicted_id;

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

        for id in to_evict {
            if let Some(builder) = self.frames.remove(&id) {
                let (failed_groups, wasted_bytes, lost_chunks) = builder.count_wasted();
                self.stats.note_wasted(failed_groups, wasted_bytes, lost_chunks);
            }
        }

        // ── Insert chunk ─────────────────────────────────────────────────────
        {
            let builder = self
                .frames
                .entry(frame_id)
                .or_insert_with(|| FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0));

            let (recoveries, maybe_task, is_nack_recovery, newly_stalled) = builder.insert_chunk(chunk);

            if let Some((slice_idx, group_idx)) = newly_stalled {
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
            }

            for recovery in recoveries {
                self.stats.note_group_recovery(recovery);
            }

            // Если требуется FEC — отправляем в async пул, не блокируя hot path.
            if let Some(task) = maybe_task {
                self.compute_pool.submit(task);
            }

            self.stats.note_chunk(chunk, is_nack_recovery, false);
            self.stats.maybe_log(now_us, self.stats_every_us);
        }

        // ── Apply async FEC results ──────────────────────────────────────────
        // Non-blocking drain: забираем всё, что пул успел посчитать.
        for result in self.compute_pool.drain_results() {
            if let Some(data) = result.data {
                if let Some(builder) = self.frames.get_mut(&result.frame_id) {
                    builder.apply_decode_result(result.slice_idx, data);
                }
            }
        }

        // ── Lazy NACK ────────────────────────────────────────────────────────
        // Спрашиваем у каждого FrameBuilder: «есть кандидат на NACK?»
        // Это O(кол-во активных кадров), а не O(кол-во всех групп).
        let nacks = self.drain_nacks(now_us);

        // ── HOL flush loop ───────────────────────────────────────────────────
        let mut output = Vec::new();
        self.flush_ordered(min_frame_id, now_us, &mut output);

        (output, nacks)
    }

    // ── HOL flush loop ───────────────────────────────────────────────────────

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
                    if let Some(packets) = builder.take_packets() {
                        self.stats.note_frame_emitted(packets.len());
                        output.extend(packets);
                    }
                    self.frames.remove(&next_id);
                    self.next_to_push_id    = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                } else {
                    break;
                }
            } else {
                let was_evicted   = self.last_evicted_id.map_or(false, |e| next_id <= e);
                let out_of_window = next_id < min_frame_id;

                if was_evicted || out_of_window {
                    log::debug!(
                        "[HOL] skipping lost frame={} (evicted={}, out_of_window={})",
                        next_id, was_evicted, out_of_window,
                    );
                    if was_evicted    { self.stats.note_frame_lost_evicted(); }
                    if out_of_window  { self.stats.note_frame_lost_out_of_window(); }
                    self.next_to_push_id    = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                } else {
                    let stall_start = *self.hol_stall_since_us.get_or_insert(now_us);
                    let stall_us    = now_us.saturating_sub(stall_start);

                    if stall_us >= STALE_FRAME_US {
                        log::debug!(
                            "[HOL] stall timeout ({} ms) for absent frame={}, skipping",
                            stall_us / 1_000,
                            next_id,
                        );
                        self.stats.note_frame_lost_timeout();
                        self.next_to_push_id    = Some(next_id + 1);
                        self.hol_stall_since_us = None;
                        // Continue: successor may already be complete.
                    } else {
                        break;
                    }
                }
            }
        }
    }

    // ── Lazy NACK ────────────────────────────────────────────────────────────
    //
    // Вместо полного сканирования всех групп (старый `maybe_generate_nack`)
    // мы делегируем поиск каждому `FrameBuilder`.  Каждая группа сама ведёт
    // свой `GroupState` и знает, когда ей нужен NACK — нам остаётся лишь
    // пройтись по активным кадрам и спросить первого «желающего».

    fn drain_nacks(&mut self, now_us: u64) -> Vec<ControlPacket> {
        // frame_id -> Vec<NackEntry>
        let mut batches: HashMap<u64, Vec<NackEntry>> = HashMap::new();
        let initial_len = self.nack_queue.len();

        for _ in 0..initial_len {
            let (frame_id, slice_idx, group_idx) = self.nack_queue.pop_front().unwrap();

            // Общий лимит батча на кадр
            let frame_batch = batches.entry(frame_id).or_default();
            if frame_batch.len() >= NACK_BATCH_SIZE as usize {
                // Этот frame уже забит — откладываем на следующий тик
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
                continue;
            }

            let mut keep_in_queue = false;
            let mut needs_nack = false;
            let mut mask = 0u64;

            if let Some(builder) = self.frames.get_mut(&frame_id) {
                if let Some(sb) = builder.slices.get_mut(&slice_idx) {
                    if let Some(gb) = sb.groups.get_mut(&group_idx) {
                        if gb.state.needs_nack(now_us) {
                            needs_nack = true;
                            mask = gb.received_mask();
                            gb.state = GroupState::Requested { sent_at_us: now_us };
                            keep_in_queue = true;
                        } else if matches!(gb.state, GroupState::Stalled { .. } | GroupState::Requested { .. }) {
                            keep_in_queue = true;
                        }
                    }
                }
            }

            if needs_nack {
                frame_batch.push(NackEntry { slice_idx, group_idx, received_mask: mask });
            }

            if keep_in_queue {
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
            }
        }

        // Собираем ControlPacket для каждого frame_id у которого есть entries
        let mut result = Vec::with_capacity(batches.len());
        for (frame_id, entries) in batches {
            if !entries.is_empty() {
                self.stats.note_nack();
                result.push(ControlPacket::NackBatch { frame_id, entries });
            }
        }
        result
    }
}