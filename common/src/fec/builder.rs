use std::collections::{BTreeMap};

use crate::{
    DatagramChunk, FrameTrace, NaluType, VideoPacket, VideoSlice,
    fec::decode::FecDecoder,
};

use super::*;

pub(crate) struct FrameBuilder {
    pub(crate) slices: HashMap<u8, SliceBuilder>,
    decoded_slices: BTreeMap<u8, VideoSlice>,
    pub(crate) total_slices: u8,
    pub(crate) is_key: bool,
    pub(crate) first_us: u64,
    keyframe_emitted: bool,
    next_emit_idx: u8,
}

impl FrameBuilder {
    pub(crate) fn new(total_slices: u8, is_key: bool) -> Self {
        Self {
            slices: HashMap::new(),
            decoded_slices: BTreeMap::new(),
            total_slices,
            is_key,
            first_us: FrameTrace::now_us(),
            keyframe_emitted: false,
            next_emit_idx: 0,
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        if self.is_key {
            self.keyframe_emitted
        } else {
            self.decoded_slices.len() as u8 == self.total_slices
        }
    }

    pub(crate) fn insert_chunk(&mut self, chunk: &DatagramChunk) -> Option<VideoPacket> {
        if self.keyframe_emitted {
            return None;
        }

        let slice_idx = chunk.slice_idx;

        {
            let slice_builder = self
                .slices
                .entry(slice_idx)
                .or_insert_with(|| SliceBuilder::new(chunk.k, chunk.m));

            slice_builder.insert(chunk);

            if !slice_builder.is_ready() {
                return None;
            }
        }

        if self.decoded_slices.contains_key(&slice_idx) {
            return None;
        }

        let video_slice = self.decode_slice(slice_idx)?;
        self.decoded_slices.insert(slice_idx, video_slice);

        // ── KEYFRAME ─────────────────────────────────────────────
        if self.is_key {
            if self.decoded_slices.len() as u8 == self.total_slices {
                self.keyframe_emitted = true;
                return self.assemble_keyframe(chunk.frame_id);
            }
            return None;
        }

        // ── NON-KEY: ordered emit ────────────────────────────────
        self.try_emit_ordered(chunk.frame_id)
    }

    fn decode_slice(&mut self, slice_idx: u8) -> Option<VideoSlice> {
        let slice_builder = self.slices.get_mut(&slice_idx)?;
        let recovered_bytes = slice_builder.get_data()?;

        let (video_slice, _remainder): (VideoSlice, _) =
            postcard::take_from_bytes(&recovered_bytes).ok()?;

        Some(video_slice)
    }
    fn try_emit_ordered(&mut self, frame_id: u64) -> Option<VideoPacket> {
        let slice = self.decoded_slices.remove(&self.next_emit_idx)?;

        // ⚠️ КРИТИЧНО: только если это действительно slice payload
        if !matches!(slice.nal_type, NaluType::SliceTrailing) {
            // пропускаем, но двигаем индекс
            self.next_emit_idx += 1;
            return self.try_emit_ordered(frame_id);
        }

        let packet = Self::slice_to_packet(slice, self.first_us);

        self.next_emit_idx += 1;

        Some(packet)
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

    fn assemble_keyframe(&mut self, frame_id: u64) -> Option<VideoPacket> {
        let mut ordered: Vec<VideoSlice> = self.decoded_slices.values().cloned().collect();
        ordered.sort_by_key(|s| (nalu_priority(s.nal_type), s.slice_idx));

        let mut full_payload = Vec::new();
        let mut final_trace: Option<FrameTrace> = None;

        for slice in ordered {
            if final_trace.is_none() {
                final_trace = slice.trace;
            }
            full_payload.extend_from_slice(&slice.payload);
        }

        if let Some(ref mut t) = final_trace {
            t.receive_us = self.first_us;
            t.reassembled_us = FrameTrace::now_us();
        }

        Some(VideoPacket {
            frame_id,
            payload: full_payload,
            slice_idx: 0,
            is_key: true,
            is_last: true,
            trace: final_trace,
        })
    }
}

fn nalu_priority(t: NaluType) -> u8 {
    match t {
        NaluType::VideoParamSet => 0,
        NaluType::SeqParamSet   => 1,
        NaluType::PicParamSet   => 2,
        NaluType::Sei           => 3,
        NaluType::SliceIdr      => 4,
        NaluType::SliceTrailing  => 5,
        NaluType::Other(_)      => 6,
    }
}

pub(crate) struct SliceBuilder {
    shards: Vec<Option<Vec<u8>>>,
    payload_lens: Vec<u16>,
    received: u8,
    k: u8,
    m: u8,
}

impl SliceBuilder {
    fn new(k: u8, m: u8) -> Self {
        Self {
            shards: vec![None; (k + m) as usize],
            payload_lens: vec![0; k as usize],
            received: 0,
            k,
            m,
        }
    }

    fn insert(&mut self, chunk: &DatagramChunk) {
        let idx = chunk.shard_idx as usize;

        if idx < self.shards.len() && self.shards[idx].is_none() {
            self.shards[idx] = Some(chunk.data.to_vec());

            if idx < self.k as usize {
                self.payload_lens[idx] = chunk.payload_len;
            }

            self.received += 1;
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.received >= self.k
    }

    pub(crate) fn received_mask(&self) -> u64 {
        let mut mask = 0u64;
        for (i, shard) in self.shards.iter().enumerate() {
            if shard.is_some() {
                mask |= 1u64 << i;
            }
        }
        mask
    }

    fn get_data(&mut self) -> Option<Vec<u8>> {
        FecDecoder::decode(self.k, self.m, self.shards.clone(), &self.payload_lens)
    }
}