use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ChannelState {
    Healthy,
    Congested,
    Lossy,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ControlState {
    Normal,
    /// Восстанавливаемся после Lossy. Игнорируем RTT, режем FPS.
    Recovering { until: Instant },
    /// Запрет на повышение битрейта (уперлись в потолок канала).
    Locked { until: Instant },
}

pub struct CongestionConfig {
    pub min_bitrate: u64,
    pub max_bitrate: u64,
    pub rtt_threshold_ms: f64,
    pub step_up: u64,
    pub backoff_congested: f64, // e.g. 0.8 (режем на 20%)
    pub backoff_lossy: f64,     // e.g. 0.5 (режем на 50%)
    pub update_interval: Duration,
    pub recovery_duration: Duration, // e.g. 1 sec
    pub base_lock_duration: Duration, // e.g. 5 sec
}

/// Что должен сделать бэкенд после апдейта
pub struct CongestionAction {
    pub new_bitrate: Option<u64>,
    pub drop_fps: bool, // Если true -> захват должен снизить FPS
}

pub struct CongestionController {
    config: CongestionConfig,
    current_bitrate: u64,
    
    last_update: Instant,
    lost_frames_acc: u32,

    control_state: ControlState,
    
    // Для механизма блокировки
    safe_bitrate: u64,
    consecutive_failures: u32,
}

impl CongestionController {
    pub fn new(initial_bitrate: u64, config: CongestionConfig) -> Self {
        Self {
            config,
            current_bitrate: initial_bitrate,
            last_update: Instant::now(),
            lost_frames_acc: 0,
            control_state: ControlState::Normal,
            safe_bitrate: initial_bitrate,
            consecutive_failures: 0,
        }
    }

    pub fn report_lost_frame(&mut self) {
        self.lost_frames_acc += 1;
    }

    pub fn on_metrics(&mut self, rtt_ms: f64) -> CongestionAction {
        let now = Instant::now();

        // 1. Оцениваем состояние канала
        let channel_state = if self.lost_frames_acc >= 1 {
            ChannelState::Lossy
        } else if rtt_ms > self.config.rtt_threshold_ms {
            ChannelState::Congested
        } else {
            ChannelState::Healthy
        };

        // Сбрасываем аккум потерь после оценки
        self.lost_frames_acc = 0; 

        // 2. Очистка устаревших состояний контроля
        match self.control_state {
            ControlState::Recovering { until } if now >= until => {
                // Вышли из восстановления. Переходим в лок, чтобы сразу не повышать
                let lock_time = self.config.base_lock_duration * (2_u32.pow(self.consecutive_failures));
                self.control_state = ControlState::Locked { until: now + lock_time };
            }
            ControlState::Locked { until } if now >= until => {
                self.control_state = ControlState::Normal;
                // Сбрасываем счетчик неудач, если мы долго продержались без потерь
                self.consecutive_failures = self.consecutive_failures.saturating_sub(1);
            }
            _ => {}
        }

        let mut next_bitrate = self.current_bitrate;
        let mut drop_fps = false;
        let mut force_update = false;

        // 3. Главная логика реакций
        match channel_state {
            ChannelState::Lossy => {
                // Lossy имеет максимальный приоритет
                if !matches!(self.control_state, ControlState::Recovering { .. }) {
                    // Мы попытались работать на этом битрейте и словили потери.
                    self.consecutive_failures = (self.consecutive_failures + 1).min(5); // кап неудач
                    
                    next_bitrate = (self.current_bitrate as f64 * self.config.backoff_lossy) as u64;
                    self.safe_bitrate = next_bitrate; // Запоминаем, где было стабильно
                    
                    self.control_state = ControlState::Recovering { 
                        until: now + self.config.recovery_duration 
                    };
                    force_update = true;
                }
                drop_fps = true; // Пока потери - душим FPS
            }
            
            ChannelState::Congested => {
                // Если мы в Recovering, игнорируем высокий RTT (даем сети разгрузиться)
                if !matches!(self.control_state, ControlState::Recovering { .. }) {
                    next_bitrate = (self.current_bitrate as f64 * self.config.backoff_congested) as u64;
                    // Сбрасываем лок, если он был, так как мы уперлись в RTT
                    self.control_state = ControlState::Normal; 
                }
            }
            
            ChannelState::Healthy => {
                // Повышаем только если мы в Normal
                if matches!(self.control_state, ControlState::Normal) {
                    if now.duration_since(self.last_update) >= self.config.update_interval {
                        next_bitrate = self.current_bitrate + self.config.step_up;
                    }
                }
            }
        }

        // 4. Применяем изменения
        next_bitrate = next_bitrate.clamp(self.config.min_bitrate, self.config.max_bitrate);

        let bitrate_changed_significantly = (next_bitrate as i64 - self.current_bitrate as i64).abs() >= 50_000;

        if (bitrate_changed_significantly && now.duration_since(self.last_update) >= self.config.update_interval) || force_update {
            self.current_bitrate = next_bitrate;
            self.last_update = now;
            
            CongestionAction {
                new_bitrate: Some(self.current_bitrate),
                drop_fps,
            }
        } else {
            CongestionAction {
                new_bitrate: None,
                drop_fps: matches!(self.control_state, ControlState::Recovering { .. }),
            }
        }
    }
}