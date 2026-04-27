use crate::{DatagramChunk, FrameTrace, fec::group_builder::{GroupBuilder, GroupState}};
use crate::fec::assembly_log::{group_state_label, slice_state_label};

use super::*;

// ═══════════════════════════════════════════════════════════════════════════════
// SliceBuilder
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SliceState {
    /// Still waiting for enough shards across all groups.
    Assembling,
    /// All groups have received ≥ k shards; RS tasks have been submitted to
    /// the pool for groups where data_count < k.  Waiting for task results.
    EmittingFec,
    /// All groups were complete via the data fast-path; VideoSlice is decoded.
    EmittingDirect,
    /// VideoSlice has been placed into `FrameBuilder::decoded_slices`.
    Emitted,
    /// Slice was abandoned due to frame expiry or eviction.
    Abandoned,
}

impl SliceState {
    #[inline]
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, SliceState::Emitted | SliceState::Abandoned)
    }
}

pub(crate) struct SliceBuilder {
    pub(crate) groups:       HashMap<u8, GroupBuilder>,
    pub(crate) total_groups: u8,
    pub(crate) ready_groups:            u8,
    pub(crate) state:        SliceState,
}

impl SliceBuilder {
    pub(crate) fn new(total_groups: u8) -> Self {
        let total = total_groups.max(1);
        log::debug!(
            "[SLICE-INIT] creating SliceBuilder total_groups={total}"
        );
        Self {
            groups:       HashMap::new(),
            total_groups: total,
            ready_groups: 0,
            state:        SliceState::Assembling,
        }
    }

    /// Insert a chunk into the appropriate `GroupBuilder`.
    ///
    /// Returns:
    /// - `Option<GroupRecovery>` — stats event if a group just became ready.
    /// - `bool`                  — whether this was a NACK-recovered chunk.
    /// - `Option<(u8, u8)>`      — `(slice_idx, group_idx)` of a newly-stalled
    ///                             group (needs NACK queue entry); uses the
    ///                             coordinates provided by the caller's chunk.
    pub(crate) fn insert(
        &mut self,
        chunk: &DatagramChunk,
        global_seq: u64,
    ) -> (Option<GroupRecovery>, bool, Option<(u8, u8)>, Option<u64>) {
        // Accept updated total_groups from any chunk (all carry the same value).
        if chunk.total_groups > 0 {
            if chunk.total_groups != self.total_groups {
                log::debug!(
                    "[SLICE-GROUPS-UPDATE] s={si} total_groups {old} → {new} \
                     (from chunk frame={fid})",
                    si  = chunk.slice_idx,
                    old = self.total_groups,
                    new = chunk.total_groups,
                    fid = chunk.frame_id,
                );
            }
            self.total_groups = chunk.total_groups;
        }

        let now_us = FrameTrace::now_us();

        // ── Log the group map before the insert ──────────────────────────────
        log::trace!(
            "[SLICE-INSERT] frame={fid} s={si} g={gi} shard={sh} \
             state={st} ready_groups={rg}/{tg}",
            fid = chunk.frame_id,
            si  = chunk.slice_idx,
            gi  = chunk.group_idx,
            sh  = chunk.shard_idx,
            st  = slice_state_label(&self.state),
            rg  = self.ready_groups,
            tg  = self.total_groups,
        );

        let group = self.groups
            .entry(chunk.group_idx)
            .or_insert_with(|| {
                log::debug!(
                    "[GROUP-NEW] frame={fid} s={si} g={gi} k={k} m={m} \
                     — first shard for this group",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    gi  = chunk.group_idx,
                    k   = chunk.k,
                    m   = chunk.m,
                );
                GroupBuilder::new(chunk.k, chunk.m)
            });

        let (was_ready, _via_parity)                  = group.is_ready();
        let (was_nack_recovery, newly_stalled_flag) =
            group.insert(chunk.shard_idx, chunk.data.clone(), chunk.payload_len, now_us, global_seq);
        let (ready_now, via_parity)                  = group.is_ready();

        // ── Group state transition: → Ready ──────────────────────────────────
        let recovery = if !was_ready && ready_now {
            self.ready_groups = self.ready_groups.saturating_add(1);
            group.state = GroupState::Ready;

            let is_direct = group.is_direct();
            let missing_d = group.k.saturating_sub(group.data_received);
            let parity_used = group.received.saturating_sub(group.data_received);

            if is_direct {
                log::debug!(
                    "[GROUP→READY-DIRECT] frame={fid} s={si} g={gi} k={k} m={m} \
                     age={age}µs  ready_groups={rg}/{tg}",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    gi  = chunk.group_idx,
                    k   = group.k,
                    m   = group.m,
                    age = now_us.saturating_sub(group.first_us),
                    rg  = self.ready_groups,
                    tg  = self.total_groups,
                );
            } else {
                log::info!(
                    "[GROUP→READY-FEC] frame={fid} s={si} g={gi} k={k} m={m} \
                     missing_data={missing_d} parity_used={parity_used} \
                     via_parity={via_parity} age={age}µs  ready_groups={rg}/{tg}",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    gi  = chunk.group_idx,
                    k   = group.k,
                    m   = group.m,
                    age = now_us.saturating_sub(group.first_us),
                    rg  = self.ready_groups,
                    tg  = self.total_groups,
                );
            }

            Some(if is_direct {
                GroupRecovery::Direct
            } else {
                GroupRecovery::Fec { missing_data_shards: missing_d, via_parity }
            })
        } else {
            None
        };

        // ── Slice state transition: Assembling → Emitting* ───────────────────
        let mut just_transitioned = false;
        if self.state == SliceState::Assembling && self.ready_groups == self.total_groups {
            let mut any_needs_fec = false;
            let mut fec_group_count: u8 = 0;

            for (_, gb) in self.groups.iter_mut() {
                if gb.state == GroupState::Ready && !gb.is_direct() && gb.state != GroupState::DecodingInProgress{
                    gb.state = GroupState::Decoding;
                    any_needs_fec = true;
                    fec_group_count += 1;
                }
            }

            let new_state = if any_needs_fec {
                SliceState::EmittingFec
            } else {
                SliceState::EmittingDirect
            };

            if any_needs_fec {
                log::info!(
                    "[SLICE→EMITTING-FEC] frame={fid} s={si} \
                     total_groups={tg} fec_groups={fg} age={age}µs",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    tg  = self.total_groups,
                    fg  = fec_group_count,
                    age = now_us,   // caller has the first_us; close enough for relative
                );
            } else {
                log::debug!(
                    "[SLICE→EMITTING-DIRECT] frame={fid} s={si} \
                     total_groups={tg} all data complete age={age}µs",
                    fid = chunk.frame_id,
                    si  = chunk.slice_idx,
                    tg  = self.total_groups,
                    age = now_us,
                );
            }

            self.state = new_state;
            just_transitioned = true;
        }

        // ── NACK stall notification ───────────────────────────────────────────
        if newly_stalled_flag {
            log::warn!(
                "[SLICE-NACK-NEEDED] frame={fid} s={si} g={gi} — \
                 group stalled, enqueuing NACK coords",
                fid = chunk.frame_id,
                si  = chunk.slice_idx,
                gi  = chunk.group_idx,
            );
        }

        // ── Dump all groups in this slice on each transition ─────────────────
        if just_transitioned || newly_stalled_flag {
            self.log_group_summary(chunk.frame_id, chunk.slice_idx, now_us);
        }

        let stalled_coords = if newly_stalled_flag {
            Some((chunk.slice_idx, chunk.group_idx))
        } else {
            None
        };
        let collecting_done = if just_transitioned { Some(FrameTrace::now_us()) } else { None };
        (recovery, was_nack_recovery, stalled_coords, collecting_done)
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready_groups == self.total_groups
    }

    /// Synchronous data assembly: concatenate decoded bytes from every group
    /// in order.  Returns `None` if any group is still incomplete or absent.
    pub(crate) fn get_data_sync(&self) -> Option<Vec<u8>> {
        log::trace!(
            "[SLICE-GET-DATA] state={st} total_groups={tg}",
            st = slice_state_label(&self.state),
            tg = self.total_groups,
        );
        let mut out = Vec::new();
        for g in 0..self.total_groups {
            match self.groups.get(&g) {
                Some(gb) => {
                    match gb.get_data() {
                        Some(part) => {
                            log::trace!(
                                "[SLICE-GET-DATA] group={g} → {bytes}B",
                                bytes = part.len(),
                            );
                            out.extend_from_slice(&part);
                        }
                        None => {
                            log::warn!(
                                "[SLICE-GET-DATA-FAIL] group={g} state={st} \
                                 k={k} m={m} received={rx} — get_data() returned None",
                                st = group_state_label(&gb.state),
                                k  = gb.k,
                                m  = gb.m,
                                rx = gb.received,
                            );
                            return None;
                        }
                    }
                }
                None => {
                    log::warn!(
                        "[SLICE-GET-DATA-MISSING] group={g} not present in \
                         slice (total_groups={tg}) — cannot assemble",
                        tg = self.total_groups,
                    );
                    return None;
                }
            }
        }
        log::trace!(
            "[SLICE-GET-DATA-OK] assembled {} bytes from {} groups",
            out.len(),
            self.total_groups,
        );
        Some(out)
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Log a one-line summary of every known group's state.
    fn log_group_summary(&self, frame_id: u64, slice_idx: u8, now_us: u64) {
        if !log::log_enabled!(log::Level::Debug) { return; }
        let mut buf = format!(
            "[SLICE-GROUPS] frame={frame_id} s={slice_idx} \
             state={st} ready={rg}/{tg} groups=[",
            st = slice_state_label(&self.state),
            rg = self.ready_groups,
            tg = self.total_groups,
        );
        for g in 0..self.total_groups {
            if let Some(gb) = self.groups.get(&g) {
                let smap = {
                    let mut s = String::new();
                    for bi in 0..gb.k {
                        s.push(if (gb.received_mask() >> bi) & 1 == 1 { '█' } else { '░' });
                    }
                    s.push('|');
                    for bi in 0..gb.m {
                        s.push(if (gb.received_mask() >> (gb.k + bi)) & 1 == 1 { '█' } else { '░' });
                    }
                    s
                };
                buf.push_str(&format!(
                    " g{g}({st} {rx}/{tot} [{smap}])",
                    st  = group_state_label(&gb.state),
                    rx  = gb.received,
                    tot = gb.k + gb.m,
                ));
            } else {
                buf.push_str(&format!(" g{g}(absent)"));
            }
        }
        buf.push(']');
        log::debug!("{buf}");
    }
}