use serde::{Serialize, Deserialize};
use bytes::{Bytes, BytesMut, BufMut};
use std::{sync::atomic::AtomicI64, time::{SystemTime, UNIX_EPOCH}};

pub mod fec;
pub const TYPE_VIDEO: u8 = 0;
pub const TYPE_AUDIO: u8 = 1;
pub const TYPE_CONTROL: u8 = 2;
pub static CLOCK_OFFSET: AtomicI64 = AtomicI64::new(0);
// ─────────────────────────────────────────────────────────────────────────────
// Wire types shared between sender and receiver
// ─────────────────────────────────────────────────────────────────────────────
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct FrameTrace {
    pub capture_us:    u64,
    pub encode_us:     u64,
    pub serialize_us:   u64,
    pub receive_us:    u64,
    pub reassembled_us: u64,
    pub decode_us:      u64,
    pub present_us:     u64,
}

impl FrameTrace {
    pub fn now_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    pub fn ms(from: u64, to: u64) -> f64 {
        (to.saturating_sub(from) as f64) / 1000.0
    }
}
/// A fully-assembled, serialised HEVC frame.
///
/// Serialised once on the sender with postcard, then chunked into QUIC
/// datagrams.  On the receiver the chunks are reassembled and deserialised
/// back into this struct.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct VideoPacket {
    pub frame_id:  u64,
    pub payload:   Vec<u8>,
    pub slice_idx:    u8,
    pub is_key:    bool,
    pub is_last:   bool,
    pub trace:     Option<FrameTrace>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VideoSlice {
    pub frame_id:     u64,
    pub slice_idx:    u8,
    pub total_slices: u8,
    pub is_key:       bool,
    pub is_last:      bool,
    pub payload:      Vec<u8>,
    pub trace:        Option<FrameTrace>,
    pub nal_type:     NaluType,
}
/// One QUIC-datagram fragment of a serialised `VideoPacket`.
///
/// # Wire format (fixed binary, NOT postcard)
///
/// ```text
/// ┌──────────────┬────────────┬──────────────┬──────────────────┐
/// │ frame_id: 8B │ idx: 2B LE │ total: 2B LE │ data: variable   │
/// └──────────────┴────────────┴──────────────┴──────────────────┘
/// ```
///
/// Using a fixed 12-byte header instead of postcard varint encoding saves
/// ~5-15 bytes per datagram and eliminates a serialisation step on the
/// per-datagram hot path.
///
/// # Reassembly contract
/// - Collect chunks by `frame_id` into a `HashMap`.
/// - When `received == total_chunks`, concatenate `data` fields ordered by
///   `chunk_idx` and deserialise the result as a `VideoPacket` via postcard.
/// - If a newer `frame_id` arrives before the current one is complete, evict
///   the stale entry (its P-frame data was already dropped by the network).
#[derive(Debug, Default)]
pub struct DatagramChunk {
    pub frame_id:     u64,
    pub slice_idx:    u8,  // Номер NALU (0..7)
    pub total_slices: u8,  // Всего NALU в кадре (чтобы ресивер знал, когда кадр собран)
    pub shard_idx:    u8,  // Номер шарда внутри этого слайса (0 .. k+m-1)
    pub k:            u8,  // Количество шардов данных
    pub m:            u8,  // Количество шардов четности
    pub payload_len:  u16, // Реальный размер данных (нужно для отрезания нулей из-за паддинга FEC)
    pub packet_type:  u8,
    pub flags:        u8,
    pub group_idx:    u8,
    pub total_groups: u8,
    pub data:         Bytes,
}

impl DatagramChunk {
    /// Fixed header size in bytes.
    pub const HEADER_LEN: usize = 19; // 8 + 1 + 1 + 1 + 1 + 1 + 2 + 1 + 1 + 1 + 1

    // ── Encoding ─────────────────────────────────────────────────────────────

    /// Serialise a chunk into a `Bytes` buffer using the fixed binary header.
    ///
    /// `data` is appended verbatim after the 12-byte header — no extra copy
    /// is required if the caller already holds a contiguous `&[u8]`.
    #[inline]
    pub fn encode(
        frame_id: u64, slice_idx: u8, total_slices: u8, shard_idx: u8,
        k: u8, m: u8, payload_len: u16, p_type: u8, flags: u8, group_idx: u8, total_groups: u8, data: &[u8]
    ) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + data.len());
        buf.put_u64_le(frame_id);
        buf.put_u8(slice_idx);
        buf.put_u8(total_slices);
        buf.put_u8(shard_idx);
        buf.put_u8(k);
        buf.put_u8(m);
        buf.put_u16_le(payload_len);
        buf.put_u8(p_type);
        buf.put_u8(flags);
        buf.put_u8(group_idx);
        buf.put_u8(total_groups);
        buf.put_slice(data);
        buf.freeze()
    }

    // ── Decoding ─────────────────────────────────────────────────────────────

    /// Deserialise from a raw QUIC datagram.
    ///
    /// Returns `None` if the buffer is shorter than [`HEADER_LEN`].
    /// The `data` field is a **zero-copy** `Bytes` slice of the input — no
    /// heap allocation for the payload bytes.
    #[inline]
    pub fn decode(raw: Bytes) -> Option<Self> {
        if raw.len() < Self::HEADER_LEN { return None; }
        Some(Self {
            frame_id:     u64::from_le_bytes(raw[0..8].try_into().unwrap()),
            slice_idx:    raw[8],
            total_slices: raw[9],
            shard_idx:    raw[10],
            k:            raw[11],
            m:            raw[12],
            payload_len:  u16::from_le_bytes(raw[13..15].try_into().unwrap()),
            packet_type:  raw[15],
            flags:        raw[16],
            group_idx:    raw[17],
            total_groups: raw[18],
            data:         raw.slice(Self::HEADER_LEN..),
        })
    }

    #[inline]
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + self.data.len());
        buf.put_u64_le(self.frame_id);
        buf.put_u8(self.slice_idx);
        buf.put_u8(self.total_slices);
        buf.put_u8(self.shard_idx);
        buf.put_u8(self.k);
        buf.put_u8(self.m);
        buf.put_u16_le(self.payload_len);
        buf.put_u8(self.packet_type);
        buf.put_u8(self.flags);
        buf.put_u8(self.group_idx);
        buf.put_u8(self.total_groups);
        buf.put_slice(&self.data);
        buf.freeze()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ControlPacket {
    Identify { 
        model: String, 
        os: String,  
    },
    StartStreaming,
    RequestKeyFrame,
    Ping { client_time_us: u64 },
    Pong { client_time_us: u64, server_time_us: u64 },
    OffsetUpdate { offset_us: i64, rtt_us: u64 },
    FrameFeedback { frame_id: u64, trace: FrameTrace },
    Communication {message: String },
    Nack {
        frame_id:      u64,
        slice_idx:     u8,
        group_idx: u8,
        /// Bitmask: bit `i` is set when shard `i` has been received.
        /// The sender retransmits shards whose bits are *clear*.
        received_mask: u64,
    },
    LostFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Amd,
    Intel,
    Nvidia,
    Unknown,
}

/// Reads `/sys/class/drm/card*/device/vendor` THE FIRST ONE IS CHOSEN
pub fn detect_gpu_vendor() -> GpuVendor {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        for i in 0..4 {
            let path = format!("/sys/class/drm/card{i}/device/vendor");
            if let Ok(raw) = fs::read_to_string(&path) {
                match raw.trim().to_lowercase().as_str() {
                    "0x1002" => return GpuVendor::Amd,
                    "0x8086" => return GpuVendor::Intel,
                    "0x10de" => return GpuVendor::Nvidia,
                    _ => {}
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let path = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
        
        if let Ok(class_key) = hklm.open_subkey(path) {
            for name in class_key.enum_keys().filter_map(|x| x.ok()) {
                if let Ok(sub_key) = class_key.open_subkey(&name) {
                    if let Ok(device_id) = sub_key.get_value::<String, _>("MatchingDeviceId") {
                        let id = device_id.to_uppercase();
                        if id.contains("VEN_1002") { return GpuVendor::Amd; }
                        if id.contains("VEN_8086") { return GpuVendor::Intel; }
                        if id.contains("VEN_10DE") { return GpuVendor::Nvidia; }
                    }
                }
            }
        }
    }

    GpuVendor::Unknown
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub enum NaluType {
    VideoParamSet,    // VPS (32)
    SeqParamSet,      // SPS (33)
    PicParamSet,      // PPS (34)
    Sei,              // Supplemental Info (38-39)
    SliceIdr,         // Ключевой кадр (19-20)
    SliceTrailing,    // Обычный P-кадр (1)
    Other(u8),        // Все остальное
}