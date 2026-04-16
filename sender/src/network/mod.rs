mod endpoint;
mod handlers;
mod clients;
mod connections;

pub(crate) use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
pub(crate) use std::{net::SocketAddr, sync::Arc, time::Duration};
pub(crate) use bytes::Bytes;
pub(crate) use quinn::{Endpoint, ServerConfig, Connection};
pub(crate) use quinn::crypto::rustls::QuicServerConfig;
pub(crate) use socket2::{Domain, Protocol, Socket, Type};
pub(crate) use tokio::sync::{RwLock, broadcast, mpsc, watch};
pub(crate) use common::{ControlPacket, DatagramChunk, FrameTrace, TYPE_CONTROL, VideoSlice};
pub(crate) use crate::{ClientIdentity, ConnectionInfo, FramePacer, SerializedFrame, ShardCache};
pub(crate) use crate::encode::EncodedFrame;

use endpoint::*;
use handlers::*;
use clients::*;
use connections::*;

pub struct QuicServer {
    /// Cloneable handle for pushing encoded frames into the transport pipeline.
    frame_tx: mpsc::Sender<EncodedFrame>,
    idr_tx: tokio::sync::watch::Sender<bool>,
}

impl QuicServer {
    /// Start the QUIC server bound to `listen_addr`.
    ///
    /// Returns immediately; all async tasks run in the background on the
    /// current Tokio runtime.
    ///
    /// Use [`frame_sink`] to obtain a channel for delivering encoded frames.
    pub async fn new(listen_addr: SocketAddr, idr_tx: tokio::sync::watch::Sender<bool>) -> Self {
        let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(32);
        let (bcast_tx, _) = broadcast::channel::<Arc<SerializedFrame>>(64);
        let shard_cache = Arc::new(ShardCache::new());

        // 1. Задача сериализатора
        let sc_serialiser = shard_cache.clone();
        let bcast_serialiser = bcast_tx.clone();
        tokio::spawn(run_serialiser_task(frame_rx, bcast_serialiser, sc_serialiser));

        // 2. Задача приема соединений
        let endpoint = build_server_endpoint(listen_addr);
        let bcast_accept = bcast_tx.clone();
        let idr_accept = idr_tx.clone();
        let sc_accept = shard_cache.clone();
        
        tokio::spawn(run_accept_loop(endpoint, bcast_accept, idr_accept, sc_accept));

        Self { frame_tx, idr_tx }
    }

    /// Return a cloneable sender for delivering encoded `(nal_bytes, is_key)` frames.
    ///
    /// Pass this to [`VideoSender::run`] to connect the capture/encode pipeline
    /// to the transport layer.
    pub fn frame_sink(&self) -> mpsc::Sender<EncodedFrame> {
        self.frame_tx.clone()
    }

    /// Convenience method — push one encoded frame directly.
    ///
    /// Drops silently if the internal channel is full (back-pressure for live
    /// video; we never want to stall the encoder waiting for the network).
    pub fn send(&self, frame: EncodedFrame) {
        match self.frame_tx.try_send(frame) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::debug!("[QuicServer] frame channel full, dropping");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::warn!("[QuicServer] frame channel closed");
            }
        }
    }
}