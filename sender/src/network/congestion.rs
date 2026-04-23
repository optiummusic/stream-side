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
    Recovering { until: Instant },
    Locked { until: Instant },
}

pub struct CongestionConfig {
    pub min_bitrate: u64,
    pub max_bitrate: u64,
    pub rtt_threshold_ms: f64,
    pub step_up: u64,
    pub backoff_congested: f64,
    pub backoff_lossy: f64,
    pub update_interval: Duration,
    pub recovery_duration: Duration,
    pub base_lock_duration: Duration,

    pub min_fps: u32,
    pub max_fps: u32,
    pub fps_step_down: u32, // Шаг снижения FPS за один тик (e.g. 5)
    pub fps_step_up: u32,   // Шаг восстановления FPS за один тик (e.g. 5)
}

pub struct CongestionAction {
    pub new_bitrate: Option<u64>,
    /// Some(fps) — выставить этот FPS; None — не менять
    pub target_fps: Option<u32>,
}

pub struct CongestionController {
    config: CongestionConfig,
    current_bitrate: u64,

    last_update: Instant,
    last_fps_update: Instant,
    lost_frames_acc: u32,

    control_state: ControlState,

    safe_bitrate: u64,
    consecutive_failures: u32,

    current_fps: u32,
}

impl CongestionController {
    pub fn new(initial_bitrate: u64, config: CongestionConfig) -> Self {
        let initial_fps = config.max_fps;
        Self {
            config,
            current_bitrate: initial_bitrate,
            last_update: Instant::now(),
            last_fps_update: Instant::now(),
            lost_frames_acc: 0,
            control_state: ControlState::Normal,
            safe_bitrate: initial_bitrate,
            consecutive_failures: 0,
            current_fps: initial_fps,
        }
    }

    pub fn report_lost_frame(&mut self) {
        self.lost_frames_acc += 1;
    }

    /// Шагаем FPS в сторону `desired`, не выходя за [min_fps, max_fps].
    /// Возвращает Some(новый_fps) если значение изменилось.
    fn step_fps(&mut self, now: Instant, desired: u32) -> Option<u32> {
        // Шагаем FPS с тем же интервалом, что и общий update_interval
        if now.duration_since(self.last_fps_update) < self.config.update_interval {
            return None;
        }

        if self.current_fps == desired {
            return None;
        }

        let next = if self.current_fps > desired {
            // Снижаем
            self.current_fps.saturating_sub(self.config.fps_step_down).max(desired)
        } else {
            // Повышаем
            (self.current_fps + self.config.fps_step_up).min(desired)
        };

        let next = next.clamp(self.config.min_fps, self.config.max_fps);

        if next != self.current_fps {
            self.current_fps = next;
            self.last_fps_update = now;
            Some(next)
        } else {
            None
        }
    }

    pub fn on_metrics(&mut self, rtt_ms: u64) -> CongestionAction {
        let now = Instant::now();

        // 1. Оцениваем состояние канала
        let channel_state = if self.lost_frames_acc >= 1 {
            ChannelState::Lossy
        } else if rtt_ms as f64 > self.config.rtt_threshold_ms {
            ChannelState::Congested
        } else {
            ChannelState::Healthy
        };

        self.lost_frames_acc = 0;

        // 2. Очистка устаревших состояний контроля
        match self.control_state {
            ControlState::Recovering { until } if now >= until => {
                let lock_time = self.config.base_lock_duration 
                    * (2_u32.pow(self.consecutive_failures.min(2)));
                self.control_state = ControlState::Locked { until: now + lock_time };
            }
            ControlState::Locked { until } if now >= until => {
                self.control_state = ControlState::Normal;
                self.consecutive_failures = self.consecutive_failures.saturating_sub(1);
            }
            _ => {}
        }

        let mut next_bitrate = self.current_bitrate;
        let mut force_update = false;

        // 3. Целевой FPS в зависимости от состояния
        let fps_target = if matches!(
            self.control_state,
            ControlState::Recovering { .. }
        ) || channel_state == ChannelState::Lossy
        {
            self.config.min_fps
        } else if matches!(self.control_state, ControlState::Normal) {
            self.config.max_fps
        } else {
            // Locked — держим текущий, не трогаем
            self.current_fps
        };

        // 4. Главная логика по битрейту
        match channel_state {
            ChannelState::Lossy => {
                if !matches!(self.control_state, ControlState::Recovering { .. }) {
                    self.consecutive_failures = (self.consecutive_failures + 1).min(5);

                    next_bitrate =
                        (self.current_bitrate as f64 * self.config.backoff_lossy) as u64;
                    self.safe_bitrate = next_bitrate;

                    self.control_state = ControlState::Recovering {
                        until: now + self.config.recovery_duration,
                    };
                    force_update = true;
                }
            }

            ChannelState::Congested => {
                if !matches!(self.control_state, ControlState::Recovering { .. }) {
                    next_bitrate =
                        (self.current_bitrate as f64 * self.config.backoff_congested) as u64;
                    self.control_state = ControlState::Normal;
                }
            }

            ChannelState::Healthy => {
                if matches!(self.control_state, ControlState::Normal)
                    && now.duration_since(self.last_update) >= self.config.update_interval
                {
                    next_bitrate = self.current_bitrate + self.config.step_up;
                }
            }
        }

        // 5. Плавный шаг FPS к цели
        let target_fps = self.step_fps(now, fps_target);

        // 6. Применяем битрейт
        next_bitrate = next_bitrate.clamp(self.config.min_bitrate, self.config.max_bitrate);

        let bitrate_changed_significantly =
            (next_bitrate as i64 - self.current_bitrate as i64).abs() >= 50_000;

        if (bitrate_changed_significantly
            && now.duration_since(self.last_update) >= self.config.update_interval)
            || force_update
        {
            self.current_bitrate = next_bitrate;
            self.last_update = now;

            CongestionAction {
                new_bitrate: Some(self.current_bitrate),
                target_fps,
            }
        } else {
            CongestionAction {
                new_bitrate: None,
                target_fps,
            }
        }
    }
}