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

use crate::backend::vaapi_concealment::VaapiConcealment;
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
    Accepted{fps: u32},
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
    
    /// Вызывается при обнаружении потери вместо clear_buffer.
    /// Отправляет skip-заглушку декодеру чтобы не рвать prediction chain,
    /// затем чистит буфер слайсов.
    unsafe fn conceal_and_clear(&mut self, lost_frame_id: u64) {
        // ── Шаг 1: стандартный FFmpeg skip-frame путь ────────────────────────
        // (базовая реализация из mod.rs — поддерживает prediction chain в декодере)
        // Вызываем super-реализацию вручную, т.к. Rust не поддерживает super::
        {
            let poc_lsb = *self.get_poc_lsb();
            let max_poc_lsb = self.get_hevc_state().max_poc_lsb;
            let next_poc = poc_lsb.wrapping_add(1) & (max_poc_lsb - 1);

            let template = self.get_skip_frame_template().cloned();
            let frame = template.as_ref().and_then(|t| {
                let state = self.get_hevc_state();
                t.make_frame(next_poc, state)
            });

            if let Some(frame) = frame {
                if self.submit_to_decoder(&frame, lost_frame_id, None).is_ok() {
                    *self.get_poc_lsb() = next_poc;
                    let abs_poc = self.get_hevc_state().remember_picture(
                        next_poc, true, false, false,
                    );
                    self.get_hevc_state().mark_output_and_prune(abs_poc);
                    log::info!(
                        "[Concealment] FFmpeg skip frame sent: lost={lost_frame_id} poc={next_poc}"
                    );
                }
            } else {
                log::warn!("[Concealment] No skip frame template yet for lost_frame_id={lost_frame_id}");
            }
        }

        // ── Шаг 2: VA-API freeze (прямая заморозка surface на GPU) ──────────
        // Работает параллельно с FFmpeg путём и перезаписывает surface
        // последним хорошим кадром вместо серого артефакта.
        #[cfg(unix)]
        {
            // 1. Получаем poc_lsb (копируется, заимствование self сразу завершается)
            let poc_lsb = *self.get_poc_lsb(); 
            
            // 2. Получаем шаблон (клонируется, заимствование self завершается)
            let template = self.get_skip_frame_template().cloned();

            // 3. Трюк с указателем для HevcState:
            // Вызываем мутабельный геттер, берем указатель и ТУТ ЖЕ завершаем заимствование self.
            let hevc_ptr = self.get_hevc_state() as *const HevcState;

            let vps = self.get_vps().clone();
            let pps = self.get_pps().clone();
            let sps = self.get_sps().clone();

            let conceal = self.get_concealment();
            let result = unsafe {
                // Превращаем указатель обратно в ссылку &HevcState прямо при вызове.
                // Это безопасно, так как мы знаем, что hevc_state и concealment 
                // — это разные поля в структуре, реализующей трейт.
                crate::backend::vaapi_concealment::vaapi_concealment_freeze(
                    conceal,
                    &*hevc_ptr,
                    poc_lsb,
                    &template,
                    &sps,
                    &pps,
                    &vps,
                )
            };

            match result {
                Ok(()) => log::info!("[Concealment] VA-API freeze OK"),
                Err(e) => log::warn!("[Concealment] VA-API freeze failed: {e}"),
            }
        }

        self.clear_buffer();
    }

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


    /// Текущий PicOrderCntLsb — трекаем на каждом submit_to_decoder
    fn get_poc_lsb(&mut self) -> &mut u32;
    fn get_hevc_state(&mut self) -> &mut HevcState;
    fn get_skip_frame_template(&self) -> Option<&SkipFrameTemplate>;
    fn get_concealment(&mut self) -> &mut VaapiConcealment;
    fn get_sps(&self) -> &Option<SpsFields>;
    fn get_pps(&self) -> &Option<PpsFields>;
    fn get_vps(&self) -> &Option<Vec<u8>>;
    
    #[cfg(target_os = "android")]
    unsafe fn init_with_surface(
        &mut self, 
        _window: *mut ndk_sys::ANativeWindow, 
        _width: i32, 
        _height: i32
    ) -> Result<(), BackendError>;
    
}

#[cfg(unix)]
pub(crate) fn dump_debug_hevc(
    vps: &[u8],
    sps: &Option<SpsFields>,
    pps: &Option<PpsFields>,
    slice: &[u8],
) {
    let mut out = Vec::new();

    // VPS (можно захардкодить или пропустить, ffmpeg иногда прощает)
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(vps);

    if let Some(sps) = sps {
        out.extend_from_slice(&[0,0,0,1]);
        out.extend_from_slice(&sps.raw); // <-- ВАЖНО: сохрани raw SPS!
    }

    if let Some(pps) = pps {
        out.extend_from_slice(&[0,0,0,1]);
        out.extend_from_slice(&pps.raw); // <-- и PPS тоже
    }

    // slice
    out.extend_from_slice(&[0, 0, 0, 1]); // Убедись, что перед слайсом тоже есть старт-код
    out.extend_from_slice(slice);

    std::fs::write("/tmp/test.h265", out).unwrap();
}
// ─────────────────────────────────────────
// Экспорт конкретных реализаций
// ─────────────────────────────────────────

pub(crate) mod hevc_parser;
pub(crate) use hevc_parser::*;

#[cfg(unix)]
pub mod vaapi_concealment;

#[cfg(not(target_os = "android"))]
pub mod desktop;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "android")]
pub mod android;