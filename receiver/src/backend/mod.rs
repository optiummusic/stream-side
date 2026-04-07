// src/backend/mod.rs
//
// Абстракция видео-пайплайна (декод + рендер).
//
// КЛЮЧЕВАЯ ПРОБЛЕМА, которую решает этот файл:
//
//   Десктоп (Linux/Windows):
//     ffmpeg декодирует HEVC → YUV420P кадры в Rust-памяти
//     → WGPU загружает текстуры → рисует через шейдер
//
//   Android:
//     MediaCodec декодирует HEVC НАПРЯМУЮ в ANativeWindow (Surface)
//     → кадры НИКОГДА не попадают в Rust-память (zero-copy!)
//     → Rust вообще не видит пиксели
//
// Трейт VideoBackend скрывает эту фундаментальную разницу.

use std::fmt;
use common::FrameTrace;
// ─────────────────────────────────────────
// Типы ошибок
// ─────────────────────────────────────────

#[derive(Debug)]
pub enum BackendError {
    /// Вызов до инициализации (Surface ещё не передан на Android)
    NotInitialized,
    /// Ошибка декодера (ffmpeg / MediaCodec)
    DecodeError(String),
    /// Ошибка конфигурации (codec not found, wrong format, etc.)
    ConfigError(String),
    /// Входной буфер декодера переполнен; кадр нужно пропустить
    BufferFull,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized     => write!(f, "Backend not initialized"),
            Self::DecodeError(msg)   => write!(f, "Decode error: {msg}"),
            Self::ConfigError(msg)   => write!(f, "Config error: {msg}"),
            Self::BufferFull         => write!(f, "Decoder input buffer full — frame dropped"),
        }
    }
}

impl std::error::Error for BackendError {}

// ─────────────────────────────────────────
// YUV-кадр (только для десктопного пути)
// ─────────────────────────────────────────

/// Декодированный кадр в формате YUV420P.
/// Используется ТОЛЬКО десктопным бекендом —
/// на Android кадры уходят прямо в Surface и в Rust не попадают.
pub struct YuvFrame {
    pub frame_id: u64,
    pub trace:    FrameTrace,

    pub width:    u32,
    pub height:   u32,
    // Три раздельных плоскости — именно этого ждёт наш WGSL-шейдер
    pub y:        Vec<u8>,
    pub u:        Vec<u8>,
    pub v:        Vec<u8>,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
}

// ─────────────────────────────────────────
// Результат одного цикла poll_output
// ─────────────────────────────────────────

/// Описывает, что произошло после запроса декодированного вывода.
///
/// Это ключевой enum, отражающий архитектурную разницу платформ:
/// - Десктоп возвращает `Yuv` — данные в Rust-памяти, готовы к GPU-загрузке.
/// - Android возвращает `DirectToSurface` — кадр уже на экране, ничего делать не нужно.
pub enum FrameOutput {
    /// YUV420P кадр для загрузки в WGPU-текстуры (десктоп)
    Yuv(YuvFrame),

    /// Кадр отрендерен напрямую в ANativeWindow/Surface (Android zero-copy)
    DirectToSurface,

    /// Декодер ещё не выдал следующий кадр — ничего делать не нужно
    Pending,
}

// ─────────────────────────────────────────
// Главный трейт
// ─────────────────────────────────────────

/// Платформо-специфичный пайплайн декодирования и рендеринга.
///
/// # Контракт использования
///
/// ```text
/// loop {
///     let packet = network.receive().await;
///     backend.push_encoded(&packet.payload, packet.frame_id)?;
///
///     // Обязательно дренируем очередь вывода декодера!
///     // На Android это отпускает буферы обратно в MediaCodec
///     // и позволяет ему рендерить в Surface.
///     loop {
///         match backend.poll_output()? {
///             FrameOutput::Yuv(frame)      => send_to_renderer(frame),
///             FrameOutput::DirectToSurface => { /* Android уже отрисовал */ }
///             FrameOutput::Pending         => break,
///         }
///     }
/// }
/// ```
///
/// # Thread safety
/// `Send + 'static` — бекенд перемещается в networking-поток.
pub trait VideoBackend: Send + 'static {
    /// Передать один закодированный HEVC-фрейм (сырые NAL-юниты из VideoPacket.payload).
    ///
    /// На десктопе: передаёт пакет в ffmpeg-декодер.
    /// На Android:  копирует данные во входной буфер AMediaCodec.
    fn push_encoded(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>,) -> Result<(), BackendError>;

    /// Получить один декодированный кадр из выходной очереди (если готов).
    ///
    /// Должен вызываться в цикле после каждого `push_encoded`, пока не вернёт `Pending`.
    ///
    /// На Android ОБЯЗАТЕЛЕН даже если результат не нужен —
    /// иначе MediaCodec не освободит буфер и встанет.
    fn poll_output(&mut self) -> Result<FrameOutput, BackendError>;

    /// Освободить все ресурсы (codec, surface, GPU handles).
    /// Безопасно вызывать несколько раз.
    fn shutdown(&mut self);
}

// ─────────────────────────────────────────
// Экспорт конкретных реализаций
// ─────────────────────────────────────────

#[cfg(not(target_os = "android"))]
pub mod desktop;

#[cfg(target_os = "android")]
pub mod android;