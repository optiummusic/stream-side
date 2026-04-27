use crate::fec::decode::FecDecoder;

use super::*;

// ── Diagnostic import ────────────────────────────────────────────────────────
use crate::fec::assembly_log::{AssemblyDiag, group_state_label};

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
    DecodingInProgress,
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
    first_global_seq: u64
}

impl GroupBuilder {
    pub(crate) fn new(k: u8, m: u8) -> Self {
        log::trace!(
            "[GROUP-INIT] creating GroupBuilder k={k} m={m} total_shards={total}",
            total = k + m,
        );

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
            first_global_seq: 0,
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
        global_seq: u64,
    ) -> (bool, bool) {
        // ── Terminal states: silently accept the shard but skip state logic ──
        if self.state.is_terminal() {
            log::trace!(
                "[SHARD-TERM] shard={idx} k={k} m={m} state={st} — \
                 terminal group, shard stored but state unchanged",
                k  = self.k,
                m  = self.m,
                st = group_state_label(&self.state),
            );
            self.store_shard(idx, data, payload_len);
            return (false, false);
        }
        if self.first_global_seq == 0 {
            self.first_global_seq = global_seq;
        }
        let group_age_packets = global_seq.saturating_sub(self.first_global_seq);
        const INTERLEAVE_THRESHOLD: u64 = 40;


        let i = idx as usize;
        let is_data = i < self.k as usize;

        if is_data {
            if i >= self.data_shards.len() {
                log::error!(
                    "[SHARD-OOB] data shard idx={idx} k={k} — out of bounds, dropping",
                    k = self.k,
                );
                return (false, false);
            }
        } else {
            let pi = i - self.k as usize;
            if pi >= self.parity_shards.len() {
                log::error!(
                    "[SHARD-OOB] parity shard idx={idx} k={k} m={m} \
                     parity_idx={pi} — out of bounds, dropping",
                    k  = self.k,
                    m  = self.m,
                    pi = i - self.k as usize,
                );
                return (false, false);
            }
        }

        // ── Accounting ───────────────────────────────────────────────────────
        let is_new = if is_data {
            self.data_shards[i].is_none()
        } else {
            self.parity_shards[i - self.k as usize].is_none()
        };

        if !is_new {
            log::trace!(
                "[SHARD-DUP] shard={idx} k={k} m={m} is_data={is_data} — \
                 duplicate, already have this shard",
                k = self.k, m = self.m,
            );
        }

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

        // ── Per-shard trace log ──────────────────────────────────────────────
        log::trace!(
            "[SHARD-INSERT] shard={idx}/{total} ({kind}) \
             k={k} m={m} payload={payload_len}B is_new={is_new} \
             data_recv={dr}/{k} parity_recv={pr}/{m} total_recv={rx}/{total} \
             mask=0x{mask:016x} state={st} ts={now_us}µs",
            total       = self.k + self.m,
            kind        = if is_data { "DATA" } else { "PARITY" },
            k           = self.k,
            m           = self.m,
            dr          = self.data_received,
            pr          = self.received.saturating_sub(self.data_received),
            rx          = self.received,
            mask        = self.received_mask,
            st          = group_state_label(&self.state),
        );

        self.store_shard(idx, data, payload_len);

        // ── State transitions ─────────────────────────────────────────────────
        let was_awaited = matches!(self.state, GroupState::Requested { .. });
        let mut newly_stalled = false;

        let total   = (self.k + self.m) as u64;
        let missing = total.saturating_sub(self.received as u64);
        let can_fec = self.m as u64;

        log::trace!(
            "[GROUP-BUDGET] k={k} m={m} received={rx} missing={missing} \
             fec_budget={can_fec} recoverable={rec}",
            k         = self.k,
            m         = self.m,
            rx        = self.received,
            rec       = missing <= can_fec,
        );

        if missing > can_fec {
            match self.state {
                GroupState::Collecting => {
                    if group_age_packets > INTERLEAVE_THRESHOLD {
                        log::warn!(
                            "[GROUP-STALL] k={k} m={m} received={rx} missing={missing} \
                            fec_budget={can_fec} — losses exceed FEC budget, \
                            transitioning Collecting → Stalled at ts={now_us}µs",
                            k  = self.k,
                            m  = self.m,
                            rx = self.received,
                        );
                        self.state    = GroupState::Stalled { since_us: now_us };
                        newly_stalled = true;
                    }
                    else {
                        // Группа "неполная", но мы ждем, так как окно интерливинга еще не закрыто
                        log::trace!("[GROUP-WAIT] Interleaving window: age {}", group_age_packets);
                    }
                }
                GroupState::Stalled { since_us } => {
                    log::debug!(
                        "[GROUP-STALL-PERSIST] k={k} m={m} received={rx} missing={missing} \
                         already Stalled since={since_us}µs age={age}µs",
                        k        = self.k,
                        m        = self.m,
                        rx       = self.received,
                        age      = now_us.saturating_sub(since_us),
                    );
                }
                GroupState::Requested { sent_at_us } => {
                    log::debug!(
                        "[GROUP-NACK-WAIT] k={k} m={m} received={rx} missing={missing} \
                         still unrecoverable, NACK sent at={sent_at_us}µs \
                         suppress_remaining={rem}µs",
                        k           = self.k,
                        m           = self.m,
                        rx          = self.received,
                        rem         = NACK_SUPPRESS_US.saturating_sub(
                                          now_us.saturating_sub(sent_at_us)),
                    );
                }
                _ => {}
            }
        } else {
            match &self.state {
                GroupState::Stalled { .. } | GroupState::Requested { .. } => {
                    log::info!(
                        "[GROUP-UNSTALL] k={k} m={m} received={rx} missing={missing} \
                         losses back within FEC budget (budget={can_fec}), \
                         transitioning → Collecting at ts={now_us}µs",
                        k  = self.k,
                        m  = self.m,
                        rx = self.received,
                    );
                    self.state = GroupState::Collecting;
                }
                _ => {}
            }
        }

        // ── Log shard map on every new insertion ─────────────────────────────
        if is_new {
            let mut shard_map = String::with_capacity((self.k + self.m) as usize + 6);
            shard_map.push_str("D[");
            for bi in 0..self.k {
                shard_map.push(if (self.received_mask >> bi) & 1 == 1 { '█' } else { '░' });
            }
            shard_map.push_str("] P[");
            for bi in 0..self.m {
                shard_map.push(if (self.received_mask >> (self.k + bi)) & 1 == 1 { '█' } else { '░' });
            }
            shard_map.push(']');

            log::trace!(
                "[SHARD-MAP] k={k} m={m} {shard_map}  \
                 missing_data={md} can_recover={cr}",
                k   = self.k,
                m   = self.m,
                md  = self.k.saturating_sub(self.data_received),
                cr  = self.received >= self.k,
            );
        }

        (was_awaited, newly_stalled)
    }

    /// `true` when we have received enough shards (≥ k) to attempt decoding,
    /// AND at least one of them is a data shard (prevents firing RS on a
    /// parity-only set, which cannot reconstruct the original data).
    pub(crate) fn is_ready(&self) -> (bool, bool) {
        // Fast path: all data shards arrived — no RS needed ever.
        if self.data_received == self.k {
            log::trace!(
                "[GROUP-READY-CHECK] k={k} m={m} data_received={dr} → DIRECT (all data)",
                k  = self.k,
                m  = self.m,
                dr = self.data_received,
            );
            return (true, false);
        }
        // FEC path: enough total shards for RS reconstruction.
        let recoverable_via_parity = self.received >= self.k;
        if recoverable_via_parity {
            log::trace!(
                "[GROUP-READY-CHECK] k={k} m={m} received={rx} → FEC path \
                 (data={dr} missing={md})",
                k  = self.k,
                m  = self.m,
                rx = self.received,
                dr = self.data_received,
                md = self.k.saturating_sub(self.data_received),
            );
            return (true, true);
        }
        log::trace!(
            "[GROUP-READY-CHECK] k={k} m={m} received={rx}/{needed} → NOT READY \
             (need {short} more shards)",
            k      = self.k,
            m      = self.m,
            rx     = self.received,
            needed = self.k,
            short  = self.k.saturating_sub(self.received),
        );
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
        log::trace!(
            "[GROUP-DECODE] k={k} m={m} data_recv={dr} → {}",
            if self.data_received == self.k { "fast-path copy" } else { "RS reconstruct" },
            k  = self.k,
            m  = self.m,
            dr = self.data_received,
        );
        let shards = self.as_rs_shards();
        let result = FecDecoder::decode(self.k, self.m, &shards, &self.payload_lens);
        if result.is_none() {
            log::error!(
                "[GROUP-DECODE-FAIL] k={k} m={m} data_recv={dr} received={rx} — \
                 FecDecoder::decode returned None",
                k  = self.k,
                m  = self.m,
                dr = self.data_received,
                rx = self.received,
            );
        }
        result
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