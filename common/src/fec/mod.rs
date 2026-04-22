

#[cfg(feature = "sender")]
pub mod encode;

#[cfg(feature = "receiver")]
pub mod assembler;
#[cfg(feature = "receiver")]
pub mod decode;
#[cfg(feature = "receiver")]
pub mod decode_task;
#[cfg(feature = "receiver")]
pub mod stats;
#[cfg(feature = "receiver")]
pub mod frame_builder;
#[cfg(feature = "receiver")]
pub mod slice_builder;
#[cfg(feature = "receiver")]
pub mod group_builder;

pub(crate) use std::collections::HashMap;

/// Minimum interval between NACKs for the same (frame, slice, group) triple.
#[cfg(feature = "receiver")]
pub(crate) const NACK_SUPPRESS_US: u64 = 8_000;

/// Do NOT fire a NACK for a frame whose first shard arrived less than this
/// long ago.  Shards are paced by the receiver and will still be in flight.
#[cfg(feature = "receiver")]
pub(crate) const MIN_NACK_AGE_US: u64 = 5_000;

#[cfg(feature = "receiver")]
pub(crate) enum GroupRecovery {
    Direct,
    Fec { missing_data_shards: u8, via_parity: bool },
}

pub(crate) use bytes::Bytes;
use std::sync::{Arc, Mutex};
