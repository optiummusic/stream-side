use serde::{Serialize, Deserialize};
use bytes::{Bytes, BytesMut, BufMut};

// ─────────────────────────────────────────────────────────────────────────────
// Wire types shared between sender and receiver
// ─────────────────────────────────────────────────────────────────────────────

/// A fully-assembled, serialised HEVC frame.
///
/// Serialised once on the sender with postcard, then chunked into QUIC
/// datagrams.  On the receiver the chunks are reassembled and deserialised
/// back into this struct.
#[derive(Serialize, Deserialize, Debug)]
pub struct VideoPacket {
    pub frame_id:  u64,
    pub payload:   Vec<u8>,
    pub timestamp: u64,
    pub is_key:    bool,
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
    /// Zero-copy Bytes slice of the raw datagram payload.
    pub data:         Bytes,
}

impl DatagramChunk {
    /// Fixed header size in bytes.
    pub const HEADER_LEN: usize = 12; // 8 + 2 + 2

    // ── Encoding ─────────────────────────────────────────────────────────────

    /// Serialise a chunk into a `Bytes` buffer using the fixed binary header.
    ///
    /// `data` is appended verbatim after the 12-byte header — no extra copy
    /// is required if the caller already holds a contiguous `&[u8]`.
    #[inline]
    pub fn encode(frame_id: u64, chunk_idx: u16, total_chunks: u16, data: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + data.len());
        buf.put_u64_le(frame_id);
        buf.put_u16_le(chunk_idx);
        buf.put_u16_le(total_chunks);
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
        Some(Self { frame_id, chunk_idx, total_chunks, data })
    }
}
