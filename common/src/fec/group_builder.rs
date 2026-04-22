use crate::fec::decode::FecDecoder;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GroupState {
    /// Holes exist but losses ≤ M — FEC can still recover; waiting for shards.
    Collecting,
    /// Losses > M; FEC cannot help.  Assembler should emit a NACK.
    /// `since_us` is used to enforce `MIN_NACK_AGE_US` before the first NACK.
    Stalled { since_us: u64 },
    /// NACK has been sent; suppressing repeats until `NACK_SUPPRESS_US` elapses.
    Requested { sent_at_us: u64 },
    /// RS reconstruction is running in the `ComputePool`; do not submit again.
    Decoding,
    /// Group fully assembled (all data present, or FEC succeeded).
    /// Terminal — no further shard processing needed.
    Ready,
    /// Frame expired or evicted; group is dead.
    /// Terminal — inserted shards are silently dropped.
    Abandoned,
}

impl GroupState {
    /// Returns `true` when this group should have a NACK sent right now.
    pub(crate) fn needs_nack(&self, now_us: u64) -> bool {
        match self {
            GroupState::Stalled { since_us } =>
                now_us.saturating_sub(*since_us) >= MIN_NACK_AGE_US,
            GroupState::Requested { sent_at_us } =>
                now_us.saturating_sub(*sent_at_us) >= NACK_SUPPRESS_US,
            _ => false,
        }
    }

    /// Whether this is a terminal state (no more work needed for this group).
    #[inline]
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, GroupState::Ready | GroupState::Abandoned | GroupState::Decoding)
    }
}

pub(crate) struct GroupBuilder {
    pub(crate) data_shards:    Vec<Option<Bytes>>,
    pub(crate) parity_shards:  Vec<Option<Bytes>>,

    pub(crate) payload_lens: Vec<u16>,
    /// Total shards received (data + parity).
    pub(crate) received:     u8,
    pub(crate) data_received:  u8,
    /// Bit-mask of received shard indices (covers indices 0..63).
    received_mask:           u64,
    pub(crate) k:            u8,
    pub(crate) m:            u8,
    pub(crate) state:        GroupState,
    /// Timestamp of the very first shard arrival (used for `MIN_NACK_AGE_US`).
    pub(crate) first_us:     u64,
}

impl GroupBuilder {
    pub(crate) fn new(k: u8, m: u8) -> Self {
        let k_us = k as usize;
        let m_us = m as usize;
        let mut data_shards = Vec::with_capacity(k_us);
        let mut parity_shards = Vec::with_capacity(m_us);
        data_shards.resize_with(k_us, || None);
        parity_shards.resize_with(m_us, || None);

        Self {
            data_shards,
            parity_shards,
            payload_lens: vec![0u16; k as usize],
            received: 0,
            data_received: 0,
            received_mask: 0,
            k,
            m,
            state: GroupState::Collecting,
            first_us: 0,
        }
    }

    /// Insert one shard.
    ///
    /// Returns:
    /// - `bool` — whether this chunk was awaited (state was `Requested`), i.e.
    ///            it arrived in response to a NACK.
    /// - `bool` — whether this insertion caused a `Collecting → Stalled`
    ///            transition that the assembler needs to enqueue.
    pub(crate) fn insert(
        &mut self,
        idx:         u8,
        data:        Bytes,
        payload_len: u16,
        now_us:      u64,
    ) -> (bool, bool) {
        // ── Terminal states: silently accept the shard but skip state logic ──
        if self.state.is_terminal() {
            self.store_shard(idx, data, payload_len);
            return (false, false);
        }

        let i = idx as usize;
        let is_data = i < self.k as usize;

        if is_data {
            if i >= self.data_shards.len() { return (false, false); }
        } else {
            let pi = i - self.k as usize;
            if pi >= self.parity_shards.len() { return (false, false); }
        }

        // ── Accounting ───────────────────────────────────────────────────────
        let is_new = if is_data {
            self.data_shards[i].is_none()
        } else {
            self.parity_shards[i - self.k as usize].is_none()
        };

        if is_new {
            self.received = self.received.saturating_add(1);
            if is_data {
                self.data_received = self.data_received.saturating_add(1);
            }
            if i < 64 {
                self.received_mask |= 1u64 << i;
            }
            if self.first_us == 0 {
                self.first_us = now_us;
            }
        }

        self.store_shard(idx, data, payload_len);

        // ── State transitions ─────────────────────────────────────────────────
        //
        // Only Collecting / Stalled / Requested can transition here.
        // Decoding / Ready / Abandoned are handled by the terminal guard above.

        let was_awaited = matches!(self.state, GroupState::Requested { .. });
        let mut newly_stalled = false;

        let total   = (self.k + self.m) as u64;
        let missing = total.saturating_sub(self.received as u64);
        let can_fec = self.m as u64;

        if missing > can_fec {
            // Losses exceed the FEC budget.
            match self.state {
                GroupState::Collecting => {
                    // First time we detect unrecoverable loss → Stalled.
                    self.state    = GroupState::Stalled { since_us: now_us };
                    newly_stalled = true;
                }
                // Already Stalled or Requested — stay as-is; the NACK loop
                // drives the Stalled→Requested transition, and the suppress
                // timer drives Requested→Stalled.
                _ => {}
            }
        } else {
            // Losses back within FEC budget (a late NACK retransmission arrived).
            // Roll back to Collecting so the slice can attempt recovery.
            //
            // Note: the NACK queue entry is NOT removed here — the drain loop
            // checks needs_nack() which returns false for Collecting, so the
            // entry is naturally dropped on the next drain without emitting a
            // spurious NACK.
            match &self.state {
                GroupState::Stalled { .. } | GroupState::Requested { .. } => {
                    self.state = GroupState::Collecting;
                }
                _ => {}
            }
        }

        (was_awaited, newly_stalled)
    }

    /// `true` when we have received enough shards (≥ k) to attempt decoding,
    /// AND at least one of them is a data shard (prevents firing RS on a
    /// parity-only set, which cannot reconstruct the original data).
    pub(crate) fn is_ready(&self) -> (bool, bool) {
        // Fast path: all data shards arrived — no RS needed ever.
        if self.data_received == self.k {
            return (true, false);
        }
        // FEC path: enough total shards for RS reconstruction.
        let recoverable_via_parity = self.received >= self.k;
        if recoverable_via_parity {
            return (true, true)
        }
        (false, false)
    }

    #[inline]
    pub(crate) fn is_direct(&self) -> bool {
        self.data_received == self.k
    }

    pub(crate) fn received_mask(&self) -> u64 {
        self.received_mask
    }

    pub(crate) fn as_rs_shards(&self) -> Vec<Option<Bytes>> {
        self.data_shards.iter()
            .chain(self.parity_shards.iter())
            .cloned()
            .collect()
    }

    /// Run FEC decode (or fast-path copy) and return the group's payload bytes.
    pub(crate) fn get_data(&self) -> Option<Vec<u8>> {
        let shards = self.as_rs_shards();
        FecDecoder::decode(self.k, self.m, &shards, &self.payload_lens)
    }

    /// Count how many of the k data shard slots are populated.
    #[cfg(debug_assertions)]
    pub(crate) fn data_received_count(&self) -> u8 {
        self.data_received
    }

    fn store_shard(&mut self, idx: u8, data: Bytes, payload_len: u16) {
        let i = idx as usize;
        if i < self.k as usize {
            self.data_shards[i]   = Some(data);
            self.payload_lens[i]  = payload_len;
        } else {
            let pi = i - self.k as usize;
            if pi < self.parity_shards.len() {
                self.parity_shards[pi] = Some(data);
            }
        }
    }
}