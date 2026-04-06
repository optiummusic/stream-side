use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct VideoPacket {
    pub frame_id: u64,
    pub payload: Vec<u8>,
    pub timestamp: u64,
    pub is_key: bool,
}

/// Один датаграмм-чанк — фрагмент сериализованного VideoPacket.
///
/// Датаграммы QUIC ограничены MTU (~1200 байт в интернете, ~1450 в LAN).
/// Кадр HEVC может быть 20–200 KB, поэтому его нужно дробить на чанки.
///
/// Схема сборки на ресивере:
///   - Собирать чанки по frame_id в HashMap
///   - Когда received_chunks == total_chunks → склеить data по chunk_idx → deserialize VideoPacket
///   - Если пришёл новый кадр с бо́льшим frame_id → старые незавершённые выбросить
#[derive(Serialize, Deserialize, Debug)]
pub struct DatagramChunk {
    pub frame_id:     u64,
    pub chunk_idx:    u16,
    pub total_chunks: u16,
    pub data:         Vec<u8>, // срез сериализованного VideoPacket
}