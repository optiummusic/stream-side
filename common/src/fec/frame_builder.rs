use crate::{DatagramChunk, FrameTrace, VideoPacket, VideoSlice, fec::{
    decode_task::DecodeTask,
    group_builder::{GroupBuilder, GroupState},
    slice_builder::{SliceBuilder, SliceState},
    assembly_log::{frame_state_label, slice_state_label, group_state_label},
}};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameState {
    /// Still assembling slices.
    Receiving,
    /// Every slice has been decoded and staged into `ready_packets`.
    Complete,
    /// Frame was evicted or timed out before all slices arrived.
    Abandoned,
}

pub(crate) struct FrameBuilder {
    pub(crate) slices:       HashMap<u8, SliceBuilder>,
    pub(crate) decoded_slices:          Vec<Option<VideoSlice>>,
    pub(crate) total_slices: u8,
    pub(crate) first_us:     u64,
    pub last_us: u64,
    pub(crate) next_emit_idx:           u8,
    pub(crate) ready_packets:           Vec<VideoPacket>,
    pub(crate) state:        FrameState,
    pub(crate) network_ready_us:    Option<u64>,
    pub(crate) collecting_done_us:  u64,
    pub(crate) fec_submitted_us:    u64,
    pub(crate) fec_done_us:         u64,
}

impl FrameBuilder {
    pub(crate) fn new(total_slices: u8, is_key: bool, receive_us: u64) -> Self {
        log::debug!(
            "[FRAME-NEW] total_slices={total_slices} is_key={is_key} \
             created_at={receive_us}µs"
        );

        let mut decoded_slices = Vec::with_capacity(total_slices as usize);
        decoded_slices.resize_with(total_slices as usize, || None);

        Self {
            slices: HashMap::new(),
            decoded_slices,
            total_slices,
            first_us: receive_us,
            last_us: 0,
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

    pub(crate) fn is_complete(&self) -> bool {
        self.state == FrameState::Complete
    }

    // ── Accounting ───────────────────────────────────────────────────────────

    pub(crate) fn count_wasted(&self) -> (u64, u64, u64) {
        let mut failed_groups        = 0u64;
        let mut wasted_payload_bytes = 0u64;
        let mut lost_chunks          = 0u64;

        for sb in self.slices.values() {
            for gb in sb.groups.values() {
                if !matches!(gb.state, GroupState::Ready) {
                    failed_groups += 1;
                    let total_expected = (gb.k + gb.m) as u64;
                    let lost = total_expected.saturating_sub(gb.received as u64);
                    lost_chunks += lost;

                    let mut wasted = 0u64;
                    for i in 0..(gb.k as usize) {
                        if gb.data_shards[i].is_some() {
                            wasted += gb.payload_lens[i] as u64;
                        }
                    }
                    wasted_payload_bytes += wasted;

                    log::debug!(
                        "[WASTED-GROUP] state={st} k={k} m={m} \
                         received={rx} lost={lost} wasted_bytes={wasted}",
                        st    = group_state_label(&gb.state),
                        k     = gb.k,
                        m     = gb.m,
                        rx    = gb.received,
                    );
                }
            }
        }

        if failed_groups > 0 {
            log::warn!(
                "[FRAME-WASTED] failed_groups={failed_groups} \
                 wasted_bytes={wasted_payload_bytes}B lost_chunks={lost_chunks}"
            );
        }

        (failed_groups, wasted_payload_bytes, lost_chunks)
    }

    // ── Core insertion ───────────────────────────────────────────────────────

    pub(crate) fn insert_chunk(
        &mut self,
        chunk: &DatagramChunk,
        global_seq: u64,
    ) -> (Vec<GroupRecovery>, Vec<DecodeTask>, bool, Option<(u8, u8)>) {
        // ── Guard: terminal frame states ─────────────────────────────────────
        if self.state != FrameState::Receiving {
            log::trace!(
                "[FRAME-INSERT-SKIP] frame={fid} s={si} g={gi} shard={sh} \
                 — frame state={st}, skipping",
                fid = chunk.frame_id,
                si  = chunk.slice_idx,
                gi  = chunk.group_idx,
                sh  = chunk.shard_idx,
                st  = frame_state_label(&self.state),
            );
            return (Vec::new(), Vec::new(), false, None);
        }

        let slice_idx = chunk.slice_idx as usize;
        if slice_idx >= self.decoded_slices.len() {
            log::error!(
                "[FRAME-INSERT-OOB] frame={fid} slice_idx={si} \
                 total_slices={ts} — out of bounds, dropping",
                fid = chunk.frame_id,
                si  = chunk.slice_idx,
                ts  = self.total_slices,
            );
            return (Vec::new(), Vec::new(), false, None);
        }

        // ── Guard: slice already decoded ─────────────────────────────────────
        if self.decoded_slices[slice_idx].is_some() {
            log::trace!(
                "[FRAME-INSERT-SKIP] frame={fid} s={si} g={gi} shard={sh} \
                 — slice already decoded, dropping late shard",
                fid = chunk.frame_id,
                si  = chunk.slice_idx,
                gi  = chunk.group_idx,
                sh  = chunk.shard_idx,
            );
            return (Vec::new(), Vec::new(), false, None);
        }

        // ── Log frame-level progress before insert ────────────────────────────
        let age_us = FrameTrace::now_us().saturating_sub(self.first_us);
        log::trace!(
            "[FRAME-INSERT] frame={fid} s={si} g={gi} shard={sh} \
             frame_state={st} next_emit={ne}/{ts} age={age}µs",
            fid = chunk.frame_id,
            si  = chunk.slice_idx,
            gi  = chunk.group_idx,
            sh  = chunk.shard_idx,
            st  = frame_state_label(&self.state),
            ne  = self.next_emit_idx,
            ts  = self.total_slices,
            age = age_us,
        );

        // ── Delegate to SliceBuilder ─────────────────────────────────────────
        self.last_us = FrameTrace::now_us();
        let mut recoveries = Vec::new();
        let mut tasks: Vec<DecodeTask> = Vec::new();

        let (slice_state_after, is_nack_recovery, newly_stalled, collecting_done) = {
            let sb = self.slices
                .entry(chunk.slice_idx)
                .or_insert_with(|| {
                    log::debug!(
                        "[SLICE-NEW] frame={fid} s={si} total_groups={tg}",
                        fid = chunk.frame_id,
                        si  = chunk.slice_idx,
                        tg  = chunk.total_groups,
                    );
                    SliceBuilder::new(chunk.total_groups)
                });

            if sb.state.is_terminal() {
                log::trace!(
                    "[SLICE-INSERT-SKIP] frame={fid} s={si} state={st} \
                     — terminal slice, skipping",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    st  = slice_state_label(&sb.state),
                );
                return (Vec::new(), Vec::new(), false, None);
            }

            let (maybe_recovery, is_nack_recovery, stalled_coords, collecting_done) =
                sb.insert(chunk, global_seq);

            if let Some(recovery) = maybe_recovery {
                recoveries.push(recovery);
            }

            let slice_state = sb.state.clone();
            (slice_state, is_nack_recovery, stalled_coords, collecting_done)
        };

        if is_nack_recovery {
            log::info!(
                "[NACK-RECOVERED] frame={fid} s={si} g={gi} shard={sh}",
                fid = chunk.frame_id,
                si  = chunk.slice_idx,
                gi  = chunk.group_idx,
                sh  = chunk.shard_idx,
            );
        }

        // ── Slice state transitions ──────────────────────────────────────────
        match slice_state_after {
            SliceState::Assembling => {
                log::trace!(
                    "[SLICE-STILL-ASSEMBLING] frame={fid} s={si}",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                );
            }

            SliceState::EmittingFec => {
                let sb_mut = self.slices.get_mut(&chunk.slice_idx).unwrap();
                let mut fec_group_count = 0u8;
                for (g_idx, gb) in sb_mut.groups.iter_mut() {
                    if gb.state == GroupState::Decoding {
                        gb.state = GroupState::DecodingInProgress;
                        if let Some(task) = Self::make_decode_task_from(
                            chunk.frame_id, chunk.slice_idx, *g_idx, gb,
                        ) {
                            let n_present = task.shards.iter().filter(|s| s.is_some()).count();
                            log::debug!(
                                "[FEC-TASK-SUBMIT] frame={fid} s={si} g={gi} \
                                 k={k} m={m} shards_present={np}/{tot} — \
                                 dispatching to rayon",
                                fid = chunk.frame_id,
                                si  = chunk.slice_idx,
                                gi  = g_idx,
                                k   = task.k,
                                m   = task.m,
                                np  = n_present,
                                tot = task.k + task.m,
                            );
                            tasks.push(task);
                            fec_group_count += 1;
                        }
                    }
                }
                if !tasks.is_empty() && self.fec_submitted_us == 0 {
                    self.fec_submitted_us = FrameTrace::now_us();
                    log::info!(
                        "[FEC-SUBMITTED] frame={fid} s={si} tasks={fec_group_count} \
                         at={ts}µs frame_age={age}µs",
                        fid = chunk.frame_id,
                        si  = chunk.slice_idx,
                        ts  = self.fec_submitted_us,
                        age = self.fec_submitted_us.saturating_sub(self.first_us),
                    );
                }
            }

            SliceState::EmittingDirect => {
                log::debug!(
                    "[SLICE-DECODE-DIRECT] frame={fid} s={si} — \
                     all data shards present, decoding synchronously",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                );
                if let Some(video_slice) = self.decode_slice_sync(chunk.slice_idx) {
                    log::debug!(
                        "[SLICE-DECODED] frame={fid} s={si} — \
                         VideoSlice decoded successfully via direct path",
                        fid = chunk.frame_id,
                        si  = chunk.slice_idx,
                    );
                    self.decoded_slices[slice_idx] = Some(video_slice);

                    if let Some(sb) = self.slices.get_mut(&chunk.slice_idx) {
                        sb.state = SliceState::Emitted;
                        log::debug!(
                            "[SLICE→EMITTED] frame={fid} s={si} via direct",
                            fid = chunk.frame_id,
                            si  = chunk.slice_idx,
                        );
                    }

                    let before_emit = self.next_emit_idx;
                    let new_packets = self.try_emit_ordered();
                    if !new_packets.is_empty() {
                        log::debug!(
                            "[FRAME-PACKETS-STAGED] frame={fid} +{n} packets \
                             next_emit {before}→{after}",
                            fid    = chunk.frame_id,
                            n      = new_packets.len(),
                            before = before_emit,
                            after  = self.next_emit_idx,
                        );
                    }
                    self.ready_packets.extend(new_packets);

                    self.try_advance_frame_state();
                    if self.is_complete() {
                        log::info!(
                            "[FRAME→COMPLETE] frame={fid} slices={ts} \
                             total_age={age}µs via direct path, spread between shards={spread}ms",
                            fid = chunk.frame_id,
                            ts  = self.total_slices,
                            age = FrameTrace::now_us().saturating_sub(self.first_us),
                            spread = (self.last_us.saturating_sub(self.first_us)) / 1000,
                        );
                        self.mark_ready();
                    }
                } else {
                    log::error!(
                        "[SLICE-DECODE-FAIL] frame={fid} s={si} — \
                         decode_slice_sync returned None on direct path",
                        fid = chunk.frame_id,
                        si  = chunk.slice_idx,
                    );
                }
            }

            SliceState::Emitted | SliceState::Abandoned => {
                log::trace!(
                    "[SLICE-STATE-UNEXPECTED] frame={fid} s={si} state={st} \
                     returned from SliceBuilder::insert — handled defensively",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    st  = slice_state_label(&slice_state_after),
                );
            }
        }

        if let Some(done_us) = collecting_done {
            if self.collecting_done_us == 0 {
                self.collecting_done_us = done_us;
                log::debug!(
                    "[FRAME-COLLECTING-DONE] frame={fid} \
                     collecting_done={done_us}µs frame_age={age}µs",
                    fid = chunk.frame_id,
                    age = done_us.saturating_sub(self.first_us),
                );
            }
        }

        (recoveries, tasks, is_nack_recovery, newly_stalled)
    }

    // ── Async FEC result integration ─────────────────────────────────────────

    pub(crate) fn apply_decode_result(&mut self, slice_idx: u8, _data: Vec<u8>) -> bool {
        let idx = slice_idx as usize;

        if self.state != FrameState::Receiving {
            log::trace!(
                "[FEC-RESULT-SKIP] s={slice_idx} frame state={st} — not Receiving",
                st = frame_state_label(&self.state),
            );
            return self.is_complete();
        }
        if idx >= self.decoded_slices.len() || self.decoded_slices[idx].is_some() {
            log::trace!(
                "[FEC-RESULT-SKIP] s={slice_idx} — slice already decoded or OOB"
            );
            return self.is_complete();
        }

        match self.slices.get(&slice_idx).map(|sb| &sb.state) {
            Some(SliceState::EmittingFec) => {}
            Some(other) => {
                log::warn!(
                    "[FEC-RESULT-WRONG-STATE] s={slice_idx} state={st} \
                     — expected EmittingFec, ignoring result",
                    st = slice_state_label(other),
                );
                return self.is_complete();
            }
            None => {
                log::warn!(
                    "[FEC-RESULT-NO-SLICE] s={slice_idx} — SliceBuilder not found"
                );
                return self.is_complete();
            }
        }

        if self.fec_done_us == 0 {
            self.fec_done_us = FrameTrace::now_us();
            let latency = self.fec_done_us.saturating_sub(self.fec_submitted_us);
            log::info!(
                "[FEC-DONE] s={slice_idx} fec_done={ts}µs \
                 latency={latency}µs frame_age={age}µs",
                ts  = self.fec_done_us,
                age = self.fec_done_us.saturating_sub(self.first_us),
            );
        }

        log::debug!(
            "[FEC-APPLY] s={slice_idx} — calling decode_slice_sync"
        );

        if let Some(video_slice) = self.decode_slice_sync(slice_idx) {
            log::info!(
                "[SLICE-DECODED-FEC] s={slice_idx} — VideoSlice decoded via FEC path"
            );
            self.decoded_slices[idx] = Some(video_slice);

            if let Some(sb) = self.slices.get_mut(&slice_idx) {
                sb.state = SliceState::Emitted;
                log::debug!(
                    "[SLICE→EMITTED] s={slice_idx} via FEC"
                );
            }

            let before_emit = self.next_emit_idx;
            let new_packets = self.try_emit_ordered();
            if !new_packets.is_empty() {
                log::debug!(
                    "[FRAME-PACKETS-STAGED] +{n} packets \
                     next_emit {before}→{after}",
                    n      = new_packets.len(),
                    before = before_emit,
                    after  = self.next_emit_idx,
                );
            }
            self.ready_packets.extend(new_packets);
            self.try_advance_frame_state();

            if self.is_complete() {
                let now_us = FrameTrace::now_us();
                let total_age = now_us.saturating_sub(self.first_us);
                let collect_ms = self.collecting_done_us.saturating_sub(self.first_us) as f32 / 1000.0;
                let fec_dur_ms = self.fec_done_us.saturating_sub(self.fec_submitted_us) as f32 / 1000.0;
                log::info!(
                    "[FRAME→COMPLETE] slices={ts} total_age={total_age}µs \
                     collect={collect_ms:.2}ms fec_dur={fec_dur_ms:.2}ms via FEC, shard_spread={spread}ms via FEC",
                    ts = self.total_slices,
                    spread = (self.last_us.saturating_sub(self.first_us)) / 1000,
                );
                self.mark_ready();
            }
        } else {
            log::error!(
                "[FEC-APPLY-FAIL] s={slice_idx} — decode_slice_sync returned None \
                 (RS output may be corrupt or all shards exhausted)"
            );
        }

        self.is_complete()
    }

    // ── Packet drain ─────────────────────────────────────────────────────────

    pub(crate) fn take_packets(&mut self) -> Option<Vec<VideoPacket>> {
        if self.is_complete() {
            let n = self.ready_packets.len();
            log::debug!(
                "[FRAME-TAKE-PACKETS] draining {n} packets"
            );
            Some(std::mem::take(&mut self.ready_packets))
        } else {
            None
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    fn try_emit_ordered(&mut self) -> Vec<VideoPacket> {
        let mut packets = Vec::new();

        while self.next_emit_idx < self.total_slices {
            let idx = self.next_emit_idx as usize;
            match self.decoded_slices[idx].take() {
                Some(slice) => {
                    log::trace!(
                        "[EMIT-SLICE] s={idx} → VideoPacket (next_emit→{})",
                        idx + 1,
                    );
                    packets.push(Self::slice_to_packet(slice, self.first_us));
                    self.next_emit_idx += 1;
                }
                None => {
                    log::trace!(
                        "[EMIT-BLOCKED] s={idx} not yet decoded, \
                         stopping ordered emit at next_emit={idx}"
                    );
                    break;
                }
            }
        }

        packets
    }

    fn try_advance_frame_state(&mut self) {
        if self.state == FrameState::Receiving && self.next_emit_idx == self.total_slices {
            log::debug!(
                "[FRAME→COMPLETE-ADVANCE] next_emit={ne} == total_slices={ts}",
                ne = self.next_emit_idx,
                ts = self.total_slices,
            );
            self.state = FrameState::Complete;
        }
    }

    fn decode_slice_sync(&self, slice_idx: u8) -> Option<VideoSlice> {
        let sb = self.slices.get(&slice_idx)?;
        log::trace!(
            "[DECODE-SLICE-SYNC] s={slice_idx} state={st} groups={tg}",
            st = slice_state_label(&sb.state),
            tg = sb.total_groups,
        );
        let data = sb.get_data_sync()?;
        log::trace!(
            "[DECODE-SLICE-SYNC] s={slice_idx} raw_bytes={n} — deserializing",
            n = data.len(),
        );
        let result = postcard::take_from_bytes(&data).ok().map(|(vs, _)| vs);
        if result.is_none() {
            log::error!(
                "[DECODE-SLICE-DESER-FAIL] s={slice_idx} raw_bytes={n} — \
                 postcard deserialization failed",
                n = data.len(),
            );
        }
        result
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
        let before_count = self.ready_packets.len();
        let new_packets  = self.try_emit_ordered();
        let added_count  = new_packets.len();

        self.ready_packets.extend(new_packets);
        let total_ready = self.ready_packets.len();

        let missing_slices = self.total_slices.saturating_sub(self.next_emit_idx);

        // Detailed slice-by-slice breakdown of what's missing
        log::warn!(
            "[FRAME-PARTIAL-EMIT] frame={frame_id} ready_packets={total_ready} \
             (new={added_count} buffered={before_count}) \
             emitted_slices={ne}/{ts} missing_slices={missing_slices}",
            ne = self.next_emit_idx,
            ts = self.total_slices,
        );

        // Log the state of every slice so we know exactly what was lost
        for si in 0..self.total_slices {
            let decoded = self.decoded_slices.get(si as usize).map_or(false, |d| d.is_some());
            let sb_state = self.slices.get(&si)
                .map(|sb| slice_state_label(&sb.state))
                .unwrap_or("absent");
            let group_info: Vec<String> = self.slices.get(&si).map(|sb| {
                sb.groups.iter().map(|(gi, gb)| {
                    format!(
                        "g{}[{} {}/{}]",
                        gi,
                        group_state_label(&gb.state),
                        gb.received,
                        gb.k + gb.m,
                    )
                }).collect()
            }).unwrap_or_default();

            log::warn!(
                "[FRAME-PARTIAL-SLICE] frame={frame_id} s={si} \
                 decoded={decoded} slice_state={sb_state} groups=[{groups}]",
                groups = group_info.join(", "),
            );
        }

        if total_ready == 0 {
            log::warn!(
                "[FRAME-PARTIAL-EMPTY] frame={frame_id} — \
                 NO packets available for concealment (complete loss)"
            );
        } else {
            if let Some(last) = self.ready_packets.last_mut() {
                log::debug!(
                    "[FRAME-PARTIAL-FORCE-LAST] frame={frame_id} \
                     forcing is_last+is_concealed on slice_idx={}",
                    last.slice_idx,
                );
                last.is_last      = true;
                last.is_concealed = true;
            }
        }

        self.state = FrameState::Abandoned;
        log::debug!(
            "[FRAME→ABANDONED] frame={frame_id} partial emit done, \
             returning {total_ready} packets"
        );

        std::mem::take(&mut self.ready_packets)
    }

    fn slice_to_packet(slice: VideoSlice, first_us: u64) -> VideoPacket {
        let mut trace = slice.trace;
        if let Some(ref mut t) = trace {
            t.receive_us = first_us;
        }
        VideoPacket {
            frame_id:     slice.frame_id,
            payload:      slice.payload,
            slice_idx:    slice.slice_idx,
            is_key:       slice.is_key,
            is_last:      slice.is_last,
            is_concealed: false,
            trace,
        }
    }

    pub fn mark_ready(&mut self) {
        if self.network_ready_us.is_none() {
            let ts = FrameTrace::now_us();
            self.network_ready_us = Some(ts);
            log::debug!(
                "[FRAME-READY-MARK] network_ready_us={ts}µs \
                 frame_age={age}µs",
                age = ts.saturating_sub(self.first_us),
            );
        }
    }

    // ── Diagnostic helpers (public for assembler) ────────────────────────────

    /// Log a full per-slice / per-group status dump for this frame.
    pub(crate) fn log_full_status(&self, frame_id: u64, label: &str, now_us: u64) {
        if !log::log_enabled!(log::Level::Debug) { return; }

        let age = now_us.saturating_sub(self.first_us);
        log::debug!(
            "[FRAME-STATUS/{label}] frame={frame_id} state={st} \
             slices={ne}/{ts} age={age}µs \
             collect={col}µs fec_sub={fs}µs fec_done={fd}µs ready={rdy:?}",
            st  = frame_state_label(&self.state),
            ne  = self.next_emit_idx,
            ts  = self.total_slices,
            col = self.collecting_done_us.saturating_sub(self.first_us),
            fs  = self.fec_submitted_us.saturating_sub(self.first_us),
            fd  = self.fec_done_us.saturating_sub(self.first_us),
            rdy = self.network_ready_us,
        );

        for si in 0..self.total_slices {
            let decoded = self.decoded_slices.get(si as usize).map_or(false, |d| d.is_some());
            if let Some(sb) = self.slices.get(&si) {
                log::debug!(
                    "[FRAME-STATUS/{label}]  s={si} state={st} \
                     ready={rg}/{tg} decoded={decoded}",
                    st = slice_state_label(&sb.state),
                    rg = sb.ready_groups,
                    tg = sb.total_groups,
                );
                for g in 0..sb.total_groups {
                    if let Some(gb) = sb.groups.get(&g) {
                        let mut smap = String::new();
                        for bi in 0..gb.k {
                            smap.push(if (gb.received_mask() >> bi) & 1 == 1 { '█' } else { '░' });
                        }
                        smap.push('|');
                        for bi in 0..gb.m {
                            smap.push(if (gb.received_mask() >> (gb.k + bi)) & 1 == 1 { '█' } else { '░' });
                        }
                        log::debug!(
                            "[FRAME-STATUS/{label}]    g={g} [{st}] \
                             k={k} m={m} recv={rx}/{tot} [{smap}] \
                             missing_data={md}",
                            st  = group_state_label(&gb.state),
                            k   = gb.k,
                            m   = gb.m,
                            rx  = gb.received,
                            tot = gb.k + gb.m,
                            md  = gb.k.saturating_sub(gb.data_received),
                        );
                    } else {
                        log::debug!(
                            "[FRAME-STATUS/{label}]    g={g} [absent]"
                        );
                    }
                }
            } else {
                log::debug!(
                    "[FRAME-STATUS/{label}]  s={si} [no SliceBuilder] decoded={decoded}"
                );
            }
        }
    }
}