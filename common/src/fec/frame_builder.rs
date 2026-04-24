use crate::{DatagramChunk, FrameTrace, VideoPacket, VideoSlice, fec::{decode_task::DecodeTask, group_builder::{GroupBuilder, GroupState}, slice_builder::{SliceBuilder, SliceState}}};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameState {
    /// Still assembling slices.
    Receiving,
    /// Every slice has been decoded and staged into `ready_packets`.
    /// The HOL loop will drain `ready_packets` and move the frame forward.
    Complete,
    /// Frame was evicted or timed out before all slices arrived.
    Abandoned,
}

pub(crate) struct FrameBuilder {
    pub(crate) slices:      HashMap<u8, SliceBuilder>,
    decoded_slices:         Vec<Option<VideoSlice>>,
    pub(crate) total_slices: u8,
    pub(crate) first_us:    u64,
    /// Points to the lowest slice index not yet staged into `ready_packets`.
    /// Advances only when `decoded_slices[next_emit_idx]` is `Some`.
    next_emit_idx:          u8,
    /// Decoded packets waiting for the HOL flush loop to release them.
    ready_packets:          Vec<VideoPacket>,
    pub(crate) state:       FrameState,
    pub(crate) network_ready_us: Option<u64>,
    pub(crate) collecting_done_us: u64,
    pub(crate) fec_submitted_us: u64,
    pub(crate) fec_done_us: u64,
}

impl FrameBuilder {
    pub(crate) fn new(total_slices: u8, _is_key: bool, receive_us: u64) -> Self {
        let mut decoded_slices = Vec::with_capacity(total_slices as usize);
        decoded_slices.resize_with(total_slices as usize, || None);

        Self {
            slices: HashMap::new(),
            decoded_slices,
            total_slices,
            first_us: receive_us,
            next_emit_idx: 0,
            ready_packets: Vec::new(),
            state: FrameState::Receiving,
            network_ready_us: None,
            collecting_done_us: 0,
            fec_submitted_us: 0,
            fec_done_us: 0,

        }
    }

    // ── Public state queries ─────────────────────────────────────────────────

    /// `true` once every slice has been decoded and staged into `ready_packets`.
    pub(crate) fn is_complete(&self) -> bool {
        self.state == FrameState::Complete
    }

    // ── Accounting ───────────────────────────────────────────────────────────

    /// Returns (failed_groups, wasted_payload_bytes, lost_chunks) for groups
    /// that were never successfully reconstructed.
    pub(crate) fn count_wasted(&self) -> (u64, u64, u64) {
        let mut failed_groups        = 0u64;
        let mut wasted_payload_bytes = 0u64;
        let mut lost_chunks          = 0u64;

        for sb in self.slices.values() {
            for gb in sb.groups.values() {
                // Only count groups that are NOT terminal-success.
                // Decoding is still in flight — treat it as failed for wasted
                // accounting (frame is being evicted so the result is moot).
                if !matches!(gb.state, GroupState::Ready) {
                    failed_groups += 1;
                    let total_expected = (gb.k + gb.m) as u64;
                    lost_chunks += total_expected.saturating_sub(gb.received as u64);

                    // Bytes we received but can never use.
                    for i in 0..(gb.k as usize) {
                        if gb.data_shards[i].is_some() {   // ← было gb.shards[i]
                            wasted_payload_bytes += gb.payload_lens[i] as u64;
                        }
                    }
                }
            }
        }

        (failed_groups, wasted_payload_bytes, lost_chunks)
    }

    // ── Core insertion ───────────────────────────────────────────────────────

    /// Insert a chunk and potentially advance the frame state machine.
    ///
    /// Returns:
    /// - `Vec<GroupRecovery>` — recovery events for stats accounting.
    /// - `Vec<DecodeTask>`    — FEC tasks to submit to `ComputePool` (empty on
    ///                          fast path).
    /// - `bool`               — `true` if this chunk was a NACK-triggered
    ///                          retransmission.
    /// - `Option<(u8, u8)>`   — `(slice_idx, group_idx)` of a group that just
    ///                          became `Stalled` and needs to enter the NACK
    ///                          queue.
    pub(crate) fn insert_chunk(
        &mut self,
        chunk: &DatagramChunk,
    ) -> (Vec<GroupRecovery>, Vec<DecodeTask>, bool, Option<(u8, u8)>) {
        // ── Guard: terminal frame states ─────────────────────────────────────
        // Once a frame is Complete or Abandoned we still allow inserts so that
        // zombie/late chunks don't create new SliceBuilders, but we return early
        // with empty outputs — the slice is either already emitted or dead.
        if self.state != FrameState::Receiving {
            return (Vec::new(), Vec::new(), false, None);
        }

        let slice_idx = chunk.slice_idx as usize;
        if slice_idx >= self.decoded_slices.len() {
            return (Vec::new(), Vec::new(), false, None);
        }

        // ── Guard: slice already decoded ─────────────────────────────────────
        if self.decoded_slices[slice_idx].is_some() {
            // Slice is done; this is a duplicate or late parity shard.
            return (Vec::new(), Vec::new(), false, None);
        }

        // ── Delegate to SliceBuilder ─────────────────────────────────────────
        let mut recoveries = Vec::new();
        let mut tasks:  Vec<DecodeTask> = Vec::new();

        let (slice_state_after, is_nack_recovery, newly_stalled, collecting_done) = {
            let sb = self.slices
                .entry(chunk.slice_idx)
                .or_insert_with(|| SliceBuilder::new(chunk.total_groups));

            // Guard: slice already in a terminal state (Emitted/Abandoned).
            if sb.state.is_terminal() {
                return (Vec::new(), Vec::new(), false, None);
            }

            let (maybe_recovery, is_nack_recovery, stalled_coords, collecting_done) = sb.insert(chunk);

            if let Some(recovery) = maybe_recovery {
                recoveries.push(recovery);
            }

            let slice_state = sb.state.clone();
            (slice_state, is_nack_recovery, stalled_coords, collecting_done)
        };

        // ── Slice state transitions ──────────────────────────────────────────
        match slice_state_after {
            // Not all groups are ready yet — nothing more to do this tick.
            SliceState::Assembling => {}

            // All groups ready; decide fast vs slow path.
            SliceState::EmittingFec => {
                // Collect every group that still needs RS reconstruction.
                // We iterate all groups here — not just chunk.group_idx — because
                // the shard that pushed the slice over the threshold may belong to
                // a group that was already data-complete, while a different group
                // still has holes.
                let sb_mut = self.slices.get_mut(&chunk.slice_idx).unwrap();
                for (g_idx, gb) in sb_mut.groups.iter_mut() {
                    if gb.state == GroupState::Decoding {
                        if let Some(task) = Self::make_decode_task_from(
                            chunk.frame_id, chunk.slice_idx, *g_idx, gb,
                        ) {
                            tasks.push(task);
                        }
                    }
                }
                if !tasks.is_empty() && self.fec_submitted_us == 0 {
                    self.fec_submitted_us = FrameTrace::now_us();
                }
            }

            SliceState::EmittingDirect => {
                // All data shards present — synchronous fast path.
                if let Some(video_slice) = self.decode_slice_sync(chunk.slice_idx) {
                    self.decoded_slices[slice_idx] = Some(video_slice);

                    // Mark the slice as Emitted so late-arriving shards skip it.
                    if let Some(sb) = self.slices.get_mut(&chunk.slice_idx) {
                        sb.state = SliceState::Emitted;
                    }

                    let new_packets = self.try_emit_ordered();
                    self.ready_packets.extend(new_packets);

                    self.try_advance_frame_state();
                    if self.is_complete() {
                        self.mark_ready();
                    }
                }
            }

            // These shouldn't be returned by SliceBuilder::insert on a live
            // slice, but handle them defensively.
            SliceState::Emitted | SliceState::Abandoned => {}
        }

        if let Some(done_us) = collecting_done {
            if self.collecting_done_us == 0 {
                self.collecting_done_us = done_us;
            }
        }
        (recoveries, tasks, is_nack_recovery, newly_stalled)
    }

    // ── Async FEC result integration ─────────────────────────────────────────

    /// Accept the result of an async RS decode from `ComputePool`.
    ///
    /// Returns `true` if the frame became `Complete` after this call.
    pub(crate) fn apply_decode_result(&mut self, slice_idx: u8, _data: Vec<u8>) -> bool {
        let idx = slice_idx as usize;

        // Guard: frame already done, or this slice already decoded.
        if self.state != FrameState::Receiving {
            return self.is_complete();
        }
        if idx >= self.decoded_slices.len() || self.decoded_slices[idx].is_some() {
            return self.is_complete();
        }

        // Guard: slice state must still be EmittingFec.
        match self.slices.get(&slice_idx).map(|sb| &sb.state) {
            Some(SliceState::EmittingFec) => {}
            _ => return self.is_complete(),
        }

        // IMPORTANT: do NOT deserialize directly from `_data`.
        //
        // `_data` is the RS output for ONE group only. For a multi-group slice
        // we must concatenate every group's decoded bytes before deserializing.
        // `decode_slice_sync` does exactly that — it calls FecDecoder::decode
        // per group (cheap fast-path for data-complete groups; re-runs RS for
        // the recovered ones) and deserializes the complete VideoSlice.
        //
        // Consequence for multi-group FEC slices: when N groups need RS and N
        // tasks are submitted, the first result to arrive triggers decode_slice_sync
        // and emits the slice.  Subsequent results hit the
        // `decoded_slices[idx].is_some()` guard above and short-circuit cleanly.
        if self.fec_done_us == 0 {
            self.fec_done_us = FrameTrace::now_us();
        }
        if let Some(video_slice) = self.decode_slice_sync(slice_idx) {
            self.decoded_slices[idx] = Some(video_slice);

            if let Some(sb) = self.slices.get_mut(&slice_idx) {
                sb.state = SliceState::Emitted;
            }

            let new_packets = self.try_emit_ordered();
            self.ready_packets.extend(new_packets);

            self.try_advance_frame_state();

            if self.is_complete() {
                self.mark_ready();
            }
        }

        self.is_complete()
    }

    // ── Packet drain ─────────────────────────────────────────────────────────

    /// Drain all staged packets.  Returns `Some` only when `is_complete()`.
    pub(crate) fn take_packets(&mut self) -> Option<Vec<VideoPacket>> {
        if self.is_complete() {
            Some(std::mem::take(&mut self.ready_packets))
        } else {
            None
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Advance `next_emit_idx` as long as `decoded_slices[idx]` is filled,
    /// converting each `VideoSlice` into a `VideoPacket`.
    fn try_emit_ordered(&mut self) -> Vec<VideoPacket> {
        let mut packets = Vec::new();

        while self.next_emit_idx < self.total_slices {
            let idx = self.next_emit_idx as usize;
            match self.decoded_slices[idx].take() {
                Some(slice) => {
                    packets.push(Self::slice_to_packet(slice, self.first_us));
                    self.next_emit_idx += 1;
                }
                None => break,
            }
        }

        packets
    }

    /// Transition the frame to `Complete` once all slices have been emitted.
    fn try_advance_frame_state(&mut self) {
        if self.state == FrameState::Receiving && self.next_emit_idx == self.total_slices {
            self.state = FrameState::Complete;
        }
    }

    fn decode_slice_sync(&self, slice_idx: u8) -> Option<VideoSlice> {
        let sb = self.slices.get(&slice_idx)?;
        let data = sb.get_data_sync()?;
        let (video_slice, _): (VideoSlice, _) = postcard::take_from_bytes(&data).ok()?;
        Some(video_slice)
    }

    fn make_decode_task_from(
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
        gb:        &GroupBuilder,
    ) -> Option<DecodeTask> {
        Some(DecodeTask {
            frame_id,
            slice_idx,
            group_idx,
            k:            gb.k,
            m:            gb.m,
            shards:       gb.as_rs_shards(),
            payload_lens: gb.payload_lens.clone(),
        })
    }

    pub(crate) fn take_partial_packets(&mut self, frame_id: u64) -> Vec<VideoPacket> {
        // 1. Считаем, сколько было до и сколько добавилось
        let before_count = self.ready_packets.len();
        let new_packets = self.try_emit_ordered();
        let added_count = new_packets.len();
        
        self.ready_packets.extend(new_packets);
        let total_ready = self.ready_packets.len();

        // 2. Логируем попытку сбора ошметков
        if total_ready == 0 {
            log::warn!(
                "[Frame Builder] Abandoing frame #{} - NO packets available for concealment", 
                frame_id
            );
        } else {
            log::info!(
                "[Frame Builder] Partial emit for frame #{}: collected {} packets ({} new, {} from buffer)",
                frame_id, 
                total_ready,
                added_count,
                before_count
            );
        }

        if let Some(last) = self.ready_packets.last_mut() {
            // Логируем, на каком именно слайсе мы «сломались»
            log::debug!(
                "[Frame Builder] Forcing last flag on slice index {} for frame #{}", 
                last.slice_idx, 
                frame_id
            );
            last.is_last = true;
            last.is_concealed = true;
        }

        self.state = FrameState::Abandoned;
        
        let packets = std::mem::take(&mut self.ready_packets);
        
        // 3. Финальный лог результата
        log::trace!("[Frame Builder] Frame #{} marked as Abandoned, returning control", frame_id);
        
        packets
    }

    fn slice_to_packet(slice: VideoSlice, first_us: u64) -> VideoPacket {
        let mut trace = slice.trace;
        if let Some(ref mut t) = trace {
            t.receive_us    = first_us;
        }
        VideoPacket {
            frame_id:  slice.frame_id,
            payload:   slice.payload,
            slice_idx: slice.slice_idx,
            is_key:    slice.is_key,
            is_last:   slice.is_last,
            is_concealed: false,
            trace,
        }
    }
    pub fn mark_ready(&mut self) {
        if self.network_ready_us.is_none() {
            self.network_ready_us = Some(FrameTrace::now_us());
        }
    }
}