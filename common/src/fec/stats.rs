use crate::{DatagramChunk, fec::builder::GroupRecovery};

#[derive(Debug, Default, Clone)]
pub struct RecoveryStats {
    pub window_start_us: u64,

    // ── Базовые метрики приема (Network RX) ──
    pub rx_chunks: u64,
    pub rx_bytes: u64,
    pub rx_data_chunks: u64,
    pub rx_parity_chunks: u64,
    pub zombie_chunks: u64,

    // ── Метрики выдачи (Application TX) ──
    pub frames_emitted: u64,
    pub video_packets_emitted: u64,

    // ── Потери и задержки (Loss & Stalls) ──
    pub frames_lost_evicted: u64,
    pub frames_lost_timeout: u64,
    pub frames_lost_out_of_window: u64,
    pub hol_skips: u64,

    // ── Обратная связь (Feedback) ──
    pub nacks_sent: u64,

    // ── Восстановление (FEC) ──
    pub fec_groups: u64,
    pub direct_groups: u64,
    pub fec_missing_data_shards: u64,
    pub fec_failed_groups: u64,
    pub wasted_payload_bytes: u64,
    pub nack_recovered_chunks: u64, // Сколько чанков пришло после отправки NACK на их группу
    pub total_lost_chunks: u64,     // Общее кол-во шардов, которые не дошли (включая восстановленные через FEC)
}

impl RecoveryStats {
    pub fn note_chunk(&mut self, chunk: &DatagramChunk, was_nacked: bool, zombie: bool) {
        // 1. Считаем байты всегда (это нагрузка на интерфейс)
        self.rx_bytes += chunk.payload_len as u64;

        // 2. Если это зомби — инкрементим только их и выходим.
        // Они не должны участвовать в расчете Loss, так как это либо дубликаты,
        // либо пакеты для уже закрытых (удаленных) кадров.
        if zombie {
            self.zombie_chunks += 1;
            return; 
        }

        // 3. Основная статистика для "живых" кадров
        self.rx_chunks += 1;

        if was_nacked {
            // Это пакет, пришедший именно по запросу NACK
            self.nack_recovered_chunks += 1;
        }

        if chunk.shard_idx >= chunk.k {
            self.rx_parity_chunks += 1;
        } else {
            self.rx_data_chunks += 1;
        }
    }

    pub fn note_frame_emitted(&mut self, packets_count: usize) {
        self.frames_emitted += 1;
        self.video_packets_emitted += packets_count as u64;
    }

    pub fn note_nack(&mut self) {
        self.nacks_sent += 1;
    }

    pub fn note_frame_lost_evicted(&mut self) {
        self.frames_lost_evicted += 1;
        self.hol_skips += 1;
    }

    pub fn note_frame_lost_timeout(&mut self) {
        self.frames_lost_timeout += 1;
        self.hol_skips += 1;
    }

    pub fn note_frame_lost_out_of_window(&mut self) {
        self.frames_lost_out_of_window += 1;
        self.hol_skips += 1;
    }

    pub fn note_group_recovery(&mut self, recovery: GroupRecovery) {
        match recovery {
            GroupRecovery::Direct => {
                self.direct_groups += 1;
            }
            GroupRecovery::Fec { missing_data_shards } => {
                self.fec_groups += 1;
                self.fec_missing_data_shards += missing_data_shards as u64;
                self.total_lost_chunks += missing_data_shards as u64;
            }
        }
    }

    pub fn note_wasted(&mut self, failed_groups: u64, wasted_bytes: u64, lost_chunks: u64) {
        self.fec_failed_groups += failed_groups;
        self.wasted_payload_bytes += wasted_bytes;
        self.total_lost_chunks += lost_chunks;
    }

    pub fn maybe_log(&mut self, now_us: u64, every_us: u64) {
        if self.window_start_us == 0 {
            self.window_start_us = now_us;
            return;
        }

        let elapsed_us = now_us.saturating_sub(self.window_start_us);
        if elapsed_us < every_us {
            return;
        }

        let secs = elapsed_us as f64 / 1_000_000.0;
        
        // Вычисляем производные метрики
        let mbps = (self.rx_bytes as f64 * 8.0) / elapsed_us as f64; // bits per microsecond == Mbps
        let fps = self.frames_emitted as f64 / secs;
        
        // Считаем % избыточных чанков (overhead)
        let parity_pct = if self.rx_chunks > 0 {
            (self.rx_parity_chunks as f64 / self.rx_chunks as f64) * 100.0
        } else {
            0.0
        };

        let total_lost = self.total_lost_chunks + self.frames_lost_evicted + self.frames_lost_out_of_window + self.frames_lost_timeout;
        let loss_pct = if self.rx_chunks + total_lost > 0 {
            (total_lost as f64 / (self.rx_chunks + total_lost) as f64) * 100.0
        } else {
            0.0
        };

        let total_received = self.rx_chunks + self.zombie_chunks;

        let zombie_pct = if total_received > 0 {
            (self.zombie_chunks as f64 / total_received as f64) * 100.0
        } else {
            0.0
        };

        log::info!(
            "[STATS {:.1}s] \n\
             ├─ NET:  {:.2} Mbps | Loss: {} chk ({:.2}%) | NACK Recov: {} | NACKS Sent: {}\n\
             ├─ IN:   Chunks: {} | Zombie Rate: {:.1}% (Data: {}, Parity: {} [{:.1}% overhead])\n\
             ├─ OUT:  {:.1} FPS  | Frames: {} | Packets: {}\n\
             ├─ FEC:  Direct: {} | Recovered: {} | Failed: {} (wasted: {} bytes)\n\
             └─ LOSS: Evicted: {} | Timeout: {} | OOW: {} | HOL skips: {}",
            secs,
            mbps, self.total_lost_chunks, loss_pct, self.nack_recovered_chunks, self.nacks_sent,
            self.rx_chunks, zombie_pct, self.rx_data_chunks, self.rx_parity_chunks, parity_pct,
            fps, self.frames_emitted, self.video_packets_emitted,
            self.direct_groups, self.fec_groups, self.fec_failed_groups, self.wasted_payload_bytes,
            self.frames_lost_evicted, self.frames_lost_timeout, self.frames_lost_out_of_window, self.hol_skips
        );

        // Сброс окна
        self.window_start_us = now_us;
        self.rx_chunks = 0;
        self.rx_bytes = 0;
        self.rx_data_chunks = 0;
        self.rx_parity_chunks = 0;
        self.frames_emitted = 0;
        self.video_packets_emitted = 0;
        self.frames_lost_evicted = 0;
        self.frames_lost_timeout = 0;
        self.frames_lost_out_of_window = 0;
        self.hol_skips = 0;
        self.nacks_sent = 0;
        self.direct_groups = 0;
        self.fec_groups = 0;
        self.fec_failed_groups = 0;
        self.fec_missing_data_shards = 0;
        self.wasted_payload_bytes = 0;
        self.nack_recovered_chunks = 0;
        self.total_lost_chunks = 0;
        self.zombie_chunks = 0;
    }
}