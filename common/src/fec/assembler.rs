use std::collections::VecDeque;

use crate::{
    ControlPacket, DatagramChunk, FrameTrace, NackEntry, RecoveryStats, VideoPacket, fec::{
        decode_task::ComputePool, frame_builder::{FrameBuilder, FrameState}, group_builder::GroupState, slice_builder::SliceState,
    }
};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

const MAX_BUFFERED_FRAMES: u64 = 60;
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
    pub fn insert(&mut self, chunk: &DatagramChunk, receive_us: u64) -> (Vec<VideoPacket>, Vec<ControlPacket>, Option<ControlPacket>) {
        let frame_id = chunk.frame_id;


        // Initialise the HOL pointer on the very first packet ever received.
        self.next_to_push_id.get_or_insert(frame_id);
        // Если пакет пришел для кадра, который мы уже отдали на декодер (или скипнули),
        // просто игнорируем его, чтобы не создавать пустой FrameBuilder.
        if let Some(hol_id) = self.next_to_push_id {
            if frame_id < hol_id {
                // Пакет безнадежно опоздал (или это дубликат)
                self.stats.note_chunk(chunk, false, true);
                return (Vec::new(), Vec::new(), None);
            }
        }

        // ── Evict old / stale frames ─────────────────────────────────────────
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        let mut max_evicted = self.last_evicted_id;

        let mut to_evict = Vec::new();
        for (&id, builder) in &self.frames {
            let keep = id >= min_frame_id
                && receive_us.saturating_sub(builder.first_us) < STALE_FRAME_US;
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
                .or_insert_with(|| FrameBuilder::new(chunk.total_slices, chunk.flags & 1 != 0, receive_us));

            let (recoveries, tasks, is_nack_recovery, newly_stalled) = builder.insert_chunk(chunk);

            if let Some((slice_idx, group_idx)) = newly_stalled {
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
            }

            for recovery in recoveries {
                self.stats.note_group_recovery(recovery);
            }

            // Если требуется FEC — отправляем в async пул, не блокируя hot path.
            for task in tasks {
                self.compute_pool.submit(task);
            }

            self.stats.note_chunk(chunk, is_nack_recovery, false);
        }
        let stats_packet = self.stats
            .maybe_log(receive_us, self.stats_every_us)
            .map(|bytes| ControlPacket::RecoveryStats { 
                data: bytes::Bytes::copy_from_slice(bytes) 
            });
        // ── Apply async FEC results ──────────────────────────────────────────
        // Non-blocking drain: take everything the pool has finished.
        // We pass the data through even on RS failure (None) so apply_decode_result
        // can use decode_slice_sync for the full multi-group assembly; it guards
        // internally against frames/slices that are already done.
        for result in self.compute_pool.drain_results() {
            if let Some(builder) = self.frames.get_mut(&result.frame_id) {
                if builder.state == FrameState::Receiving {
                    // Pass an empty Vec on RS failure — apply_decode_result will
                    // call decode_slice_sync which handles per-group recovery.
                    builder.apply_decode_result(result.slice_idx, result.data.unwrap_or_default());
                }
            }
        }

        // ── Lazy NACK ────────────────────────────────────────────────────────
        // Спрашиваем у каждого FrameBuilder: «есть кандидат на NACK?»
        // Это O(кол-во активных кадров), а не O(кол-во всех групп).
        let nacks = self.drain_nacks(receive_us);

        // ── HOL flush loop ───────────────────────────────────────────────────
        let mut output = Vec::new();
        self.flush_ordered(min_frame_id, receive_us, &mut output);

        (output, nacks, stats_packet)
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
                    // ── Fast path: frame fully assembled ─────────────────────
                    let released_us = FrameTrace::now_us();
                    let ready_us = builder.network_ready_us.unwrap_or_default();
                    if let Some(mut packets) = builder.take_packets() {
                        for p in &mut packets {
                            if let Some(ref mut t) = p.trace {
                                t.network_ready_us = ready_us;
                                t.hol_released_us = released_us;
                                t.collecting_done_us = builder.collecting_done_us;
                                t.fec_submitted_us   = builder.fec_submitted_us;
                                t.fec_done_us        = builder.fec_done_us;
                            }
                        }
                        self.stats.note_frame_emitted(packets.len());
                        output.extend(packets);
                    }
                    self.frames.remove(&next_id);
                    self.next_to_push_id    = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                } else {
                    // ── Builder exists but frame is incomplete ────────────────
                    // Check if we should conceal and move on.
                    let stall_start = *self.hol_stall_since_us.get_or_insert(now_us);
                    let stall_us    = now_us.saturating_sub(stall_start);

                    if stall_us >= STALE_FRAME_US {
                        log::debug!(
                            "[HOL] stall timeout ({} ms) for incomplete frame={}, concealing",
                            stall_us / 1_000,
                            next_id,
                        );
                        self.stats.note_frame_lost_timeout();

                        // ↓ NEW: выплёвываем то, что есть + concealment-пустышки
                        let mut partial = builder.take_partial_packets(next_id);
                        if !partial.is_empty() {
                            let released_us = FrameTrace::now_us();
                            let ready_us = builder.network_ready_us.unwrap_or_default();
                            for p in &mut partial {
                                if let Some(ref mut t) = p.trace {
                                    t.network_ready_us = ready_us;
                                    t.hol_released_us = released_us;
                                    t.collecting_done_us = builder.collecting_done_us;
                                    t.fec_submitted_us   = builder.fec_submitted_us;
                                    t.fec_done_us        = builder.fec_done_us;
                                }
                            }
                            self.stats.note_frame_emitted(partial.len());
                            output.extend(partial);
                        }

                        self.frames.remove(&next_id);
                        self.stats.note_partial_slicing();
                        self.next_to_push_id    = Some(next_id + 1);
                        self.hol_stall_since_us = None;
                        // Continue: successor may already be complete.
                    } else {
                        break; // Still within wait window — keep blocking HOL.
                    }
                }
            } else {
                // ── No builder at all (zero packets received for this frame) ──
                let was_evicted   = self.last_evicted_id.map_or(false, |e| next_id <= e);
                let out_of_window = next_id < min_frame_id;

                if was_evicted || out_of_window {
                    log::debug!(
                        "[HOL] skipping lost frame={} (evicted={}, out_of_window={})",
                        next_id, was_evicted, out_of_window,
                    );
                    if was_evicted   { self.stats.note_frame_lost_evicted(); }
                    if out_of_window { self.stats.note_frame_lost_out_of_window(); }
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
                    // Only groups in Assembling slices can still benefit from a NACK.
                    // Slices in EmittingFec/Emitted/Abandoned have either queued RS
                    // tasks or are dead — retransmitting shards can't help them.
                    if sb.state == SliceState::Assembling {
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

    pub fn poll(&mut self) -> (Vec<VideoPacket>, Vec<ControlPacket>) {
        let now_us = FrameTrace::now_us();
        let min_frame_id = self.next_to_push_id
            .unwrap_or(0)
            .saturating_sub(MAX_BUFFERED_FRAMES);

        // Дренируем FEC результаты
        for result in self.compute_pool.drain_results() {
            if let Some(builder) = self.frames.get_mut(&result.frame_id) {
                if builder.state == FrameState::Receiving {
                    builder.apply_decode_result(result.slice_idx, result.data.unwrap_or_default());
                }
            }
        }

        // Выталкиваем готовые кадры
        let mut output = Vec::new();
        self.flush_ordered(min_frame_id, now_us, &mut output);

        // NACK-и тоже проверяем — вдруг что-то протухло
        let nacks = self.drain_nacks(now_us);

        (output, nacks)
    }
}