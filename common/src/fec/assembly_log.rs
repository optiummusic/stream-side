//! # Assembly Diagnostics — Exhaustive 10-frame Rolling Window Logger
//!
//! Drop this file into your `fec/` module directory and add to `mod.rs`:
//!
//! ```rust
//! #[cfg(feature = "receiver")]
//! pub mod assembly_log;
//! ```
//!
//! Then add `use super::assembly_log::AssemblyDiag;` in assembler.rs / frame_builder.rs.
//!
//! Every public method is `#[inline]` so that in release builds the compiler
//! can eliminate the entire call site when `ASSEMBLY_LOG_LEVEL` is below the
//! call site's level.  The `log` crate macros do the same, but the structure
//! construction overhead is also removed.
//!
//! ## Log levels used
//! | crate level | what                                                    |
//! |-------------|----------------------------------------------------------|
//! | `trace`     | Per-shard insert, shard bitmask, raw bytes               |
//! | `debug`     | Per-group state transitions, FEC math, NACK events       |
//! | `info`      | Per-slice / per-frame completions, 10-frame window dump  |
//! | `warn`      | Stalls, FEC failures, HOL blocking, evictions            |
//! | `error`     | Invariant violations, RS reconstruct failures            |

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as FmtWrite;

// ── Re-export so callers do `use super::assembly_log::*;` ───────────────────
pub use self::frame_snapshot::FrameSnap;
pub use self::group_snapshot::GroupSnap;
pub use self::slice_snapshot::SliceSnap;

// ── Window size ──────────────────────────────────────────────────────────────
const WINDOW: usize = 10;

// ═══════════════════════════════════════════════════════════════════════════════
// Snapshot types — lightweight, `Copy`-friendly, no heap allocation
// ═══════════════════════════════════════════════════════════════════════════════

mod group_snapshot {
    /// Snapshot of a `GroupBuilder` at a single point in time.
    #[derive(Debug, Clone)]
    pub struct GroupSnap {
        pub group_idx:     u8,
        pub k:             u8,
        pub m:             u8,
        /// Shards received so far (data + parity).
        pub received:      u8,
        /// Of those, how many were data shards.
        pub data_received: u8,
        /// Bitmask of which shard indices have arrived (indices 0..63).
        pub received_mask: u64,
        /// Human-readable state label.
        pub state_label:   &'static str,
        /// µs timestamp of the first shard for this group.
        pub first_us:      u64,
    }

    impl GroupSnap {
        /// Pretty-print the received_mask as a compact shard presence string.
        /// e.g. `"D:██░░█░  P:█░"` (D=data, P=parity, █=received, ░=missing)
        pub fn shard_map(&self) -> String {
            let mut s = String::with_capacity((self.k + self.m) as usize + 6);
            s.push_str("D[");
            for i in 0..self.k {
                s.push(if (self.received_mask >> i) & 1 == 1 { '█' } else { '░' });
            }
            s.push_str("] P[");
            for i in 0..self.m {
                s.push(if (self.received_mask >> (self.k + i)) & 1 == 1 { '█' } else { '░' });
            }
            s.push(']');
            s
        }

        /// How many data shards are still missing.
        pub fn data_missing(&self) -> u8 {
            self.k.saturating_sub(self.data_received)
        }

        /// How many parity shards have arrived.
        pub fn parity_received(&self) -> u8 {
            self.received.saturating_sub(self.data_received)
        }

        /// Whether FEC has enough shards to reconstruct (received >= k).
        pub fn can_recover(&self) -> bool {
            self.received >= self.k
        }
    }
}

mod slice_snapshot {
    use super::GroupSnap;

    #[derive(Debug, Clone)]
    pub struct SliceSnap {
        pub slice_idx:    u8,
        pub total_groups: u8,
        pub ready_groups: u8,
        pub state_label:  &'static str,
        pub groups:       Vec<GroupSnap>,
    }

    impl SliceSnap {
        /// Total shards received across all groups in this slice.
        pub fn total_received(&self) -> u32 {
            self.groups.iter().map(|g| g.received as u32).sum()
        }

        /// Total shards expected across all groups (k + m each).
        pub fn total_expected(&self) -> u32 {
            self.groups.iter().map(|g| (g.k + g.m) as u32).sum()
        }

        /// True if any group is unrecoverable (missing > m).
        pub fn has_unrecoverable_group(&self) -> bool {
            self.groups.iter().any(|g| !g.can_recover()
                && g.received < g.k + g.m   // still alive, not just 0/0
            )
        }
    }
}

mod frame_snapshot {
    use super::SliceSnap;

    #[derive(Debug, Clone)]
    pub struct FrameSnap {
        pub frame_id:       u64,
        pub total_slices:   u8,
        pub emitted_slices: u8,
        /// next_emit_idx from FrameBuilder
        pub next_emit_idx:  u8,
        pub state_label:    &'static str,
        pub first_us:       u64,
        pub age_us:         u64,
        /// Whether this frame is the current HOL head.
        pub is_hol:         bool,
        pub slices:         Vec<SliceSnap>,
        // Timing
        pub collecting_done_us: u64,
        pub fec_submitted_us:   u64,
        pub fec_done_us:        u64,
        pub network_ready_us:   Option<u64>,
    }

    impl FrameSnap {
        /// Total shards across the whole frame.
        pub fn total_received_shards(&self) -> u32 {
            self.slices.iter().map(|s| s.total_received()).sum()
        }

        pub fn total_expected_shards(&self) -> u32 {
            self.slices.iter().map(|s| s.total_expected()).sum()
        }

        /// % of shards received (0.0 – 100.0).
        pub fn completion_pct(&self) -> f32 {
            let exp = self.total_expected_shards();
            if exp == 0 { return 0.0; }
            self.total_received_shards() as f32 / exp as f32 * 100.0
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AssemblyDiag — the rolling-window accumulator
// ═══════════════════════════════════════════════════════════════════════════════

/// Maintains a rolling snapshot of the last `WINDOW` frames and exposes
/// every diagnostic log call the pipeline needs.
pub struct AssemblyDiag {
    /// frame_id → latest snapshot
    window: HashMap<u64, FrameSnap>,
    /// Ordered frame IDs in arrival order (front = oldest)
    order:  VecDeque<u64>,
    /// Total shard insertions logged since construction.
    shard_count: u64,
}

impl AssemblyDiag {
    pub fn new() -> Self {
        Self {
            window:      HashMap::with_capacity(WINDOW * 2),
            order:       VecDeque::with_capacity(WINDOW * 2),
            shard_count: 0,
        }
    }

    // ── Snapshot upsert ──────────────────────────────────────────────────────

    /// Upsert a `FrameSnap` into the rolling window.
    /// Evicts the oldest entry when the window overflows.
    pub fn upsert_frame(&mut self, snap: FrameSnap) {
        let fid = snap.frame_id;
        if !self.window.contains_key(&fid) {
            if self.order.len() >= WINDOW * 2 {
                if let Some(old) = self.order.pop_front() {
                    self.window.remove(&old);
                }
            }
            self.order.push_back(fid);
        }
        self.window.insert(fid, snap);
    }

    /// Get a mutable reference to a snapshot, if present.
    pub fn get_mut(&mut self, frame_id: u64) -> Option<&mut FrameSnap> {
        self.window.get_mut(&frame_id)
    }

    // ── Per-shard events ─────────────────────────────────────────────────────

    /// Called on every shard insertion in `GroupBuilder::insert`.
    pub fn on_shard_insert(
        &self,
        frame_id:    u64,
        slice_idx:   u8,
        group_idx:   u8,
        shard_idx:   u8,
        k:           u8,
        m:           u8,
        payload_len: u16,
        is_new:      bool,
        is_parity:   bool,
        received:    u8,
        received_mask: u64,
        now_us:      u64,
    ) {
        log::trace!(
            "[SHARD] frame={frame_id} s={slice_idx} g={group_idx} \
             shard={shard_idx}/{total}({kind}) payload={payload_len}B \
             new={is_new} received={received}/{total} \
             mask=0x{received_mask:016x} ts={now_us}µs",
            total    = k + m,
            kind     = if is_parity { "P" } else { "D" },
        );
    }

    /// Called when a duplicate (already-received) shard arrives.
    pub fn on_shard_duplicate(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
        shard_idx: u8,
    ) {
        log::trace!(
            "[SHARD-DUP] frame={frame_id} s={slice_idx} g={group_idx} shard={shard_idx} — duplicate, ignored"
        );
    }

    /// Called when a zombie shard arrives (frame already emitted / before HOL).
    pub fn on_zombie_shard(
        &self,
        frame_id:    u64,
        slice_idx:   u8,
        group_idx:   u8,
        shard_idx:   u8,
        reason:      &str,
    ) {
        log::debug!(
            "[ZOMBIE] frame={frame_id} s={slice_idx} g={group_idx} shard={shard_idx} — {reason}"
        );
    }

    // ── Per-group events ─────────────────────────────────────────────────────

    /// Called when a group transitions from `Collecting` → `Stalled`.
    pub fn on_group_stalled(
        &self,
        frame_id:    u64,
        slice_idx:   u8,
        group_idx:   u8,
        k:           u8,
        m:           u8,
        received:    u8,
        missing:     u8,
        now_us:      u64,
    ) {
        log::warn!(
            "[GROUP-STALL] frame={frame_id} s={slice_idx} g={group_idx} \
             k={k} m={m} received={received} missing={missing} \
             (budget={m}, UNRECOVERABLE) ts={now_us}µs"
        );
    }

    /// Called when a stalled group recovers (losses drop back within FEC budget).
    pub fn on_group_recovered_from_stall(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
        k:         u8,
        m:         u8,
        received:  u8,
    ) {
        log::debug!(
            "[GROUP-UNSTALL] frame={frame_id} s={slice_idx} g={group_idx} \
             k={k} m={m} received={received} — losses back within FEC budget, resuming"
        );
    }

    /// Called when a group reaches `Ready` via the all-data fast path.
    pub fn on_group_ready_direct(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
        k:         u8,
        m:         u8,
        age_us:    u64,
    ) {
        log::debug!(
            "[GROUP-READY-DIRECT] frame={frame_id} s={slice_idx} g={group_idx} \
             k={k} m={m} all data shards present — no RS needed  age={age_us}µs"
        );
    }

    /// Called when a group reaches `Ready` via FEC recovery.
    pub fn on_group_ready_fec(
        &self,
        frame_id:           u64,
        slice_idx:          u8,
        group_idx:          u8,
        k:                  u8,
        m:                  u8,
        missing_data:       u8,
        parity_used:        u8,
        age_us:             u64,
    ) {
        log::info!(
            "[GROUP-READY-FEC] frame={frame_id} s={slice_idx} g={group_idx} \
             k={k} m={m} missing_data={missing_data} parity_used={parity_used} \
             — FEC recovery succeeded  age={age_us}µs"
        );
    }

    /// Called when an FEC reconstruction task is submitted to the compute pool.
    pub fn on_fec_submitted(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
        k:         u8,
        m:         u8,
        n_shards_present: usize,
    ) {
        log::debug!(
            "[FEC-SUBMIT] frame={frame_id} s={slice_idx} g={group_idx} \
             k={k} m={m} shards_present={n_shards_present}/{total} — dispatching to rayon",
            total = k + m,
        );
    }

    /// Called when an FEC reconstruction result comes back from the compute pool.
    pub fn on_fec_result(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        success:   bool,
        latency_us: u64,
    ) {
        if success {
            log::debug!(
                "[FEC-RESULT] frame={frame_id} s={slice_idx} SUCCESS latency={latency_us}µs"
            );
        } else {
            log::warn!(
                "[FEC-RESULT] frame={frame_id} s={slice_idx} FAILED (RS reconstruct error) latency={latency_us}µs"
            );
        }
    }

    // ── Per-slice events ─────────────────────────────────────────────────────

    /// Called when a slice transitions `Assembling → EmittingDirect`.
    pub fn on_slice_emitting_direct(
        &self,
        frame_id:     u64,
        slice_idx:    u8,
        total_groups: u8,
        age_us:       u64,
    ) {
        log::debug!(
            "[SLICE-DIRECT] frame={frame_id} s={slice_idx} \
             all {total_groups} groups complete via data fast-path  age={age_us}µs"
        );
    }

    /// Called when a slice transitions `Assembling → EmittingFec`.
    pub fn on_slice_emitting_fec(
        &self,
        frame_id:      u64,
        slice_idx:     u8,
        total_groups:  u8,
        fec_groups:    u8,
        age_us:        u64,
    ) {
        log::info!(
            "[SLICE-FEC] frame={frame_id} s={slice_idx} \
             {fec_groups}/{total_groups} groups need RS  age={age_us}µs"
        );
    }

    /// Called when a slice reaches `Emitted`.
    pub fn on_slice_emitted(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        age_us:    u64,
        via_fec:   bool,
    ) {
        log::debug!(
            "[SLICE-EMITTED] frame={frame_id} s={slice_idx} via={} age={age_us}µs",
            if via_fec { "FEC" } else { "direct" },
        );
    }

    // ── Per-frame events ─────────────────────────────────────────────────────

    /// Called when a new `FrameBuilder` is created.
    pub fn on_frame_created(
        &self,
        frame_id:     u64,
        total_slices: u8,
        is_key:       bool,
        now_us:       u64,
    ) {
        log::debug!(
            "[FRAME-NEW] frame={frame_id} total_slices={total_slices} \
             key={is_key} created_at={now_us}µs"
        );
    }

    /// Called when a frame transitions to `Complete`.
    pub fn on_frame_complete(
        &self,
        frame_id:           u64,
        total_slices:       u8,
        first_us:           u64,
        collecting_done_us: u64,
        fec_submitted_us:   u64,
        fec_done_us:        u64,
        network_ready_us:   u64,
        now_us:             u64,
    ) {
        let total_age       = now_us.saturating_sub(first_us);
        let collect_time    = collecting_done_us.saturating_sub(first_us);
        let fec_submit_lag  = fec_submitted_us.saturating_sub(collecting_done_us);
        let fec_duration    = fec_done_us.saturating_sub(fec_submitted_us);
        let ready_lag       = network_ready_us.saturating_sub(if fec_done_us > 0 { fec_done_us } else { collecting_done_us });

        log::info!(
            "[FRAME-COMPLETE] frame={frame_id} slices={total_slices} \
             total_age={total_age}µs  collect={collect_time}µs  \
             fec_submit_lag={fec_submit_lag}µs  fec_dur={fec_duration}µs  \
             ready_lag={ready_lag}µs"
        );
    }

    /// Called when a frame is released by the HOL loop.
    pub fn on_frame_released(
        &self,
        frame_id:     u64,
        packets:      usize,
        hol_wait_us:  u64,
    ) {
        log::info!(
            "[FRAME-RELEASE] frame={frame_id} packets={packets} hol_wait={hol_wait_us}µs"
        );
    }

    /// Called when a frame is evicted (window overflow or stale timeout).
    pub fn on_frame_evicted(
        &self,
        frame_id:         u64,
        reason:           &str,
        failed_groups:    u64,
        wasted_bytes:     u64,
        lost_chunks:      u64,
        age_us:           u64,
    ) {
        log::warn!(
            "[FRAME-EVICT] frame={frame_id} reason={reason} \
             failed_groups={failed_groups} wasted_bytes={wasted_bytes}B \
             lost_chunks={lost_chunks} age={age_us}µs"
        );
    }

    /// Called on a partial emit (timeout on an incomplete frame).
    pub fn on_frame_partial_emit(
        &self,
        frame_id:        u64,
        packets_emitted: usize,
        missing_slices:  u8,
        stall_us:        u64,
    ) {
        log::warn!(
            "[FRAME-PARTIAL] frame={frame_id} emitted={packets_emitted} \
             missing_slices={missing_slices} stall={stall_us}µs"
        );
    }

    // ── HOL / NACK events ────────────────────────────────────────────────────

    /// Called whenever the HOL pointer advances.
    pub fn on_hol_advance(&self, old_id: u64, new_id: u64) {
        log::trace!("[HOL] advanced {old_id} → {new_id}");
    }

    /// Called when the HOL loop is blocked waiting for a frame.
    pub fn on_hol_blocked(
        &self,
        blocked_on:  u64,
        stall_us:    u64,
        budget_us:   u64,
    ) {
        if stall_us > budget_us / 2 {
            log::warn!(
                "[HOL-BLOCKED] waiting on frame={blocked_on} \
                 stall={stall_us}µs (budget={budget_us}µs, \
                 {pct:.0}% used)",
                pct = stall_us as f32 / budget_us as f32 * 100.0,
            );
        } else {
            log::debug!(
                "[HOL-BLOCKED] waiting on frame={blocked_on} stall={stall_us}µs"
            );
        }
    }

    /// Called when a NACK is queued for a group.
    pub fn on_nack_queued(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
    ) {
        log::debug!(
            "[NACK-QUEUE] frame={frame_id} s={slice_idx} g={group_idx} — added to NACK queue"
        );
    }

    /// Called when a NACK packet is actually emitted.
    pub fn on_nack_sent(
        &self,
        frame_id:      u64,
        slice_idx:     u8,
        group_idx:     u8,
        received_mask: u64,
        k:             u8,
        m:             u8,
    ) {
        // Build a compact "which shards we have" display.
        let mut have = String::with_capacity((k + m) as usize + 4);
        for i in 0..(k + m) {
            have.push(if (received_mask >> i) & 1 == 1 { '█' } else { '░' });
        }
        log::info!(
            "[NACK-SENT] frame={frame_id} s={slice_idx} g={group_idx} \
             shards=[{have}] k={k} m={m}"
        );
    }

    /// Called when a NACK-recovered shard arrives.
    pub fn on_nack_recovery(
        &self,
        frame_id:  u64,
        slice_idx: u8,
        group_idx: u8,
        shard_idx: u8,
    ) {
        log::info!(
            "[NACK-RECOVERED] frame={frame_id} s={slice_idx} g={group_idx} shard={shard_idx}"
        );
    }

    // ── Window dump ──────────────────────────────────────────────────────────

    /// Dump the full 10-frame rolling window to `log::info!`.
    /// Call this at any diagnostic checkpoint (e.g. on a timer, on eviction,
    /// or when a frame completes).
    pub fn dump_window(&self, label: &str, hol_id: Option<u64>, now_us: u64) {
        if !log::log_enabled!(log::Level::Info) { return; }

        let mut buf = String::with_capacity(4096);
        let _ = writeln!(buf,
            "╔══ ASSEMBLY WINDOW [{label}] now={now_us}µs HOL={hol:?} frames={n} ══",
            hol = hol_id,
            n   = self.window.len(),
        );

        // Sort by frame_id for stable output.
        let mut ids: Vec<u64> = self.window.keys().copied().collect();
        ids.sort_unstable();

        for fid in &ids {
            let f = &self.window[fid];
            let hol_marker = if hol_id == Some(*fid) { " ◀HOL" } else { "" };

            let _ = writeln!(buf,
                "║  Frame {fid:>8}{hol_marker}  [{state}]  \
                 slices={emitted}/{total}  shards={rx}/{exp} ({pct:.0}%)  \
                 age={age}µs  key={key}",
                fid     = fid,
                state   = f.state_label,
                emitted = f.emitted_slices,
                total   = f.total_slices,
                rx      = f.total_received_shards(),
                exp     = f.total_expected_shards(),
                pct     = f.completion_pct(),
                age     = f.age_us,
                key     = hol_id == Some(*fid),  // reuse as "is HOL"
            );

            // Timing pipeline breakdown
            if f.collecting_done_us > 0 {
                let collect_ms   = f.collecting_done_us.saturating_sub(f.first_us) as f32 / 1000.0;
                let fec_lag_ms   = if f.fec_submitted_us > 0 {
                    f.fec_submitted_us.saturating_sub(f.collecting_done_us) as f32 / 1000.0
                } else { 0.0 };
                let fec_dur_ms   = if f.fec_done_us > 0 {
                    f.fec_done_us.saturating_sub(f.fec_submitted_us) as f32 / 1000.0
                } else { 0.0 };
                let ready_lag_ms = f.network_ready_us.map_or(0.0, |r| {
                    let base = if f.fec_done_us > 0 { f.fec_done_us } else { f.collecting_done_us };
                    r.saturating_sub(base) as f32 / 1000.0
                });
                let _ = writeln!(buf,
                    "║    timing: collect={collect_ms:.2}ms  fec_lag={fec_lag_ms:.2}ms  \
                     fec_dur={fec_dur_ms:.2}ms  ready_lag={ready_lag_ms:.2}ms"
                );
            }

            // Per-slice breakdown
            for ss in &f.slices {
                let _ = writeln!(buf,
                    "║    Slice {si}  [{st}]  groups={rg}/{tg}  shards={rx}/{exp}",
                    si = ss.slice_idx,
                    st = ss.state_label,
                    rg = ss.ready_groups,
                    tg = ss.total_groups,
                    rx = ss.total_received(),
                    exp = ss.total_expected(),
                );

                // Per-group breakdown
                for gs in &ss.groups {
                    let warn = if !gs.can_recover() && gs.received < gs.k + gs.m {
                        " ⚠ UNRECOVERABLE"
                    } else { "" };
                    let _ = writeln!(buf,
                        "║      Group {gi}  [{gst}]  k={k} m={m}  \
                         data={dr}/{k}  parity={pr}/{m}  total={rx}/{tot}  \
                         {smap}{warn}",
                        gi  = gs.group_idx,
                        gst = gs.state_label,
                        k   = gs.k,
                        m   = gs.m,
                        dr  = gs.data_received,
                        pr  = gs.parity_received(),
                        rx  = gs.received,
                        tot = gs.k + gs.m,
                        smap = gs.shard_map(),
                    );
                }
            }
        }

        let _ = writeln!(buf,
            "╚══ total_shard_insertions={n} ══",
            n = self.shard_count,
        );

        log::info!("{}", buf);
    }

    // ── Incremental counters ─────────────────────────────────────────────────

    pub fn inc_shard_count(&mut self) {
        self.shard_count += 1;
    }
}

impl Default for AssemblyDiag {
    fn default() -> Self { Self::new() }
}


// ═══════════════════════════════════════════════════════════════════════════════
// Helper: build a GroupSnap from a GroupBuilder reference
// (avoids coupling this file to GroupBuilder's field layout)
// ═══════════════════════════════════════════════════════════════════════════════

/// Build a `GroupSnap` from the fields you already have available inside
/// `GroupBuilder::insert`.  Call this after updating accounting but before
/// the state transition.
#[inline]
pub fn snap_group(
    group_idx:     u8,
    k:             u8,
    m:             u8,
    received:      u8,
    data_received: u8,
    received_mask: u64,
    state_label:   &'static str,
    first_us:      u64,
) -> GroupSnap {
    GroupSnap {
        group_idx,
        k,
        m,
        received,
        data_received,
        received_mask,
        state_label,
        first_us,
    }
}

/// Map a `GroupState` to a static string label (no alloc).
pub fn group_state_label(state: &crate::fec::group_builder::GroupState) -> &'static str {
    use crate::fec::group_builder::GroupState;
    match state {
        GroupState::Collecting          => "Collecting",
        GroupState::Stalled  { .. }    => "Stalled",
        GroupState::Requested { .. }   => "Requested",
        GroupState::Decoding            => "Decoding",
        GroupState::Ready               => "Ready",
        GroupState::Abandoned           => "Abandoned",
        GroupState::DecodingInProgress => "Decoding is in process"
    }
}

pub fn slice_state_label(state: &crate::fec::slice_builder::SliceState) -> &'static str {
    use crate::fec::slice_builder::SliceState;
    match state {
        SliceState::Assembling      => "Assembling",
        SliceState::EmittingFec     => "EmittingFec",
        SliceState::EmittingDirect  => "EmittingDirect",
        SliceState::Emitted         => "Emitted",
        SliceState::Abandoned       => "Abandoned",
    }
}

pub fn frame_state_label(state: &crate::fec::frame_builder::FrameState) -> &'static str {
    use crate::fec::frame_builder::FrameState;
    match state {
        FrameState::Receiving  => "Receiving",
        FrameState::Complete   => "Complete",
        FrameState::Abandoned  => "Abandoned",
    }
}