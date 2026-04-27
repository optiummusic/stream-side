
use common::clock::Clock;

use crate::NetworkStats;

use super::*;

pub(crate) async fn handle_connection(
    conn: Connection,
    bcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
    shard_cache: Arc<ShardCache>,
    congestion_ctl: Arc<Mutex<CongestionController>>,
    senders: Senders,
    info_tx: tokio::sync::watch::Sender<Option<Arc<ConnectionInfo>>>,
) {
    let remote_addr = conn.remote_address();
    log::info!("[QUIC] Client connected: {}", remote_addr);

    // Подготовка контекста клиента
    let client_rx = bcast_tx.subscribe();
    let (identity_tx, _) = watch::channel(ClientIdentity::default());
    
    let info = Arc::new(ConnectionInfo {
        remote: remote_addr.to_string(),
        label: RwLock::new(remote_addr.to_string()),
        ready: AtomicBool::new(false),
        clock: Clock::new(),
        stats: NetworkStats::new(),
    });
    
    let _ = info_tx.send(Some(info.clone()));
    // Клоны для разных задач внутри одного соединения
    let info_bi = info.clone();
    let info_uni = info.clone();
    let info_main = info.clone();
    let idr_tx_uni = senders.idr_tx.clone();
    let send_main = senders.clone();
    let conn_bi = conn.clone();
    let conn_uni = conn.clone();

    // 1. Прием BI-стримов (Identify)
    tokio::spawn(async move {
        while let Ok((mut send, mut recv)) = conn_bi.accept_bi().await {
            let _ = handle_bi_stream(&mut send, &mut recv, identity_tx.clone(), info_bi.clone()).await;
        }
    });

    // 2. Прием UNI-стримов (Feedback/NACK)
    tokio::spawn(async move {
        while let Ok(recv) = conn_uni.accept_uni().await {
            let _ = handle_uni_stream(
                conn_uni.clone(), 
                recv, 
                info_uni.clone(), 
                idr_tx_uni.clone(), 
                shard_cache.clone()
            ).await;
        }
    });

    // 3. Основной цикл отправки (Datagrams)
    send_loop_to_client(conn, client_rx, info_main, congestion_ctl, send_main).await;
}

pub(crate) async fn run_accept_loop(
    endpoint: Endpoint,
    bcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
    shard_cache: Arc<ShardCache>,
    congestion_ctl: Arc<Mutex<CongestionController>>,
    senders: Senders,
    info_tx: tokio::sync::watch::Sender<Option<Arc<ConnectionInfo>>>,
) {
    while let Some(connecting) = endpoint.accept().await {
        let bcast_tx = bcast_tx.clone();
        let shard_cache = shard_cache.clone();
        let cong_clone = congestion_ctl.clone();
        let senders_clone = senders.clone();
        let watcher = info_tx.clone();

        tokio::spawn(async move {
            match connecting.await {
                Ok(conn) => {
                    handle_connection(conn, bcast_tx, shard_cache, cong_clone, senders_clone, watcher).await;
                }
                Err(e) => log::warn!("[QUIC] Handshake failed: {e}"),
            }
        });
    }
}