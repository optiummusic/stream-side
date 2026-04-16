use super::*;

pub(crate) async fn handle_connection(
    conn: Connection,
    bcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
    idr_tx: watch::Sender<bool>,
    shard_cache: Arc<ShardCache>,
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
    });
    
    let clock_offset = Arc::new(AtomicI64::new(0));

    // Клоны для разных задач внутри одного соединения
    let info_bi = info.clone();
    let info_uni = info.clone();
    let info_main = info.clone();
    let idr_tx_uni = idr_tx.clone();
    let idr_tx_main = idr_tx.clone();
    let clock_off_main = clock_offset.clone();
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
                clock_offset.clone(), 
                idr_tx_uni.clone(), 
                shard_cache.clone()
            ).await;
        }
    });

    // 3. Основной цикл отправки (Datagrams)
    send_loop_to_client(conn, client_rx, info_main, &clock_off_main, idr_tx_main).await;
}

pub(crate) async fn run_accept_loop(
    endpoint: Endpoint,
    bcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
    idr_tx: watch::Sender<bool>,
    shard_cache: Arc<ShardCache>,
) {
    while let Some(connecting) = endpoint.accept().await {
        let bcast_tx = bcast_tx.clone();
        let idr_tx = idr_tx.clone();
        let shard_cache = shard_cache.clone();

        tokio::spawn(async move {
            match connecting.await {
                Ok(conn) => {
                    handle_connection(conn, bcast_tx, idr_tx, shard_cache).await;
                }
                Err(e) => log::warn!("[QUIC] Handshake failed: {e}"),
            }
        });
    }
}