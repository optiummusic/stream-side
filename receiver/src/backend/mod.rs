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
use bytes::BytesMut;
use common::FrameTrace;

#[cfg(unix)]
pub use crate::types::DmaBufFrame;

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
    pub y:        Vec<u8>,
    pub uv:       Vec<u8>,
    pub y_stride: u32,
    pub uv_stride: u32,
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

    #[cfg(unix)]
    DmaBuf(DmaBufFrame),

    /// Кадр отрендерен напрямую в ANativeWindow/Surface (Android zero-copy)
    DirectToSurface,

    /// Декодер ещё не выдал следующий кадр — ничего делать не нужно
    Pending,

    /// Декодер выплюнул кадр
    Dropped{age: Option<f32>},
}

pub enum PushStatus {
    Accepted,
    Accumulating,
    Dropped{age: Option<f32>},
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
    /// 
    /// Default implementation for bufferizing slices
    fn push_encoded(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>, is_last: bool) -> Result<PushStatus, BackendError> {
        if trace.is_some() {
            *self.get_current_trace() = trace;
        }
        
        self.get_slice_buffer().extend_from_slice(payload);
        
        if !is_last {
            return Ok(PushStatus::Accumulating);
        }
        
        let full_payload = self.get_slice_buffer().split().freeze();
        let full_trace = self.get_current_trace().take();
        
        self.submit_to_decoder(&full_payload, frame_id, full_trace)
    }
    /// Initialize slice buffer in Backend struct as
    /// slice_buffer:   BytesMut,
    fn get_slice_buffer(&mut self) -> &mut BytesMut;

    /// This is for capturing trace datapack that we embed in very first slice during serialization on server's side
    /// Inititalize in the Backend struct as 
    /// current_frame_trace: Option<FrameTrace>,
    fn get_current_trace(&mut self) -> &mut Option<FrameTrace>;

    /// This is a function that takes the fully assembled slice, given by the default push_encoded function
    fn submit_to_decoder(&mut self, payload: &[u8], frame_id: u64, trace: Option<FrameTrace>) -> Result<PushStatus, BackendError>;

    /// This is the method we call from network stack in case we detect packet loss/index inconsistency
    /// So we don't keep irrelevant slices in buffer which would break sequencing
    fn clear_buffer(&mut self) {
        self.get_slice_buffer().clear();
        self.get_current_trace().take();
    }
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

    #[cfg(target_os = "android")]
    unsafe fn init_with_surface(
        &mut self, 
        _window: *mut ndk_sys::ANativeWindow, 
        _width: i32, 
        _height: i32
    ) -> Result<(), BackendError>;
    
}

// ─────────────────────────────────────────
// Экспорт конкретных реализаций
// ─────────────────────────────────────────

#[cfg(not(target_os = "android"))]
pub mod desktop;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "android")]
pub mod android;