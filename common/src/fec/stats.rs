use crate::{DatagramChunk, RecoveryReport, RecoveryStats, fec::GroupRecovery};

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

    pub fn note_partial_slicing(&mut self) {
        self.partial += 1;
    }

    pub fn note_group_recovery(&mut self, recovery: GroupRecovery) {
        match recovery {
            GroupRecovery::Direct => {
                self.direct_groups += 1;
            }
            GroupRecovery::Fec { missing_data_shards, via_parity } => {
                    self.fec_missing_data_shards += missing_data_shards as u64;
                    self.total_lost_chunks += missing_data_shards as u64;
                    self.fec_groups += 1;                
            }
        }
    }

    pub fn note_wasted(&mut self, failed_groups: u64, wasted_bytes: u64, lost_chunks: u64) {
        self.fec_failed_groups += failed_groups;
        self.wasted_payload_bytes += wasted_bytes;
        self.total_lost_chunks += lost_chunks;
    }

    pub fn maybe_log(&mut self, now_us: u64, every_us: u64) -> Option<&[u8]> {
        if self.window_start_us == 0 {
            self.window_start_us = now_us;
            return None;
        }

        let elapsed_us = now_us.saturating_sub(self.window_start_us);
        if elapsed_us < every_us {
            return None;
        }

        let secs = elapsed_us as f32 / 1_000_000.0;
        
        // --- Расчеты ---
        let total_lost = self.total_lost_chunks + self.frames_lost_evicted + 
                         self.frames_lost_out_of_window + self.frames_lost_timeout;
        
        let report = RecoveryReport {
            mbps: (self.rx_bytes as f32 * 8.0) / elapsed_us as f32,
            fps: self.frames_emitted as f32 / secs,
            loss_pct: if self.rx_chunks + total_lost > 0 {
                (total_lost as f32 / (self.rx_chunks + total_lost) as f32) * 100.0
            } else { 0.0 },
            nack_sent: self.nacks_sent,
            nack_recovery: self.nack_recovered_chunks,
            partial_slices: self.partial,
            direct: self.direct_groups,
            recovered: self.fec_groups,
            failed: self.fec_failed_groups,
            evicted: self.frames_lost_evicted,
            timeout: self.frames_lost_timeout,
            oow: self.frames_lost_out_of_window,
            hol: self.hol_skips,
        };

        // Сериализуем отчет во внутренний буфер
        let len = postcard::to_slice(&report, &mut self.serialization_buffer)
            .ok()?
            .len();

        // Опционально: оставляем лог для дебага на стороне отправителя
        log::info!("[STATS] Sent report: {:.2} Mbps, {:.1} FPS, Loss: {:.2}%", report.mbps, report.fps, report.loss_pct);

        // --- Полный сброс всех счетчиков ---
        let buf_copy = self.serialization_buffer;
        let next_start = now_us;
        
        *self = Self::default(); 
        
        self.window_start_us = next_start;
        self.serialization_buffer = buf_copy;

        Some(&self.serialization_buffer[..len])
    }
}