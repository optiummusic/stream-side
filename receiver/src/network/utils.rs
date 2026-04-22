use super::*;
const OFFSET_ALPHA: f64 = 0.01;

pub(crate) async fn send_offset_update(conn: &quinn::Connection, rtt_us: u64) -> Result<(), Box<dyn std::error::Error>> {
    let offset = CLOCK_OFFSET.load(Ordering::Relaxed);
    let packet = ControlPacket::OffsetUpdate { offset_us: offset, rtt_us };

    let bin = postcard::to_stdvec(&packet)?;
    let dgram = DatagramChunk{
        payload_len: bin.len() as u16, 
        packet_type: TYPE_CONTROL, 
        data: bin.into(),
        ..Default::default()
    }.to_bytes();
    conn.send_datagram(dgram)?;

    Ok(())
}

pub(crate) fn process_control_feedback(ctrl: ControlPacket) -> Option<(i64, u64)> {
    if let ControlPacket::Pong { client_time_us, server_time_us } = ctrl {
        let t2 = FrameTrace::now_us();
        let rtt = t2.saturating_sub(client_time_us);

        let new_raw_offset = (server_time_us as i64) - (client_time_us + rtt / 2) as i64;
        let current_offset = CLOCK_OFFSET.load(Ordering::Relaxed);

        let filtered_offset = if current_offset == 0 {
            new_raw_offset
        } else {
            (current_offset as f64 + OFFSET_ALPHA * (new_raw_offset - current_offset) as f64) as i64
        };

        CLOCK_OFFSET.store(filtered_offset, Ordering::Relaxed);
        Some((filtered_offset, rtt))
    } else {
        None
    }
}

pub(crate) async fn send_identity_and_wait_ack(conn: &quinn::Connection) -> Result<(), Box<dyn Error>> {
    let (send, mut recv) = conn.open_bi().await?;
    send_identity(send).await;

    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;

    let len = u32::from_le_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;

    match postcard::from_bytes::<ControlPacket>(&data) {
        Ok(ControlPacket::StartStreaming) => {
            log::info!("[QUIC] Server ACK → start streaming");
            Ok(())
        }
        Ok(other) => {
            Err(format!("unexpected ACK packet: {:?}", other).into())
        }
        Err(e) => {
            Err(format!("ACK decode error: {e}").into())
        }
    }
}