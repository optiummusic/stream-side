use serde::{Serialize, Deserialize};
use bytes::{Bytes, BytesMut, BufMut};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub is_key:       bool,
    /// Zero-copy Bytes slice of the raw datagram payload.
    pub data:         Bytes,
}

impl DatagramChunk {
    /// Fixed header size in bytes.
    pub const HEADER_LEN: usize = 13; // 8 + 2 + 2 + 1

    // ── Encoding ─────────────────────────────────────────────────────────────

    /// Serialise a chunk into a `Bytes` buffer using the fixed binary header.
    ///
    /// `data` is appended verbatim after the 12-byte header — no extra copy
    /// is required if the caller already holds a contiguous `&[u8]`.
    #[inline]
    pub fn encode(frame_id: u64, chunk_idx: u16, total_chunks: u16, data: &[u8], is_key: bool,) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + data.len());
        buf.put_u64_le(frame_id);
        buf.put_u16_le(chunk_idx);
        buf.put_u16_le(total_chunks);
        buf.put_u8(u8::from(is_key));
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
        let frame_id     = u64::from_le_bytes(raw[0..8].try_into().ok()?);
        let chunk_idx    = u16::from_le_bytes(raw[8..10].try_into().ok()?);
        let total_chunks = u16::from_le_bytes(raw[10..12].try_into().ok()?);
        let data         = raw.slice(Self::HEADER_LEN..);
        let is_key       = raw[12] != 0;
        Some(Self { frame_id, chunk_idx, total_chunks, is_key, data })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ControlPacket {
    Ping { client_time_us: u64 },
    Pong { client_time_us: u64, server_time_us: u64 },
}