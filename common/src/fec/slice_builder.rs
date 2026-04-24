use crate::{DatagramChunk, FrameTrace, fec::group_builder::{GroupBuilder, GroupState}};

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
    ready_groups:            u8,
    pub(crate) state:        SliceState,
}

impl SliceBuilder {
    pub(crate) fn new(total_groups: u8) -> Self {
        Self {
            groups:       HashMap::new(),
            total_groups: total_groups.max(1),
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
    ) -> (Option<GroupRecovery>, bool, Option<(u8, u8)>, Option<u64>) {
        // Accept updated total_groups from any chunk (all carry the same value).
        if chunk.total_groups > 0 {
            self.total_groups = chunk.total_groups;
        }

        let now_us = FrameTrace::now_us();

        let group = self.groups
            .entry(chunk.group_idx)
            .or_insert_with(|| GroupBuilder::new(chunk.k, chunk.m));

        let (was_ready, _via_parity)                  = group.is_ready();
        let (was_nack_recovery, newly_stalled_flag) =
            group.insert(chunk.shard_idx, chunk.data.clone(), chunk.payload_len, now_us);
        let (ready_now, via_parity)                  = group.is_ready();
        // ── Group state transition: → Ready ──────────────────────────────────
        let recovery = if !was_ready && ready_now {
            self.ready_groups = self.ready_groups.saturating_add(1);
            group.state = GroupState::Ready;

            Some(if group.is_direct() {
                GroupRecovery::Direct
            } else {
                GroupRecovery::Fec { missing_data_shards: group.k - group.data_received, via_parity }
            })
        } else {
            None
        };

        // ── Slice state transition: Assembling → Emitting* ───────────────────
        let mut just_transitioned = false;
        if self.state == SliceState::Assembling && self.ready_groups == self.total_groups {
            let mut any_needs_fec = false;

            for (_, gb) in self.groups.iter_mut() {
                if gb.state == GroupState::Ready && !gb.is_direct() {
                    gb.state = GroupState::Decoding;
                    any_needs_fec = true;
                }
            }

            self.state = if any_needs_fec {
                SliceState::EmittingFec
            } else {
                SliceState::EmittingDirect
            };
            just_transitioned = true;
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
        let mut out = Vec::new();
        for g in 0..self.total_groups {
            let part = self.groups.get(&g)?.get_data()?;
            out.extend_from_slice(&part);
        }
        Some(out)
    }
}