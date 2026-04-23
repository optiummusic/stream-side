use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;
use lazy_static::lazy_static;

use crate::FrameTrace;

lazy_static! {
    pub static ref START_INSTANT: Instant = Instant::now();
}

#[cfg(feature = "receiver")]
lazy_static! {
    /// Shared clock state for the client process.
    /// Replaces the bare `CLOCK_OFFSET: AtomicI64`.
    pub static ref CLIENT_CLOCK: Clock = Clock::new();
}

#[derive(Default)]
pub struct Clock {
    offset_us: AtomicI64,
    sync_count: AtomicU64,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            offset_us: AtomicI64::new(0),
            sync_count: AtomicU64::new(0),
        }
        
    }

    #[cfg(feature = "sender")]
    pub fn sync(&self, server_now: u64, client_now: u64, rtt: u64) {
        let client_point_in_time = (client_now + rtt / 2) as i64;
        let new_offset = server_now as i64 - client_point_in_time;
        let current = self.offset_us.load(Ordering::Relaxed);
        let count = self.sync_count.fetch_add(1, Ordering::Relaxed);

        if count == 0 {
            self.offset_us.store(new_offset, Ordering::Relaxed);
            log::info!("[CLOCK] Initialized offset: {}ms (RTT: {:.2}ms)", 
                new_offset / 1000, rtt as f64 / 1000.0);
        } else {
            // Насколько текущий замер отличается от усредненного
            let drift_delta = new_offset - current;
            
            // Если дрейф больше 10мс — это повод обратить внимание
            if drift_delta.abs() > 10_000 {
                log::warn!(
                    "[CLOCK] Significant jitter detected! Raw delta: {:.1}ms, RTT: {:.1}ms",
                    drift_delta as f64 / 1000.0,
                    rtt as f64 / 1000.0
                );
            }
            let alpha = if count < 50 { 0.1 } else { 0.001 };
            self.update_offset(new_offset, alpha);
        }
    }

    /// Локальное монотонное время в микросекундах
    #[inline]
    pub fn local_now_us(&self) -> u64 {
        START_INSTANT.elapsed().as_micros() as u64
    }

    /// Удаленное время (скорректированное оффсетом)
    #[inline]
    pub fn remote_now_us(&self) -> u64 {
        let local = self.local_now_us();
        let off = self.offset_us.load(Ordering::Relaxed);
        (local as i64 + off) as u64
    }
    
    #[inline]
    #[cfg(feature = "sender")]
    pub fn client_to_server(&self, us: u64) -> i64 {
        let off = self.offset_us.load(Ordering::Relaxed);
        us as i64 + off
    }

    #[cfg(feature = "receiver")]
    pub fn apply_remote_offset(&self, new_offset: i64) {
        let old_offset = self.offset_us.load(Ordering::Relaxed);
        let adjustment = new_offset - old_offset;

        // Если это первая установка или большой прыжок — логируем
        if old_offset == 0 || adjustment.abs() > 1000 {
            log::info!("[CLOCK] Server updated offset: {}us ({:+}us adjustment)", 
                new_offset, adjustment);
        }

        self.offset_us.store(new_offset, Ordering::Relaxed);
        self.sync_count.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    #[cfg(feature = "receiver")]
    pub fn server_to_client(&self, remote_us: u64) -> i64 {
        let off = self.offset_us.load(Ordering::Relaxed);
        remote_us as i64 - off
    }

    /// Установить оффсет (например, после первого Ping/Pong)
    #[inline]
    pub fn set_offset(&self, offset: i64) {
        self.offset_us.store(offset, Ordering::Relaxed);
    }

    #[inline]
    pub fn get_offset(&self) -> i64 {
        self.offset_us.load(Ordering::Relaxed)
    }

    /// Плавное обновление оффсета (фильтрация)
    pub fn update_offset(&self, new_raw_offset: i64, alpha: f64) {
        let current = self.offset_us.load(Ordering::Relaxed);
        let filtered = (current as f64 + alpha * (new_raw_offset - current) as f64) as i64;
        self.offset_us.store(filtered, Ordering::Relaxed);
    }
}

#[repr(usize)]
pub enum FrameStep {
    Capture = 0,
    Encode = 1,
    Serialize = 2,
    Receive = 3,
    Reassembled = 4,
    JitterOut = 5,
    DecoderSubmit = 6,
    Decode = 7,
    Present = 8,
}

impl FrameTrace {
    pub fn get_local(&self, step: FrameStep, clock: &Clock) -> i64 {
        let step: usize = step as usize;
        let val = self.raw_by_idx(step);
        
        // Разделяем логику внутри одного метода через cfg
        #[cfg(feature = "sender")]
        {
            return match step as usize {
                0..=2 => val as i64,
                3..=8 => clock.client_to_server(val),
                _ => 0,
            }
        }

        #[cfg(feature = "receiver")]
        #[allow(unreachable_code)]
        {
            return match step as usize {
                0..=2 => clock.server_to_client(val),
                3..=8 => val as i64,
                _ => 0,
            }
        }
    }

    fn raw_by_idx(&self, idx: usize) -> u64 {
        match idx {
            0 => self.capture_us,
            1 => self.encode_us,
            2 => self.serialize_us,
            3 => self.receive_us,
            4 => self.reassembled_us,
            5 => self.jitter_out_us,
            6 => self.decoder_submit_us,
            7 => self.decode_us,
            8 => self.present_us,
            _ => 0,
        }
    }
}