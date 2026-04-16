use super::*;

pub(crate) async fn run_serialiser_task(
    mut frame_rx: mpsc::Receiver<EncodedFrame>,
    broadcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
    shard_cache: Arc<ShardCache>,
) {
    let mut frame_id = 0u64;
    
    // Calculate a safe max chunk size for the broadcast.
    // Quinn's initial_mtu is 1200, so we use that as our baseline constraint.
    let max_dgram = 1200;
    let max_chunk_data = max_dgram - DatagramChunk::HEADER_LEN - 8;

    while let Some(mut frame) = frame_rx.recv().await {
        frame_id += 1;
        frame.frame_id = frame_id;

        if let Some(t) = frame.trace.as_mut() {
            t.serialize_us = FrameTrace::now_us(); 
        }

        let total_slices = frame.slices.len() as u8;
        if total_slices == 0 {
            continue;
        }

        let mut datagrams = Vec::new();

        for (s_idx, (slice_data, is_critical)) in frame.slices.iter().enumerate() {
            // 1. Calculate flags
            let mut flags = if frame.is_key { 1 } else { 0 };
            if *is_critical { flags |= 2; }

            // 2. Wrap payload
            let slice = VideoSlice {
                frame_id: frame.frame_id,
                slice_idx: s_idx as u8,
                total_slices,
                is_key: frame.is_key,
                payload: slice_data.to_vec(),

                // We embed our trace in the first slice
                trace: if s_idx == 0 { frame.trace.clone() } else { None },
            };

            // 3. Serialize using postcard
            let serialized = postcard::to_allocvec(&slice).unwrap_or_default();

            // 4. FEC Encoding
            let chunks = common::fec::FecEncoder::encode_slice(
                frame.frame_id,
                s_idx as u8,
                total_slices,
                &serialized, 
                max_chunk_data,
                flags
            );

            // 5. Convert to Bytes instantly
            for chunk in chunks {
                datagrams.push(chunk.to_bytes());
            }
        }

        // 6. Broadcast the pre-computed datagrams
        let shared_frame = Arc::new(SerializedFrame {
            frame_id: frame.frame_id,
            is_key: frame.is_key,
            datagrams,
        });
        shard_cache.insert(shared_frame.frame_id, &shared_frame.datagrams);
        if let Err(_) = broadcast_tx.send(shared_frame) {
            // No active subscribers, safely ignore.
        }
    }
}