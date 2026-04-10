use serde::{Serialize, Deserialize};
use bytes::{Bytes, BytesMut, BufMut};
use std::{sync::atomic::AtomicI64, time::{SystemTime, UNIX_EPOCH}};


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
#[derive(Serialize, Deserialize, Debug)]
pub struct VideoPacket {
    pub frame_id:  u64,
    pub payload:   Vec<u8>,
    pub is_key:    bool,
    pub trace:     Option<FrameTrace>,
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
#[derive(Debug)]
pub struct DatagramChunk {
    pub frame_id:     u64,
    pub chunk_idx:    u16,
    pub total_chunks: u16,
    pub flags:        u8,
    pub packet_type:  u8,
    /// Zero-copy Bytes slice of the raw datagram payload.
    pub data:         Bytes,
}

impl DatagramChunk {
    /// Fixed header size in bytes.
    pub const HEADER_LEN: usize = 14; // 8 + 2 + 2 + 1 + 1

    // ── Encoding ─────────────────────────────────────────────────────────────

    /// Serialise a chunk into a `Bytes` buffer using the fixed binary header.
    ///
    /// `data` is appended verbatim after the 12-byte header — no extra copy
    /// is required if the caller already holds a contiguous `&[u8]`.
    #[inline]
    pub fn encode(frame_id: u64, idx: u16, total: u16, p_type: u8, flags: u8, data: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + data.len());
        buf.put_u64_le(frame_id);
        buf.put_u16_le(idx);
        buf.put_u16_le(total);
        buf.put_u8(p_type); // Записываем тип
        buf.put_u8(flags);  // Записываем флаги
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
        if raw.len() < Self::HEADER_LEN {
            return None;
        }
        
        // Читаем строго по индексам:
        let frame_id     = u64::from_le_bytes(raw[0..8].try_into().ok()?);    // 0..8
        let chunk_idx    = u16::from_le_bytes(raw[8..10].try_into().ok()?);   // 8..10
        let total_chunks = u16::from_le_bytes(raw[10..12].try_into().ok()?);  // 10..12
        let packet_type  = raw[12];                                           // 12
        let flags        = raw[13];                                           // 13
        
        let data         = raw.slice(Self::HEADER_LEN..);
        
        Some(Self { 
            frame_id, 
            chunk_idx, 
            total_chunks, 
            packet_type, 
            flags, 
            data 
        })
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
}