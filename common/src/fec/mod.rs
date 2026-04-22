pub mod assembler;
pub mod decode;
pub mod encode;
pub mod builder;
pub mod stats;

pub(crate) use std::collections::HashMap;

/// Minimum interval between NACKs for the same (frame, slice, group) triple.
pub(crate) const NACK_SUPPRESS_US: u64 = 8_000;

/// Do NOT fire a NACK for a frame whose first shard arrived less than this
/// long ago.  Shards are paced by the sender and will still be in flight.
pub(crate) const MIN_NACK_AGE_US: u64 = 5_000;
