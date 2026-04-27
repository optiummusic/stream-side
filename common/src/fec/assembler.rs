use std::collections::VecDeque;

use crate::{
    ControlPacket, DatagramChunk, FrameTrace, NackEntry, RecoveryStats, VideoPacket, fec::{
        assembly_log::{
            AssemblyDiag, FrameSnap, SliceSnap, GroupSnap,
            frame_state_label, slice_state_label, group_state_label,
            snap_group,
        },
        decode_task::ComputePool,
        frame_builder::{FrameBuilder, FrameState},
        group_builder::GroupState,
        slice_builder::SliceState,
    }
};

use super::*;

// ── Constants ────────────────────────────────────────────────────────────────

const MAX_BUFFERED_FRAMES: u64 = 512;
const NACK_BATCH_SIZE: u32 = 64;
const STALE_FRAME_US: u64 = 150_000;

/// How often (in inserted chunks) to dump the full rolling-window summary.
/// At ~1000 chunks/s this fires roughly every second.
const WINDOW_DUMP_EVERY_N_CHUNKS: u64 = 500;

// ── FrameAssembler ───────────────────────────────────────────────────────────

pub struct FrameAssembler {
    frames:             HashMap<u64, FrameBuilder>,
    nack_queue:         VecDeque<(u64, u8, u8)>,
    last_evicted_id:    Option<u64>,
    next_to_push_id:    Option<u64>,
    hol_stall_since_us: Option<u64>,
    compute_pool:       ComputePool,
    stats:              RecoveryStats,
    stats_every_us:     u64,

    // ── Diagnostic state ─────────────────────────────────────────────────────
    /// Rolling 10-frame window logger.
    diag:               AssemblyDiag,
    /// Monotonically increasing chunk counter (for periodic window dumps).
    chunk_counter:      u64,
}

impl FrameAssembler {
    pub fn new() -> Self {
        log::info!("[ASSEMBLER-INIT] FrameAssembler created \
                   MAX_BUFFERED_FRAMES={MAX_BUFFERED_FRAMES} \
                   STALE_FRAME_US={STALE_FRAME_US}µs");
        Self {
            frames:             HashMap::new(),
            nack_queue:         VecDeque::new(),
            last_evicted_id:    None,
            next_to_push_id:    None,
            hol_stall_since_us: None,
            compute_pool:       ComputePool::new(),
            stats:              RecoveryStats::default(),
            stats_every_us:     5_000_000,
            diag:               AssemblyDiag::new(),
            chunk_counter:      0,
        }
    }

    // ── Snapshot helpers ─────────────────────────────────────────────────────

    /// Build a fresh `FrameSnap` from a `FrameBuilder` and upsert it into the
    /// diagnostic window.  Call this after any state-changing operation.
    fn refresh_snap(&mut self, frame_id: u64, now_us: u64) {
        if !log::log_enabled!(log::Level::Debug) { return; }

        let Some(fb) = self.frames.get(&frame_id) else { return };

        let slices: Vec<SliceSnap> = (0..fb.total_slices).filter_map(|si| {
            let sb = fb.slices.get(&si)?;
            let groups: Vec<GroupSnap> = (0..sb.total_groups).filter_map(|gi| {
                let gb = sb.groups.get(&gi)?;
                Some(snap_group(
                    gi,
                    gb.k,
                    gb.m,
                    gb.received,
                    gb.data_received,
                    gb.received_mask(),
                    group_state_label(&gb.state),
                    gb.first_us,
                ))
            }).collect();

            Some(SliceSnap {
                slice_idx:    si,
                total_groups: sb.total_groups,
                ready_groups: sb.ready_groups,
                state_label:  slice_state_label(&sb.state),
                groups,
            })
        }).collect();

        let snap = FrameSnap {
            frame_id,
            total_slices:       fb.total_slices,
            emitted_slices:     fb.next_emit_idx,
            next_emit_idx:      fb.next_emit_idx,
            state_label:        frame_state_label(&fb.state),
            first_us:           fb.first_us,
            age_us:             now_us.saturating_sub(fb.first_us),
            is_hol:             self.next_to_push_id == Some(frame_id),
            slices,
            collecting_done_us: fb.collecting_done_us,
            fec_submitted_us:   fb.fec_submitted_us,
            fec_done_us:        fb.fec_done_us,
            network_ready_us:   fb.network_ready_us,
        };

        self.diag.upsert_frame(snap);
    }

    // ── Public API ───────────────────────────────────────────────────────────
    const REORDER_WINDOW: u64 = 3;
    pub fn insert(
        &mut self,
        chunk: &DatagramChunk,
        receive_us: u64,
    ) -> (Vec<VideoPacket>, Vec<ControlPacket>, Option<ControlPacket>) {
        let frame_id  = chunk.frame_id;
        let is_parity = chunk.shard_idx >= chunk.k;

        self.chunk_counter += 1;
        self.diag.inc_shard_count();

        // ── HOL init ─────────────────────────────────────────────────────────
        let was_uninit = self.next_to_push_id.is_none();
        self.next_to_push_id.get_or_insert(frame_id);
        if was_uninit {
            log::info!(
                "[ASSEMBLER-HOL-INIT] HOL pointer initialised to frame={frame_id}"
            );
        }

        // ── Zombie / behind-HOL check ─────────────────────────────────────────
        if let Some(hol_id) = self.next_to_push_id {
            if frame_id < hol_id {
                log::warn!(
                    "[ZOMBIE] frame={frame_id} < HOL={hol_id} s={si} g={gi} shard={sh} \
                     ({kind}) — too late, counting as zombie",
                    si   = chunk.slice_idx,
                    gi   = chunk.group_idx,
                    sh   = chunk.shard_idx,
                    kind = if is_parity { "P" } else { "D" },
                );
                self.stats.note_chunk(chunk, false, true);
                return (Vec::new(), Vec::new(), None);
            }
        }

        // ── Evict stale frames ────────────────────────────────────────────────
        let min_frame_id = frame_id.saturating_sub(MAX_BUFFERED_FRAMES);
        let mut max_evicted = self.last_evicted_id;
        let mut to_evict: Vec<u64> = Vec::new();

        for (&id, builder) in &self.frames {
            let age = receive_us.saturating_sub(builder.first_us);
            let in_window = id >= min_frame_id;
            let not_stale = age < STALE_FRAME_US;
            let keep = in_window && not_stale;
            if !keep {
                to_evict.push(id);
                max_evicted = Some(max_evicted.map_or(id, |m: u64| m.max(id)));
                log::warn!(
                    "[EVICT-CANDIDATE] frame={id} age={age}µs \
                     in_window={in_window} not_stale={not_stale} — will evict"
                );
            }
        }

        self.last_evicted_id = max_evicted;

        for id in &to_evict {
            if let Some(builder) = self.frames.remove(id) {
                let (failed_groups, wasted_bytes, lost_chunks) = builder.count_wasted();
                log::warn!(
                    "[FRAME-EVICTED] frame={id} \
                     failed_groups={failed_groups} wasted_bytes={wasted_bytes}B \
                     lost_chunks={lost_chunks} last_evicted_id={:?}",
                    self.last_evicted_id,
                );
                self.stats.note_wasted(failed_groups, wasted_bytes, lost_chunks);
                self.diag.on_frame_evicted(
                    *id,
                    "stale_or_out_of_window",
                    failed_groups,
                    wasted_bytes,
                    lost_chunks,
                    receive_us,
                );
            }
        }

        if !to_evict.is_empty() {
            log::info!(
                "[EVICT-BATCH] evicted {} frames min_frame_id={min_frame_id} \
                 live_frames={}",
                to_evict.len(),
                self.frames.len(),
            );
        }

        // ── Insert chunk ─────────────────────────────────────────────────────
        let is_new_frame = !self.frames.contains_key(&frame_id);
        let live = self.frames.len() + 1;
        {
            let builder = self.frames
                .entry(frame_id)
                .or_insert_with(|| {
                    let is_key = chunk.flags & 1 != 0;
                    log::debug!(
                        "[FRAME-CREATE] frame={frame_id} total_slices={ts} \
                            is_key={is_key} at={receive_us}µs  \
                            live_frames={live}",
                        ts   = chunk.total_slices,
                        live = live
                    );
                    self.diag.on_frame_created(
                        frame_id, chunk.total_slices, is_key, receive_us,
                    );
                    FrameBuilder::new(chunk.total_slices, is_key, receive_us)
                });

            let (recoveries, tasks, is_nack_recovery, newly_stalled) =
                builder.insert_chunk(chunk, self.chunk_counter);

            if is_nack_recovery {
                self.diag.on_nack_recovery(
                    frame_id, chunk.slice_idx, chunk.group_idx, chunk.shard_idx,
                );
            }

            if let Some((slice_idx, group_idx)) = newly_stalled {
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
                self.diag.on_nack_queued(frame_id, slice_idx, group_idx);
            }

            for recovery in recoveries {
                self.stats.note_group_recovery(recovery);
            }

            for task in tasks {
                let n = task.shards.iter().filter(|s| s.is_some()).count();
                self.diag.on_fec_submitted(
                    frame_id, task.slice_idx, task.group_idx,
                    task.k, task.m, n,
                );
                self.compute_pool.submit(task);
            }

            self.stats.note_chunk(chunk, is_nack_recovery, false);
        }

        if Some(frame_id) == self.next_to_push_id {
            self.hol_stall_since_us = None;
        }

        // Refresh the rolling-window snapshot for this frame.
        self.refresh_snap(frame_id, receive_us);

        // ── Per-chunk trace log ───────────────────────────────────────────────
        self.diag.on_shard_insert(
            frame_id,
            chunk.slice_idx,
            chunk.group_idx,
            chunk.shard_idx,
            chunk.k,
            chunk.m,
            chunk.payload_len,
            true, // "new" at assembler level (zombie guard above catches dupes)
            is_parity,
            0, 0, // received / mask retrieved from GroupBuilder inside the trace logs
            receive_us,
        );

        // ── Stats packet ──────────────────────────────────────────────────────
        let stats_packet = self.stats
            .maybe_log(receive_us, self.stats_every_us)
            .map(|bytes| ControlPacket::RecoveryStats {
                data: bytes::Bytes::copy_from_slice(bytes),
            });

        // ── Apply async FEC results ───────────────────────────────────────────
        let fec_results = self.compute_pool.drain_results();
        if !fec_results.is_empty() {
            log::debug!(
                "[FEC-DRAIN] {} results from compute pool", fec_results.len()
            );
        }
        for result in fec_results {
            let fid = result.frame_id;
            let ok  = result.data.is_some();
            let fec_latency = receive_us; // approx — we don't store submit time per-result here
            self.diag.on_fec_result(fid, result.slice_idx, ok, 0);

            if let Some(builder) = self.frames.get_mut(&fid) {
                if builder.state == FrameState::Receiving {
                    let completed = builder.apply_decode_result(
                        result.slice_idx,
                        result.data.unwrap_or_default(),
                    );
                    if completed {
                        log::info!(
                            "[FEC-COMPLETE] frame={fid} s={si} — \
                             frame reached Complete state",
                            si = result.slice_idx,
                        );
                    }
                    self.refresh_snap(fid, receive_us);
                }
            }
            let _ = fec_latency;
        }

        // ── Lazy NACK ─────────────────────────────────────────────────────────
        let nacks = self.drain_nacks(receive_us);

        // ── HOL flush ────────────────────────────────────────────────────────
        let mut output = Vec::new();
        self.flush_ordered(min_frame_id, receive_us, &mut output);

        // ── Periodic window dump ──────────────────────────────────────────────
        if self.chunk_counter % WINDOW_DUMP_EVERY_N_CHUNKS == 0 {
            self.diag.dump_window(
                "periodic",
                self.next_to_push_id,
                receive_us,
            );
            // Also dump every active frame's full per-group status at debug level.
            if log::log_enabled!(log::Level::Debug) {
                let now_us = FrameTrace::now_us();
                let ids: Vec<u64> = self.frames.keys().copied().collect();
                for fid in ids {
                    if let Some(fb) = self.frames.get(&fid) {
                        fb.log_full_status(fid, "periodic", now_us);
                    }
                }
            }
        }

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

            if let Some(mut builder) = self.frames.remove(&next_id) {
                if builder.is_complete() {
                    // ── Frame ready ───────────────────────────────────────────
                    let released_us = FrameTrace::now_us();
                    let ready_us    = builder.network_ready_us.unwrap_or_default();
                    let hol_wait    = released_us.saturating_sub(ready_us);

                    if let Some(mut packets) = builder.take_packets() {
                        for p in &mut packets {
                            if let Some(ref mut t) = p.trace {
                                t.network_ready_us   = ready_us;
                                t.hol_released_us    = released_us;
                                t.collecting_done_us = builder.collecting_done_us;
                                t.fec_submitted_us   = builder.fec_submitted_us;
                                t.fec_done_us        = builder.fec_done_us;
                            }
                        }
                        let n = packets.len();
                        log::info!(
                            "[HOL-RELEASE] frame={next_id} packets={n} \
                             hol_wait={hol_wait}µs ready_us={ready_us}µs \
                             released_us={released_us}µs",
                        );
                        self.diag.on_frame_released(next_id, n, hol_wait);
                        self.stats.note_frame_emitted(n);
                        output.extend(packets);
                    }
                    
                    let old_id = next_id;
                    self.next_to_push_id    = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                    self.diag.on_hol_advance(old_id, next_id + 1);

                } else {
                    // ── Frame incomplete: possibly stalling HOL ───────────────
                    let stall_start = *self.hol_stall_since_us
                        .get_or_insert_with(|| FrameTrace::now_us()); // ← реальное время, не receive_us
                    let stall_us = FrameTrace::now_us().saturating_sub(stall_start);

                    self.diag.on_hol_blocked(next_id, stall_us, STALE_FRAME_US);

                    if stall_us >= STALE_FRAME_US / 2 && log::log_enabled!(log::Level::Debug) {
                        builder.log_full_status(next_id, "hol-stall", now_us);
                    }
                    
                    if stall_us >= STALE_FRAME_US {
                        log::warn!("[HOL-TIMEOUT] frame={next_id} stall={stall_us}µs — emitting partial");
                        self.stats.note_frame_lost_timeout();

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
                            let n = partial.len();
                            log::warn!(
                                "[HOL-PARTIAL-RELEASE] frame={next_id} \
                                 partial_packets={n} stall={stall_us}µs"
                            );
                            self.stats.note_frame_emitted(n);
                            output.extend(partial);
                        }
                        else {
                            log::warn!(
                                "[HOL-ZERO-RELEASE] frame={next_id} \
                                 — zero recoverable packets, skipping"
                            );
                        }
                        
                        self.stats.note_partial_slicing();
                        self.next_to_push_id = Some(next_id + 1);
                        self.hol_stall_since_us = None;
                        self.diag.on_hol_advance(next_id, next_id + 1);
                    } else {
                        log::trace!(
                            "[HOL-WAIT] frame={next_id} stall={stall_us}µs / \
                             {STALE_FRAME_US}µs — still within budget"
                        );
                        self.frames.insert(next_id, builder);
                        break; 
                    }
                }
            } else {
                // ── No builder at all (zero packets received for this frame) ──
                let was_evicted   = self.last_evicted_id.map_or(false, |e| next_id <= e);
                let out_of_window = next_id < min_frame_id;

                if was_evicted || out_of_window {
                    log::debug!(
                        "[HOL-SKIP-LOST] frame={next_id} \
                         evicted={was_evicted} out_of_window={out_of_window}"
                    );
                    if was_evicted   { self.stats.note_frame_lost_evicted(); }
                    if out_of_window { self.stats.note_frame_lost_out_of_window(); }
                    let old_id = next_id;
                    self.next_to_push_id    = Some(next_id + 1);
                    self.hol_stall_since_us = None;
                    self.diag.on_hol_advance(old_id, next_id + 1);
                } else {
                    let stall_start = *self.hol_stall_since_us
                        .get_or_insert_with(|| FrameTrace::now_us()); // ← реальное время, не receive_us
                    let stall_us = FrameTrace::now_us().saturating_sub(stall_start);

                    self.diag.on_hol_blocked(next_id, stall_us, STALE_FRAME_US);

                    if stall_us >= STALE_FRAME_US {
                        log::warn!(
                            "[HOL-ABSENT-TIMEOUT] frame={next_id} \
                             stall={stall_us}µs — no builder, skipping (total loss)"
                        );
                        self.stats.note_frame_lost_timeout();
                        let old_id = next_id;
                        self.next_to_push_id    = Some(next_id + 1);
                        self.hol_stall_since_us = None;
                        self.diag.on_hol_advance(old_id, next_id + 1);
                    } else {
                        log::trace!(
                            "[HOL-ABSENT-WAIT] frame={next_id} stall={stall_us}µs \
                             — no builder yet, waiting"
                        );
                        break;
                    }
                }
            }
        }

        // Log HOL state summary at trace level every cycle.
        log::trace!(
            "[HOL-STATE] next={:?} stall_since={:?} live_frames={}",
            self.next_to_push_id,
            self.hol_stall_since_us,
            self.frames.len(),
        );
    }

    // ── Lazy NACK ────────────────────────────────────────────────────────────

    fn drain_nacks(&mut self, now_us: u64) -> Vec<ControlPacket> {
        let mut batches: HashMap<u64, Vec<NackEntry>> = HashMap::new();
        let initial_len = self.nack_queue.len();

        log::trace!(
            "[NACK-DRAIN] queue_len={initial_len} active_frames={}",
            self.frames.len(),
        );

        for _ in 0..initial_len {
            let (frame_id, slice_idx, group_idx) = self.nack_queue.pop_front().unwrap();

            let frame_batch = batches.entry(frame_id).or_default();
            if frame_batch.len() >= NACK_BATCH_SIZE as usize {
                log::debug!(
                    "[NACK-BATCH-FULL] frame={frame_id} — \
                     batch full ({NACK_BATCH_SIZE}), deferring s={slice_idx} g={group_idx}"
                );
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
                continue;
            }

            let mut keep_in_queue = false;
            let mut needs_nack    = false;
            let mut mask          = 0u64;
            let mut k_val         = 0u8;
            let mut m_val         = 0u8;

            if let Some(builder) = self.frames.get_mut(&frame_id) {
                if let Some(sb) = builder.slices.get_mut(&slice_idx) {
                    if sb.state == SliceState::Assembling {
                        if let Some(gb) = sb.groups.get_mut(&group_idx) {
                            k_val = gb.k;
                            m_val = gb.m;
                            mask  = gb.received_mask();

                            log::trace!(
                                "[NACK-CHECK] frame={frame_id} s={slice_idx} g={group_idx} \
                                 state={st} received={rx}/{tot} \
                                 needs_nack={need}",
                                st  = group_state_label(&gb.state),
                                rx  = gb.received,
                                tot = gb.k + gb.m,
                                need = gb.state.needs_nack(now_us),
                            );

                            if gb.state.needs_nack(now_us) {
                                needs_nack = true;
                                gb.state   = GroupState::Requested { sent_at_us: now_us };
                                keep_in_queue = true;

                                self.diag.on_nack_sent(
                                    frame_id, slice_idx, group_idx, mask, k_val, m_val,
                                );
                            } else if matches!(
                                gb.state,
                                GroupState::Stalled { .. } | GroupState::Requested { .. }
                            ) {
                                keep_in_queue = true;
                                log::trace!(
                                    "[NACK-SUPPRESS] frame={frame_id} s={slice_idx} \
                                     g={group_idx} state={st} — suppressing, re-queuing",
                                    st = group_state_label(&gb.state),
                                );
                            }
                        }
                    } else {
                        log::trace!(
                            "[NACK-SKIP-SLICE] frame={frame_id} s={slice_idx} \
                             slice_state={st} — not Assembling, dropping NACK entry",
                            st = slice_state_label(&sb.state),
                        );
                    }
                } else {
                    log::trace!(
                        "[NACK-SKIP-NO-SLICE] frame={frame_id} s={slice_idx} \
                         — SliceBuilder not found"
                    );
                }
            } else {
                log::trace!(
                    "[NACK-SKIP-NO-FRAME] frame={frame_id} — \
                     FrameBuilder not found (evicted?)"
                );
            }

            if needs_nack {
                log::info!(
                    "[NACK-FIRE] frame={frame_id} s={slice_idx} g={group_idx} \
                     k={k_val} m={m_val} mask=0x{mask:016x}"
                );
                frame_batch.push(NackEntry { slice_idx, group_idx, received_mask: mask });
            }

            if keep_in_queue {
                self.nack_queue.push_back((frame_id, slice_idx, group_idx));
            }
        }

        let mut result = Vec::with_capacity(batches.len());
        for (frame_id, entries) in batches {
            if !entries.is_empty() {
                log::debug!(
                    "[NACK-PACKET] frame={frame_id} entries={n}",
                    n = entries.len(),
                );
                self.stats.note_nack();
                result.push(ControlPacket::NackBatch { frame_id, entries });
            }
        }

        if !result.is_empty() {
            log::info!(
                "[NACK-BATCH-SENT] {} NackBatch packets  nack_queue_remaining={}",
                result.len(),
                self.nack_queue.len(),
            );
        }

        result
    }

    // ── poll() ───────────────────────────────────────────────────────────────

    pub fn poll(&mut self) -> (Vec<VideoPacket>, Vec<ControlPacket>) {
        let now_us = FrameTrace::now_us();
        let min_frame_id = self.next_to_push_id
            .unwrap_or(0)
            .saturating_sub(MAX_BUFFERED_FRAMES);

        log::trace!(
            "[POLL] now={now_us}µs HOL={:?} live_frames={} nack_q={}",
            self.next_to_push_id,
            self.frames.len(),
            self.nack_queue.len(),
        );

        // Drain FEC results
        let fec_results = self.compute_pool.drain_results();
        if !fec_results.is_empty() {
            log::debug!("[POLL-FEC-DRAIN] {} results", fec_results.len());
        }
        for result in fec_results {
            let fid = result.frame_id;
            let ok  = result.data.is_some();
            self.diag.on_fec_result(fid, result.slice_idx, ok, 0);

            if let Some(builder) = self.frames.get_mut(&fid) {
                if builder.state == FrameState::Receiving {
                    builder.apply_decode_result(
                        result.slice_idx,
                        result.data.unwrap_or_default(),
                    );
                    self.refresh_snap(fid, now_us);
                }
            }
        }

        let mut output = Vec::new();
        self.flush_ordered(min_frame_id, now_us, &mut output);

        let nacks = self.drain_nacks(now_us);

        if !output.is_empty() || !nacks.is_empty() {
            log::debug!(
                "[POLL-RESULT] output_packets={} nack_packets={}",
                output.len(), nacks.len(),
            );
        }

        (output, nacks)
    }

    // ── Diagnostic helpers ───────────────────────────────────────────────────

    /// Force a full window dump right now regardless of chunk count.
    pub fn dump_diagnostics(&mut self) {
        let now_us = FrameTrace::now_us();
        // Refresh every live frame before dumping.
        let ids: Vec<u64> = self.frames.keys().copied().collect();
        for fid in ids {
            self.refresh_snap(fid, now_us);
        }
        self.diag.dump_window("manual", self.next_to_push_id, now_us);

        // Also emit full per-frame status at debug level.
        let ids2: Vec<u64> = self.frames.keys().copied().collect();
        for fid in ids2 {
            if let Some(fb) = self.frames.get(&fid) {
                fb.log_full_status(fid, "manual", now_us);
            }
        }

        log::info!(
            "[DIAG-SUMMARY] HOL={:?} live_frames={} nack_q={} \
             last_evicted={:?} chunk_count={}",
            self.next_to_push_id,
            self.frames.len(),
            self.nack_queue.len(),
            self.last_evicted_id,
            self.chunk_counter,
        );
    }
}