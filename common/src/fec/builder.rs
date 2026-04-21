use std::collections::HashMap;

use bytes::Bytes;

use crate::{
    fec::decode::FecDecoder,
    DatagramChunk,
    FrameTrace,
    VideoPacket,
    VideoSlice,
};

use super::*;

// ── FrameBuilder ─────────────────────────────────────────────────────────────

pub(crate) enum GroupRecovery {
    Direct,
    Fec { missing_data_shards: u8 },
}

pub(crate) struct FrameBuilder {
    pub(crate) slices: HashMap<u8, SliceBuilder>,
    decoded_slices: Vec<Option<VideoSlice>>,
    pub(crate) total_slices: u8,
    pub(crate) first_us: u64,
    next_emit_idx: u8,
    /// Packets that have been decoded and are waiting for the HOL flush loop
    /// to release them in order.  Populated incrementally as slices complete;
    /// drained all at once by [`take_packets`] when the frame is complete.
    ready_packets: Vec<VideoPacket>,
}

impl FrameBuilder {
    pub(crate) fn new(total_slices: u8, _is_key: bool) -> Self {
        let mut decoded_slices = Vec::with_capacity(total_slices as usize);
        decoded_slices.resize_with(total_slices as usize, || None);

        Self {
            slices: HashMap::new(),
            decoded_slices,
            total_slices,
            first_us: FrameTrace::now_us(),
            next_emit_idx: 0,
            ready_packets: Vec::new(),
        }
    }

    /// Returns `true` once every slice has been decoded and staged in
    /// `ready_packets`.
    pub(crate) fn is_complete(&self) -> bool {
        self.next_emit_idx == self.total_slices
    }

    pub(crate) fn count_wasted(&self) -> (u64, u64, u64) {
        let mut failed_groups = 0;
        let mut wasted_payload_bytes = 0;
        let mut lost_chunks = 0;

        for sb in self.slices.values() {
            for gb in sb.groups.values() {
                if !gb.is_ready() {
                    failed_groups += 1;
                    // Считаем сколько шардов (data+parity) не дошло в этой группе
                    let total_expected = (gb.k + gb.m) as u64;
                    lost_chunks += total_expected.saturating_sub(gb.received as u64);
                }
                
                for i in 0..(gb.k as usize) {
                    if gb.shards[i].is_some() {
                        wasted_payload_bytes += gb.payload_lens[i] as u64;
                    }
                }
            }
        }

        (failed_groups, wasted_payload_bytes, lost_chunks)
    }

    /// Insert a chunk into this builder.
    ///
    /// Unlike the old API this method no longer returns packets directly;
    /// decoded packets are accumulated in `ready_packets` and released only
    /// when the [`FrameAssembler`]'s HOL flush loop decides it is this
    /// frame's turn.
    pub(crate) fn insert_chunk(&mut self, chunk: &DatagramChunk) -> Vec<GroupRecovery> {
        let slice_idx = chunk.slice_idx as usize;
        if slice_idx >= self.decoded_slices.len() {
            return Vec::new();
        }

        let mut recoveries = Vec::new();

        let ready = {
            let sb = self
                .slices
                .entry(chunk.slice_idx)
                .or_insert_with(|| SliceBuilder::new(chunk.total_groups));

            if let Some(recovery) = sb.insert(chunk) {
                recoveries.push(recovery);
            }

            sb.is_ready()
        };

        if !ready || self.decoded_slices[slice_idx].is_some() {
            return recoveries;
        }

        if let Some(video_slice) = self.decode_slice(chunk.slice_idx) {
            self.decoded_slices[slice_idx] = Some(video_slice);
        }

        let new_packets = self.try_emit_ordered();
        self.ready_packets.extend(new_packets);

        recoveries
    }

    /// Drain all staged packets if the frame is fully assembled.
    ///
    /// Returns `None` if the frame is still incomplete.  After returning
    /// `Some(_)` the caller should remove this builder from the map.
    pub(crate) fn take_packets(&mut self) -> Option<Vec<VideoPacket>> {
        if self.is_complete() {
            Some(std::mem::take(&mut self.ready_packets))
        } else {
            None
        }
    }

    fn decode_slice(&self, slice_idx: u8) -> Option<VideoSlice> {
        let sb = self.slices.get(&slice_idx)?;
        let data = sb.get_data()?;
        let (video_slice, _): (VideoSlice, _) = postcard::take_from_bytes(&data).ok()?;
        Some(video_slice)
    }

    /// Advance `next_emit_idx` as far as possible in slice order and return
    /// the newly-unlocked packets.  Called internally; results are staged in
    /// `ready_packets`, not returned to the assembler directly.
    pub(crate) fn try_emit_ordered(&mut self) -> Vec<VideoPacket> {
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

    fn slice_to_packet(slice: VideoSlice, first_us: u64) -> VideoPacket {
        let mut trace = slice.trace;
        if let Some(ref mut t) = trace {
            t.receive_us = first_us;
            t.reassembled_us = FrameTrace::now_us();
        }

        VideoPacket {
            frame_id: slice.frame_id,
            payload: slice.payload,
            slice_idx: slice.slice_idx,
            is_key: slice.is_key,
            is_last: slice.is_last,
            trace,
        }
    }
}

// ── SliceBuilder ─────────────────────────────────────────────────────────────

pub(crate) struct SliceBuilder {
    pub(crate) groups: HashMap<u8, GroupBuilder>,
    pub(crate) total_groups: u8,
    ready_groups: u8,
}

impl SliceBuilder {
    pub(crate) fn new(total_groups: u8) -> Self {
        Self {
            groups: HashMap::new(),
            total_groups: total_groups.max(1),
            ready_groups: 0,
        }
    }

    pub(crate) fn insert(&mut self, chunk: &DatagramChunk) -> Option<GroupRecovery> {
        if chunk.total_groups > 0 {
            self.total_groups = chunk.total_groups;
        }

        let group = self
            .groups
            .entry(chunk.group_idx)
            .or_insert_with(|| GroupBuilder::new(chunk.k, chunk.m));

        let was_ready = group.is_ready();
        group.insert(chunk.shard_idx, chunk.data.clone(), chunk.payload_len);

        if !was_ready && group.is_ready() {
            self.ready_groups = self.ready_groups.saturating_add(1);

            let data_present = group.data_received_count();
            let missing_data = group.k.saturating_sub(data_present);

            return Some(if missing_data == 0 {
                GroupRecovery::Direct
            } else {
                GroupRecovery::Fec {
                    missing_data_shards: missing_data,
                }
            });
        }

        None
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready_groups == self.total_groups
    }

    pub(crate) fn group_ready_mask(&self) -> u64 {
        let mut mask = 0u64;
        for g in 0..self.total_groups.min(64) {
            if self.groups.get(&g).is_some_and(|gb| gb.is_ready()) {
                mask |= 1u64 << g;
            }
        }
        mask
    }

    pub(crate) fn group_received_mask(&self, group_idx: u8) -> u64 {
        self.groups
            .get(&group_idx)
            .map_or(0, |gb| gb.received_mask())
    }

    pub(crate) fn get_data(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for g in 0..self.total_groups {
            let part = self.groups.get(&g)?.get_data()?;
            out.extend_from_slice(&part);
        }
        Some(out)
    }
}

// ── GroupBuilder ─────────────────────────────────────────────────────────────

pub(crate) struct GroupBuilder {
    shards: Vec<Option<Bytes>>,
    payload_lens: Vec<u16>,
    received: u8,
    received_mask: u64,
    pub(crate) k: u8,
    pub(crate) m: u8,
}

impl GroupBuilder {
    pub(crate) fn new(k: u8, m: u8) -> Self {
        let mut shards = Vec::with_capacity((k + m) as usize);
        shards.resize_with((k + m) as usize, || None);

        Self {
            shards,
            payload_lens: vec![0u16; k as usize],
            received: 0,
            received_mask: 0,
            k,
            m,
        }
    }

    pub(crate) fn insert(&mut self, idx: u8, data: Bytes, payload_len: u16) {
        let i = idx as usize;
        if i >= self.shards.len() {
            return;
        }

        if self.shards[i].is_none() {
            self.received = self.received.saturating_add(1);
            if i < 64 {
                self.received_mask |= 1u64 << i;
            }
        }

        self.shards[i] = Some(data);

        if i < self.k as usize {
            self.payload_lens[i] = payload_len;
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.received >= self.k
    }

    pub(crate) fn received_mask(&self) -> u64 {
        self.received_mask
    }

    pub(crate) fn get_data(&self) -> Option<Vec<u8>> {
        FecDecoder::decode(self.k, self.m, &self.shards, &self.payload_lens)
    }

    pub(crate) fn data_received_count(&self) -> u8 {
        let mut c = 0u8;
        for i in 0..self.k as usize {
            if self.shards[i].is_some() {
                c += 1;
            }
        }
        c
    }
}