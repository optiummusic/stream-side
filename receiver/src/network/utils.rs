use common::clock::CLIENT_CLOCK;

use super::*;

pub(crate) fn process_control(ctrl: ControlPacket) {
    match ctrl {
        ControlPacket::Pong { offset } => CLIENT_CLOCK.apply_remote_offset(offset),
        _ => ()
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