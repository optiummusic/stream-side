// src/backend/vaapi_concealment.rs
//
// VA-API Error Concealment для HEVC — «заморозка» кадра при потере NALU.
//
// ## Архитектура
//
// При потере слайса FFmpeg увидит синтаксическую ошибку и выдаст серый/
// артефактный кадр, потому что prediction chain сломан. Мы решаем это так:
//
//   1. Захватываем VASurfaceID последнего успешно декодированного кадра.
//   2. Генерируем минимальный TRAIL_R slice header с корректным poc_lsb и RPS,
//      который указывает на этот surface как референс.
//   3. Вызываем vaBeginPicture / vaRenderPicture / vaEndPicture напрямую —
//      VA-API «декодирует» skip-frame, записывая в тот же surface.
//   4. Отдаём surface обратно FFmpeg через фиктивный пакет, поддерживая chain.
//
// ## Почему не через FFmpeg
//
// ffmpeg_next не предоставляет публичного API для:
//   - доступа к VADisplay из hw_device_ctx
//   - получения VASurfaceID из AVFrame(VAAPI)
//   - ручной отправки VA буферов
//
// Поэтому используем ffmpeg-sys-next для raw pointer access и libva напрямую.
//
// ## Зависимости (Cargo.toml)
//
// ```toml
// [dependencies]
// ffmpeg-sys-next = "7"
// ffmpeg-next    = "7"
// libva-sys      = "0.1"   # или ручные extern "C" ниже (см. VaRaw)
// ```
//
// Если libva-sys недоступен — используй блок `mod va_raw` в конце файла.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::{os::raw::c_int, ptr};
use ffmpeg_next::{
    codec,
    format::Pixel,
    software::scaling,
    util::frame::video::Video,
    ffi::*,
};

use crate::backend::{BitWriter, HevcState, PpsFields, ShortTermRps, SkipFrameTemplate, SpsFields, dump_debug_hevc};

pub const AV_PIX_FMT_VAAPI: i32 = 44; 

#[repr(C)]
pub struct AVVAAPIDeviceContext {
    pub display: va::VADisplay,
    pub driver_quirks: c_int,
}

#[repr(C)]
pub struct AVVAAPIFramesContext {
    pub surface_ids: *mut va::VASurfaceID,
    pub nb_surfaces: c_int,
}
// ─────────────────────────────────────────────────────────────────────────────
// Низкоуровневые VA-API типы (если libva-sys не используется)
// ─────────────────────────────────────────────────────────────────────────────

// Если у тебя есть libva-sys — замени этот блок на:
//   use libva_sys::*;
mod va {
    #![allow(non_camel_case_types, non_snake_case, clippy::all)]

    pub type VADisplay    = *mut std::ffi::c_void;
    pub type VASurfaceID  = u32;
    pub type VABufferID   = u32;
    pub type VAContextID  = u32;
    pub type VAConfigID   = u32;
    pub type VAStatus     = i32;
    pub type VAImageID    = u32;

    pub const VA_INVALID_SURFACE: VASurfaceID = 0xFFFF_FFFF;
    pub const VA_INVALID_ID:      u32         = 0xFFFF_FFFF;
    pub const VA_STATUS_SUCCESS:  VAStatus    = 0;

    pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;

    // FourCC коды
    pub const VA_FOURCC_NV12: u32 = 0x3231564E; // 'NV12'
    pub const VA_FOURCC_YV12: u32 = 0x32315659; // 'YV12'
    pub const VA_FOURCC_I420: u32 = 0x30323449; // 'I420'

    // VAImageFormat byte order
    pub const VA_LSB_FIRST: u32 = 0;
    pub const VA_MSB_FIRST: u32 = 1;

    // VAProfile / VAEntrypoint
    pub const VAProfileHEVCMain:              i32 = 19;
    pub const VAProfileHEVCMain10:    i32 = 21;
    pub const VAEntrypointVLD:                i32 = 1;

    // VABufferType
    pub const VAPictureParameterBufferType:   u32 = 0;
    pub const VASliceParameterBufferType:     u32 = 1;
    pub const VASliceDataBufferType:          u32 = 2;

    // VA_PICTURE_HEVC flags
    pub const VA_PICTURE_HEVC_INVALID:             u32 = 0x0000_0001;
    pub const VA_PICTURE_HEVC_LONG_TERM_REFERENCE: u32 = 0x0000_0002;
    pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE:  u32 = 0x0000_0004;
    pub const VA_PICTURE_HEVC_RPS_ST_CURR_AFTER:   u32 = 0x0000_0008;

    /// VAImageFormat из <va/va.h>
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct VAImageFormat {
        pub fourcc:           u32,
        pub byte_order:       u32,
        pub bits_per_pixel:   u32,
        pub depth:            u32,
        pub red_mask:         u32,
        pub green_mask:       u32,
        pub blue_mask:        u32,
        pub alpha_mask:       u32,
        pub va_reserved:      [u32; 4],
    }

    /// VAImage из <va/va.h>
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct VAImage {
        pub image_id:         VAImageID,
        pub format:           VAImageFormat,
        pub buf:              VABufferID,
        pub width:            u32,
        pub height:           u32,
        pub data_size:        u32,
        pub num_planes:       u32,
        pub pitches:          [u32; 3],
        pub offsets:          [u32; 3],
        pub va_reserved:      [u32; 8],
    }

    impl Default for VAImage {
        fn default() -> Self {
            Self {
                image_id:      VA_INVALID_ID,
                format:        VAImageFormat::default(),
                buf:           VA_INVALID_ID,
                width:         0,
                height:        0,
                data_size:     0,
                num_planes:    0,
                pitches:       [0; 3],
                offsets:       [0; 3],
                va_reserved:   [0; 8],
            }
        }
    }

    /// Соответствует VAPictureHEVC из <va/va.h>
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct VAPictureHEVC {
        pub picture_id:       VASurfaceID,
        pub pic_order_cnt:    i32,
        pub flags:            u32,
        pub va_reserved:   [u32; 4],
    }

    impl Default for VAPictureHEVC {
        fn default() -> Self {
            Self {
                picture_id:    VA_INVALID_SURFACE,
                pic_order_cnt: 0,
                flags:         VA_PICTURE_HEVC_INVALID,
                va_reserved:   [0u32; 4]
            }
        }
    }

    /// VAPictureParameterBufferHEVC (va/va_dec_hevc.h)
    #[repr(C)]
    pub struct VAPictureParameterBufferHEVC {
        pub CurrPic:               VAPictureHEVC,
        pub ReferenceFrames:       [VAPictureHEVC; 15],
        pub pic_width_in_luma_samples:  u16,
        pub pic_height_in_luma_samples: u16,
        pub pic_fields:            u32,
        pub sps_max_dec_pic_buffering_minus1: u8,
        pub bit_depth_luma_minus8:   u8,
        pub bit_depth_chroma_minus8: u8,
        pub pcm_sample_bit_depth_luma_minus1:   u8,
        pub pcm_sample_bit_depth_chroma_minus1: u8,
        pub log2_min_luma_coding_block_size_minus3:     u8,
        pub log2_diff_max_min_luma_coding_block_size:   u8,
        pub log2_min_transform_block_size_minus2:       u8,
        pub log2_diff_max_min_transform_block_size:     u8,
        pub log2_min_pcm_luma_coding_block_size_minus3: u8,
        pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
        pub max_transform_hierarchy_depth_intra: u8,
        pub max_transform_hierarchy_depth_inter: u8,
        pub init_qp_minus26:         i8,
        pub diff_cu_qp_delta_depth:  u8,
        pub pps_cb_qp_offset:        i8,
        pub pps_cr_qp_offset:        i8,
        pub log2_parallel_merge_level_minus2: u8,
        pub num_tile_columns_minus1: u8,
        pub num_tile_rows_minus1:    u8,
        pub column_width_minus1:     [u16; 19],
        pub row_height_minus1:       [u16; 21],
        pub slice_parsing_fields:    u32,
        pub log2_max_pic_order_cnt_lsb_minus4: u8,
        pub num_short_term_ref_pic_sets: u8,
        pub num_long_term_ref_pic_sps:   u8,
        pub num_ref_idx_l0_default_active_minus1: u8,
        pub num_ref_idx_l1_default_active_minus1: u8,
        pub pps_beta_offset_div2:    i8,
        pub pps_tc_offset_div2:      i8,
        pub num_extra_slice_header_bits: u8,
        pub st_rps_bits:             u32,
        pub va_reserved: [u32; 8],
    }

    /// VASliceParameterBufferHEVC (va/va_dec_hevc.h)
    #[repr(C)]
    pub struct VASliceParameterBufferHEVC {
        pub slice_data_size:              u32,
        pub slice_data_offset:            u32,
        pub slice_data_flag:              u32,
        pub slice_data_byte_offset:       u32,
        pub slice_segment_address:        u32,
        pub RefPicList:                   [[u8; 15]; 2],
        pub LongSliceFlags:               u32,
        pub collocated_ref_idx:           u8,
        pub num_ref_idx_l0_active_minus1: u8,
        pub num_ref_idx_l1_active_minus1: u8,
        pub slice_qp_delta:               i8,
        pub slice_cb_qp_offset:           i8,
        pub slice_cr_qp_offset:           i8,
        pub slice_beta_offset_div2:       i8,
        pub slice_tc_offset_div2:         i8,
        pub luma_log2_weight_denom:       u8,
        pub delta_chroma_log2_weight_denom: i8,
        pub delta_luma_weight_l0:         [i8; 15],
        pub luma_offset_l0:               [i8; 15],
        pub delta_chroma_weight_l0:       [[i8; 2]; 15],
        pub ChromaOffsetL0:               [[i8; 2]; 15],
        pub delta_luma_weight_l1:         [i8; 15],
        pub luma_offset_l1:               [i8; 15],
        pub delta_chroma_weight_l1:       [[i8; 2]; 15],
        pub ChromaOffsetL1:               [[i8; 2]; 15],
        pub five_minus_max_num_merge_cand: u8,
        pub num_entry_point_offsets:      u16,
        pub entry_offset_to_subset_array: u16,
        pub slice_data_num_emu_prevn_bytes: u16,
        pub va_reserved: [u32; 2],
    }

    // libva symbols
    #[link(name = "va")]
    unsafe extern "C" {
        pub fn vaQueryConfigProfiles(
            dpy:          VADisplay,
            profile_list: *mut i32,
            num_profiles: *mut i32,
        ) -> VAStatus;

        pub fn vaMaxNumProfiles(dpy: VADisplay) -> i32;

        pub fn vaCreateSurfaces(
            dpy:         VADisplay,
            format:      u32,
            width:       u32,
            height:      u32,
            surfaces:    *mut VASurfaceID,
            num_surfaces: u32,
            attrib_list: *mut std::ffi::c_void,
            num_attribs: u32,
        ) -> VAStatus;
 
        pub fn vaDestroyConfig(
            dpy:    VADisplay,
            config: VAConfigID,
        ) -> VAStatus;
 
        pub fn vaDestroySurfaces(
            dpy:      VADisplay,
            surfaces: *mut VASurfaceID,
            num:      i32,
        ) -> VAStatus;

        pub fn vaBeginPicture(
            dpy:        VADisplay,
            context:    VAContextID,
            render_target: VASurfaceID,
        ) -> VAStatus;

        pub unsafe fn vaCreateBuffer(
            dpy:         VADisplay,
            context:     VAContextID,
            type_:       u32,
            size:        u32,
            num_elements: u32,
            data:        *mut std::ffi::c_void,
            buf_id:      *mut VABufferID,
        ) -> VAStatus;

        pub fn vaGetImage(
            dpy:       VADisplay,
            surface:   VASurfaceID,
            x:         i32,
            y:         i32,
            width:     u32,
            height:    u32,
            image_id:  VAImageID,
        ) -> VAStatus;

        pub unsafe fn vaRenderPicture(
            dpy:     VADisplay,
            context: VAContextID,
            buffers: *mut VABufferID,
            num_buffers: i32,
        ) -> VAStatus;

        pub unsafe fn vaEndPicture(
            dpy:     VADisplay,
            context: VAContextID,
        ) -> VAStatus;

        pub unsafe fn vaDestroyBuffer(
            dpy:    VADisplay,
            buf_id: VABufferID,
        ) -> VAStatus;

        pub unsafe fn vaSyncSurface(
            dpy:     VADisplay,
            surface: VASurfaceID,
        ) -> VAStatus;

        // Функции для работы с VAImage
        pub fn vaDeriveImage(
            dpy:     VADisplay,
            surface: VASurfaceID,
            image:   *mut VAImage,
        ) -> VAStatus;

        pub fn vaDestroyImage(
            dpy:      VADisplay,
            image_id: VAImageID,
        ) -> VAStatus;

        pub fn vaPutImage(
            dpy:        VADisplay,
            surface:    VASurfaceID,
            image:      VAImageID,
            src_x:      i32,
            src_y:      i32,
            src_width:  u32,
            src_height: u32,
            dst_x:      i32,
            dst_y:      i32,
            dst_width:  u32,
            dst_height: u32,
        ) -> VAStatus;


        pub fn vaMapBuffer(
            dpy:    VADisplay,
            buf_id: VABufferID,
            pbuf:   *mut *mut std::ffi::c_void,
        ) -> VAStatus;

        pub fn vaUnmapBuffer(
            dpy:    VADisplay,
            buf_id: VABufferID,
        ) -> VAStatus;

        pub fn vaCreateImage(
            dpy:    VADisplay,
            format: *const VAImageFormat,
            width:  i32,
            height: i32,
            image:  *mut VAImage,
        ) -> VAStatus;
    }
}
// ─────────────────────────────────────────────────────────────────────────────
// Хранилище состояния concealment — добавь в DesktopFfmpegBackend
// ─────────────────────────────────────────────────────────────────────────────

/// Добавь эти поля в DesktopFfmpegBackend:
///
/// ```rust
/// pub struct DesktopFfmpegBackend {
///     // ... существующие поля ...
///     concealment: VaapiConcealment,
/// }
/// ```
pub struct VaapiConcealment {
    /// VASurfaceID последнего успешно декодированного кадра.
    /// Обновляется в poll_output() при получении VAAPI frame.
    pub last_good_surface: va::VASurfaceID,

    /// Абсолютный POC последнего успешного кадра (для вычисления delta в RPS).
    pub last_good_abs_poc: i32,

    /// VAContextID декодера — берём из AVHWFramesContext.
    /// Заполняется при первом успешном poll_output().
    pub va_context: va::VAContextID,
    pub va_config:           va::VAConfigID,

    /// VADisplay — берём из AVVAAPIDeviceContext через hw_device_ctx.
    pub va_display: va::VADisplay,

    /// Параметры из SPS, нужны для заполнения VAPictureParameterBufferHEVC.
    pub pic_params_template: Option<ConcealmentPicParams>,

    pub concealment_surface: va::VASurfaceID,
}

/// Параметры, извлечённые из первого успешного VAPictureParameterBufferHEVC.
/// В реальности нужно перехватить при первом декодировании (см. ниже).
#[derive(Clone)]
pub struct ConcealmentPicParams {
    pub width:  u16,
    pub height: u16,
    pub log2_max_poc_lsb_minus4:            u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub init_qp_minus26:          i8,
    pub num_short_term_ref_pic_sets: u8,
    pub pps_id: u8,  // из последнего slice header
    pub num_ref_idx_l0_default_active_minus1: u8,
    /// Сырой pic_fields из оригинального VAPictureParameterBuffer
    pub pic_fields: u32,
    pub slice_parsing_fields: u32,

    pub deblocking_filter_override_enabled: bool,
    pub pps_disable_deblocking_filter:      bool,
    pub sps_temporal_mvp_enabled:           bool,
    pub sao_enabled:                        bool,
    pub cabac_init_present:                 bool,
    pub slice_chroma_qp_offsets_present:    bool,
    pub pps_cb_qp_offset:                   i8,
    pub pps_cr_qp_offset:                   i8,
    pub pps_beta_offset_div2:               i8,
    pub pps_tc_offset_div2:                 i8,
    pub log2_parallel_merge_level_minus2:   u8,
    pub num_extra_slice_header_bits:        u8,
    pub diff_cu_qp_delta_depth:             u8,
}

impl Default for VaapiConcealment {
    fn default() -> Self {
        Self {
            last_good_surface: va::VA_INVALID_SURFACE,
            last_good_abs_poc: 0,
            va_context:        va::VA_INVALID_ID,
            va_config:           va::VA_INVALID_ID,
            va_display:        ptr::null_mut(),
            pic_params_template: None,
            concealment_surface: va::VA_INVALID_SURFACE,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Шаг 1: Извлечение VADisplay из AVCodecContext
// ─────────────────────────────────────────────────────────────────────────────

/// Извлекает `VADisplay` из `hw_device_ctx` codec контекста.
///
/// Иерархия:
///   AVCodecContext.hw_device_ctx → AVBufferRef → AVHWDeviceContext
///   → AVVAAPIDeviceContext.display (это и есть VADisplay).
///
/// # Safety
/// `codec_ctx` должен быть инициализирован с VAAPI hw_device_ctx.
/// Результат валиден пока жив codec context.
pub unsafe fn extract_va_display(
    codec_ctx: *const AVCodecContext,
) -> Option<va::VADisplay> {
    if codec_ctx.is_null() {
        return None;
    }

    let hw_ref = (*codec_ctx).hw_device_ctx;
    if hw_ref.is_null() {
        log::error!("[Concealment] hw_device_ctx is null");
        return None;
    }

    // AVBufferRef.data → AVHWDeviceContext*
    let hw_dev_ctx = (*(*codec_ctx).hw_device_ctx).data as *const AVHWDeviceContext;
    if hw_dev_ctx.is_null() {
        log::error!("[Concealment] AVHWDeviceContext is null");
        return None;
    }

    // AVHWDeviceContext.hwctx → AVVAAPIDeviceContext* для VAAPI
    let vaapi_dev_ctx = (*hw_dev_ctx).hwctx as *const AVVAAPIDeviceContext;
    if vaapi_dev_ctx.is_null() {
        log::error!("[Concealment] AVVAAPIDeviceContext is null");
        return None;
    }

    let display = (*vaapi_dev_ctx).display;
    if display.is_null() {
        log::error!("[Concealment] VADisplay is null in AVVAAPIDeviceContext");
        return None;
    }

    log::debug!("[Concealment] VADisplay extracted: {:p}", display);
    Some(display)
}

/// Извлекает `VAContextID` из `hw_frames_ctx` декодированного AVFrame.
///
/// Это контекст конкретного surface pool декодера. Нужен для vaBeginPicture.
///
/// # Safety
/// `frame` — валидный AVFrame с форматом AV_PIX_FMT_VAAPI.
pub unsafe fn extract_va_context(frame: *const AVFrame) -> Option<va::VAContextID> {
    if frame.is_null() {
        return None;
    }

    let hw_frames_ref = (*frame).hw_frames_ctx;
    if hw_frames_ref.is_null() {
        log::warn!("[Concealment] hw_frames_ctx is null on VAAPI frame");
        return None;
    }

    let hw_frames_ctx = (*hw_frames_ref).data as *const AVHWFramesContext;
    if hw_frames_ctx.is_null() {
        return None;
    }

    // AVHWFramesContext.hwctx → AVVAAPIFramesContext*
    let vaapi_frames_ctx = (*hw_frames_ctx).hwctx as *const AVVAAPIFramesContext;
    if vaapi_frames_ctx.is_null() {
        return None;
    }

    // AVVAAPIFramesContext содержит surface_ids и nb_surfaces, но НЕ VAContextID напрямую.
    // VAContextID живёт в decoder context (AVCodecContext внутренних структурах).
    //
    // Обходной путь: VAContextID создаётся ffmpeg при avcodec_open2 и хранится
    // в приватном контексте декодера. Для iHD драйвера его можно достать через
    // vaapi_decode_context (непубличный тип ffmpeg).
    //
    // ПРАКТИЧЕСКОЕ РЕШЕНИЕ: Используем surface из frame.data[3] как VASurfaceID,
    // а VAContextID создаём сами (vaCreateContext) с теми же параметрами.
    // Но это сложно. Проще — хранить VAContextID из перехвата get_hw_format callback.
    //
    // Для iHD: context = surface_pool.context_id (внутреннее поле).
    // Мы возвращаем 0 как сигнал "нужно создать свой контекст".
    log::debug!("[Concealment] VAContextID extraction: use manual vaCreateContext path");
    Some(0) // placeholder — см. create_concealment_context()
}

/// Извлекает `VASurfaceID` из декодированного VAAPI AVFrame.
///
/// AVFrame(VAAPI).data[3] = VASurfaceID (приведённый к *mut u8).
///
/// # Safety
/// `frame` — валидный AVFrame формата AV_PIX_FMT_VAAPI.
pub unsafe fn extract_va_surface(frame: *const AVFrame) -> Option<va::VASurfaceID> {
    if frame.is_null() { return None; }

    // Убрали проверку формата — просто берём data[3]
    let surface_id = (*frame).data[3] as usize as va::VASurfaceID;
    if surface_id == va::VA_INVALID_SURFACE {
        log::warn!("[Concealment] data[3] is VA_INVALID_SURFACE");
        return None;
    }
    log::debug!("[Concealment] VASurfaceID from frame: {:#x}", surface_id);
    Some(surface_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// Шаг 2: Создание VA-контекста для concealment
// ─────────────────────────────────────────────────────────────────────────────

/// Создаём отдельный VAConfig + VAContext для concealment-рендеринга.
///
/// Это нужно потому что VAContextID декодера недоступен публично через ffmpeg API.
/// Мы создаём второй контекст с теми же параметрами — iHD это поддерживает.
///
/// # Параметры
/// - `display`: VADisplay из extract_va_display()
/// - `width`, `height`: размер кадра
///
/// # Возвращает
/// (VAConfigID, VAContextID) которые нужно сохранить и освободить при Drop.
pub unsafe fn create_concealment_context(
    display: va::VADisplay,
    width:   i32,
    height:  i32,
) -> Result<(va::VAConfigID, va::VAContextID, va::VASurfaceID), String> {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn vaCreateConfig(
            dpy: va::VADisplay, profile: i32, entrypoint: i32,
            attribs: *mut c_void, num: i32, id: *mut va::VAConfigID,
        ) -> va::VAStatus;
        fn vaCreateContext(
            dpy: va::VADisplay, config: va::VAConfigID,
            w: i32, h: i32, flag: i32,
            surfaces: *mut va::VASurfaceID, num: i32,
            ctx: *mut va::VAContextID,
        ) -> va::VAStatus;
    }

    // Пробуем профили в порядке: сначала стандартный (Intel/iHD = 23),
    // затем Mesa (AMD/RadeonSI = 4), затем перебираем остальные варианты.
    let candidates = [19i32, 21, 23, 4];

    let mut config_id: va::VAConfigID = va::VA_INVALID_ID;
    let mut used_profile = -1i32;

    for &profile in &candidates {
        let st = vaCreateConfig(
            display,
            profile,
            va::VAEntrypointVLD,
            ptr::null_mut(),
            0,
            &mut config_id,
        );
        if st == va::VA_STATUS_SUCCESS {
            used_profile = profile;
            log::info!("[Concealment] vaCreateConfig OK: profile={profile}");
            break;
        }
        log::debug!("[Concealment] vaCreateConfig profile={profile} → {st}");
    }

    if config_id == va::VA_INVALID_ID {
        return Err(format!(
            "[Concealment] vaCreateConfig failed for all HEVC profiles {:?}", candidates
        ));
    }


    let mut concealment_surf: va::VASurfaceID = va::VA_INVALID_SURFACE;
    let st = va::vaCreateSurfaces(
        display,
        va::VA_RT_FORMAT_YUV420,
        width as u32,
        height as u32,
        &mut concealment_surf,
        1,
        ptr::null_mut(),
        0,
    );
    if st != va::VA_STATUS_SUCCESS {
        // Не фатально — можно продолжить без отдельного surface,
        // но concealment не будет работать на radeonsi.
        return Err(format!(
            "[Concealment] vaCreateSurfaces failed: {} ({}x{})",
            st, width, height
        ));
    }

    let surfaces = [concealment_surf];
    let mut context_id: va::VAContextID = va::VA_INVALID_ID;
    let st = vaCreateContext(
        display,
        config_id,
        width,
        height,
        0x1,
        surfaces.as_ptr() as *mut _,
        1,
        &mut context_id,
    );

    if st != va::VA_STATUS_SUCCESS {
        return Err(format!(
            "[Concealment] vaCreateContext failed: {st} (profile={used_profile} size={width}x{height})"
        ));
    }

 
    log::info!(
        "[Concealment] VA context ready: profile={used_profile} \
         config={config_id:#x} ctx={context_id:#x} surf={concealment_surf:#x}"
    );
    Ok((config_id, context_id, concealment_surf))
}

// ─────────────────────────────────────────────────────────────────────────────
// Шаг 3: Генерация фиктивного NALU (битовый буфер slice header TRAIL_R)
// ─────────────────────────────────────────────────────────────────────────────

/// Результат генерации concealment slice.
pub struct ConcealmentSlice {
    /// Сырые байты NALU (включая start code 0x00 00 00 01).
    pub nalu_bytes: Vec<u8>,
    /// Смещение от начала nalu_bytes до первого байта после start code
    /// и NAL header (т.е. начало RBSP данных).
    pub rbsp_offset: usize,
    /// Количество бит в st_rps поле — нужно для VAPictureParameterBufferHEVC.st_rps_bits.
    pub st_rps_bits: u32,
    /// Смещение конца RPS (= начало slice_segment_header_extension_length area)
    /// от начала slice RBSP. Нужно для VASliceParameterBufferHEVC.slice_data_byte_offset.
    pub header_bits: u32,
}

/// Генерирует минимальный TRAIL_R slice header для concealment кадра.
///
/// Структура HEVC slice_segment_header (ITU-T H.265 §7.3.6.1):
///
/// ```text
/// first_slice_segment_in_pic_flag   u(1)   = 1
/// slice_pic_parameter_set_id         ue(v)  = pps_id
/// slice_type                         ue(v)  = 1 (P)
/// pic_order_cnt_lsb                  u(log2_max_poc_lsb) = poc_lsb
/// short_term_ref_pic_set_sps_flag    u(1)   = 0
/// short_term_ref_pic_set()           inline
/// num_ref_idx_active_override_flag   u(1)   = 0
/// slice_deblocking_filter_override_flag u(1)= 0  [если в PPS = enabled]
/// slice_qp_delta                     se(v)  = 0
/// cabac_init_flag                    u(1)   = 0  [если slice_type != I]
/// five_minus_max_num_merge_cand      ue(v)  = 4  (1 кандидат)
/// slice_segment_header_extension_length ue(v) = 0
/// byte_alignment()
/// ```
///
/// После генерации RBSP применяем emulation prevention bytes (0x000003).
pub fn generate_concealment_slice(
    pps_id:     u8,
    poc_lsb:    u32,
    log2_max_poc_lsb: u8,
    rps:        &ShortTermRps,
    params:     &ConcealmentPicParams,
) -> ConcealmentSlice {
    // ─────────────────────────────────────────────────────────────
    // 1. ПИШЕМ RBSP (БЕЗ NAL HEADER!)
    // ─────────────────────────────────────────────────────────────
    let mut w = BitWriter::new();

    // first_slice_segment_in_pic_flag = 1
    w.write_bit(true);

    // slice_pic_parameter_set_id
    w.write_ue(pps_id as u32);

    // slice_type = P
    w.write_ue(1);

    // poc_lsb
    w.write_bits(poc_lsb as u64, log2_max_poc_lsb as usize);

    // ── RPS ──────────────────────────────────────────────────────
    w.write_bit(false); // short_term_ref_pic_set_sps_flag = 0

    let rps_start = w.bit_len();
    rps.encode_inline(&mut w);
    let rps_end = w.bit_len();
    let st_rps_bits = (rps_end - rps_start) as u32;

    if params.slice_parsing_fields & (1 << 1) != 0 {
        w.write_ue(0);
        w.write_ue(0);
    }

    if params.sps_temporal_mvp_enabled {
        w.write_bit(false);
    }

    if params.sao_enabled {
        w.write_bit(false);
        w.write_bit(false);
    }

    // num_ref_idx_active_override_flag
    w.write_bit(false);

    if params.cabac_init_present {
        w.write_bit(false);
    }

    // merge candidates
    w.write_ue(4);

    // qp delta
    w.write_se(0);

    if params.slice_chroma_qp_offsets_present {
        w.write_se(0);
        w.write_se(0);
    }

    if params.deblocking_filter_override_enabled {
        w.write_bit(false);
    }

    if params.pic_fields & 0x3 != 0 {
        w.write_ue(0);
    }

    if params.slice_parsing_fields & (1 << 12) != 0 {
        w.write_ue(0);
    }

    // ── ВАЖНО: добавляем минимальный payload ─────────────────────
    // Без этого у тебя будет "11 байт и пока"
    w.write_bits(0, 8);

    // ── ALIGNMENT ────────────────────────────────────────────────
    let header_bits = w.bit_len();

    w.write_bit(true); // rbsp_stop_one_bit
    while w.bit_len() % 8 != 0 {
        w.write_bit(false);
    }

    let rbsp_bytes = w.into_bytes();

    // ─────────────────────────────────────────────────────────────
    // 2. EMULATION PREVENTION (ТОЛЬКО RBSP!)
    // ─────────────────────────────────────────────────────────────
    let escaped_rbsp = apply_emulation_prevention(&rbsp_bytes);

    // ─────────────────────────────────────────────────────────────
    // 3. СОБИРАЕМ NALU
    // ─────────────────────────────────────────────────────────────
    let mut nalu = Vec::with_capacity(4 + 2 + escaped_rbsp.len());

    // Annex B start code
    nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);

    // NAL HEADER (TRAIL_R)
    nalu.push(0x02);
    nalu.push(0x01);

    // RBSP
    nalu.extend_from_slice(&escaped_rbsp);

    ConcealmentSlice {
        rbsp_offset: 6, // 4 (start code) + 2 (NAL header)
        st_rps_bits,
        header_bits: header_bits as u32,
        nalu_bytes: nalu,
    }
}

/// Вставляет emulation prevention byte 0x03 согласно H.265 §7.4.1.
///
/// Правило: последовательность 0x00 0x00 {0x00, 0x01, 0x02, 0x03}
/// в RBSP должна быть экранирована как 0x00 0x00 0x03 {0x00, 0x01, 0x02, 0x03}.
fn apply_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 32);
    let mut zero_count = 0u32;

    for &byte in rbsp {
        if zero_count == 2 && byte <= 0x03 {
            out.push(0x03); // emulation prevention byte
            zero_count = 0;
        }

        if byte == 0x00 {
            zero_count += 1;
        } else {
            zero_count = 0;
        }

        out.push(byte);
    }

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Шаг 2 (продолжение): Заполнение VAPictureParameterBufferHEVC
// ─────────────────────────────────────────────────────────────────────────────

/// Заполняет `VAPictureParameterBufferHEVC` для concealment кадра.
///
/// # Ключевые поля для iHD
///
/// ## ReferenceFrames / CurrPic
/// iHD строго проверяет:
///   - VASurfaceID в ReferenceFrames должен быть реально выделен в том же пуле.
///   - pic_order_cnt должен точно соответствовать DPB состоянию декодера.
///   - Флаг VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE обязателен для P-slice.
///
/// ## st_rps_bits
/// Это не "размер RPS в структуре", а количество бит поля
/// `short_term_ref_pic_set` в bitstream заголовка слайса.
/// iHD использует его для расчёта смещения при аппаратном парсинге.
/// Берём значение из `ConcealmentSlice.st_rps_bits`.
pub unsafe fn fill_pic_param_hevc(
    last_surface:  va::VASurfaceID,
    concealment_surface: va::VASurfaceID,
    last_abs_poc:  i32,
    curr_poc_lsb:  u32,
    curr_abs_poc:  i32,
    state:         &HevcState,
    rps:           &ShortTermRps,
    params:        &ConcealmentPicParams,
    st_rps_bits:   u32,
) -> va::VAPictureParameterBufferHEVC {
    // Инициализируем все surface как невалидные
    let invalid_pic = va::VAPictureHEVC::default();
    let mut ref_frames = [invalid_pic; 15];
    let mut ref_idx = 0usize;

    // Заполняем референсы из DPB.
    // Для "freeze frame" concealment нам нужен хотя бы один — last_good_surface.
    //
    // iHD требование: если picture_id != VA_INVALID_SURFACE,
    // то он должен быть в текущем surface pool.
    // Используем только last_surface — он гарантированно там есть.

    // Negative refs (прошлые кадры) — последний успешный кадр
    for pic in &rps.negative {
        if ref_idx >= 15 { break; }
        // Для freeze frame все negative refs указывают на last_good_surface
        ref_frames[ref_idx] = va::VAPictureHEVC {
            picture_id:    last_surface,
            pic_order_cnt: pic.abs_poc,
            flags:         va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            va_reserved: [0u32; 4]
        };
        ref_idx += 1;
    }

    // Positive refs (будущие кадры) — тоже last_surface
    for pic in &rps.positive {
        if ref_idx >= 15 { break; }
        ref_frames[ref_idx] = va::VAPictureHEVC {
            picture_id:    last_surface,
            pic_order_cnt: pic.abs_poc,
            flags:         va::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER,
            va_reserved: [0u32; 4]
        };
        ref_idx += 1;
    }

    // Если DPB пуст — добавляем last_surface как самореференс.
    // Это создаёт "copy предыдущего кадра" эффект.
    if ref_idx == 0 {
        ref_frames[0] = va::VAPictureHEVC {
            picture_id:    last_surface,
            pic_order_cnt: last_abs_poc,
            flags:         va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            va_reserved: [0u32; 4]
        };
    }

    // pic_fields bits (упрощённо для HEVC Main Profile):
    //
    // Бит 0:  tiles_enabled_flag = 0
    // Бит 1:  entropy_coding_sync_enabled_flag = 0
    // Бит 2:  sign_data_hiding_enabled_flag = 0
    // Бит 3:  scaling_list_enabled_flag = 0
    // Бит 4:  transform_skip_enabled_flag = 0
    // Бит 5:  amp_enabled_flag = 1  (обычно включён для Main Profile)
    // Бит 6:  strong_intra_smoothing_enabled_flag = 1
    // Бит 7:  pcm_enabled_flag = 0
    // Бит 8:  pcm_loop_filter_disabled_flag = 0
    // Бит 9:  weighted_pred_flag = 0
    // Бит 10: weighted_bipred_flag = 0
    // Бит 11: loop_filter_across_tiles_enabled_flag = 0
    // Бит 12: loop_filter_across_slices_enabled_flag = 1
    // Бит 13: output_flag_present_flag = 0
    // Бит 14: num_extra_slice_header_bits (в поле отдельно)
    // Бит 15: pps_slice_chroma_qp_offsets_present_flag = 0
    // Бит 16: deblocking_filter_override_enabled_flag = 0
    // Бит 17: pps_disable_deblocking_filter_flag = 0
    // Бит 18: lists_modification_present_flag = 0
    // Бит 19: slice_segment_header_extension_present_flag = 0
    // Бит 20: high_precision_offsets_enabled_flag = 0 (REXT)
    // Бит 21: chroma_qp_offset_list_enabled_flag = 0 (REXT)
    //
    // Для concealment используем шаблон из реального кадра (params.pic_fields),
    // чтобы точно совпасть с тем, что ожидает iHD.
    let pic_fields = params.pic_fields;

    // slice_parsing_fields:
    // Бит 0: lists_modification_present_flag = 0
    // Бит 1: long_term_ref_pics_present_flag = 0  ← важно для iHD
    // Бит 2: sps_temporal_mvp_enabled_flag = 0    ← выключаем для simplicity
    // Бит 3: cabac_init_present_flag = 0
    // Бит 4: output_flag_present_flag = 0
    // Бит 5: dependent_slice_segments_enabled_flag = 0
    // Бит 6: pps_slice_chroma_qp_offsets_present_flag = 0
    // Бит 7: sample_adaptive_offset_enabled_flag = 0
    // Бит 8: luma_bit_depth_entry_minus8 (в другом поле)
    // Бит 9: chroma_bit_depth_entry_minus8 (в другом поле)
    // Бит 10: deblocking_filter_override_enabled_flag = 0
    // Бит 11: pps_disable_deblocking_filter_flag = 0
    // Бит 12: slice_segment_header_extension_present_flag = 0
    // Бит 13: RapPicFlag = 0 (TRAIL_R — не RAP)
    // Бит 14: IdrPicFlag = 0
    // Бит 15: IntraPicFlag = 0 (P-frame)
    let slice_parsing_fields = params.slice_parsing_fields
        & !(1 << 13)  // RapPicFlag = 0
        & !(1 << 14)  // IdrPicFlag = 0
        & !(1 << 15); // IntraPicFlag = 0

    va::VAPictureParameterBufferHEVC {
        CurrPic: va::VAPictureHEVC {
            picture_id:    concealment_surface, // рендерим В тот же surface
            pic_order_cnt: curr_abs_poc,
            flags:         0,
            va_reserved: [0u32; 4]            // текущий кадр — без флагов
        },
        ReferenceFrames: ref_frames,
        pic_width_in_luma_samples:  params.width,
        pic_height_in_luma_samples: params.height,
        pic_fields,
        sps_max_dec_pic_buffering_minus1: state.dpb.len().saturating_sub(1).min(15) as u8,
        bit_depth_luma_minus8:   0,
        bit_depth_chroma_minus8: 0,
        pcm_sample_bit_depth_luma_minus1:   7,
        pcm_sample_bit_depth_chroma_minus1: 7,
        log2_min_luma_coding_block_size_minus3: params.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: params.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: params.log2_min_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: params.log2_diff_max_min_transform_block_size,
        log2_min_pcm_luma_coding_block_size_minus3: 0,
        log2_diff_max_min_pcm_luma_coding_block_size: 0,
        max_transform_hierarchy_depth_intra: params.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: params.max_transform_hierarchy_depth_inter,
        init_qp_minus26:         params.init_qp_minus26,
        diff_cu_qp_delta_depth:  params.diff_cu_qp_delta_depth,
        pps_cb_qp_offset:        params.pps_cb_qp_offset,
        pps_cr_qp_offset:        params.pps_cr_qp_offset,
        log2_parallel_merge_level_minus2: params.log2_parallel_merge_level_minus2,
        num_tile_columns_minus1: 0,
        num_tile_rows_minus1:    0,
        column_width_minus1:     [0u16; 19],
        row_height_minus1:       [0u16; 21],
        slice_parsing_fields,
        log2_max_pic_order_cnt_lsb_minus4: params.log2_max_poc_lsb_minus4,
        num_short_term_ref_pic_sets: params.num_short_term_ref_pic_sets,
        num_long_term_ref_pic_sps:   0,
        num_ref_idx_l0_default_active_minus1: params.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: 0,
        pps_beta_offset_div2:    params.pps_beta_offset_div2,
        pps_tc_offset_div2:      params.pps_tc_offset_div2,
        num_extra_slice_header_bits: params.num_extra_slice_header_bits,
        // ── Ключевое поле для iHD ─────────────────────────────────────────
        // st_rps_bits = количество бит поля short_term_ref_pic_set в заголовке слайса,
        // включая sps_flag бит.
        // Аппаратный парсер iHD использует это для вычисления:
        //   slice_header_start + poc_lsb_bits + st_rps_bits → позиция после RPS
        // Если ошибиться — декодер неправильно прочитает slice_qp_delta и т.д.
        st_rps_bits,
        va_reserved: [0u32; 8],
    }
}

/// Заполняет VASliceParameterBufferHEVC для минимального P-slice.
///
/// # Важные поля
///
/// - `RefPicList[0]` — список l0 референсов (индексы в ReferenceFrames).
///   Для freeze frame: [0, 0xFF, 0xFF, ...] — только первый референс.
/// - `LongSliceFlags` — битовое поле, бит 0 = slice_type (0=B,1=P,2=I).
///   Бит 2 = dependent_slice_segment_flag = 0.
pub fn fill_slice_param_hevc(
    slice: &ConcealmentSlice,
    rps:   &ShortTermRps,
    params: &ConcealmentPicParams,
) -> va::VASliceParameterBufferHEVC {
    // RefPicList[0]: l0 (backward refs для P-slice)
    let mut ref_list_0 = [0xFFu8; 15];
    let num_l0 = rps.negative.len().min(15);
    for i in 0..num_l0 {
        ref_list_0[i] = i as u8; // индексы в VAPictureParameterBufferHEVC.ReferenceFrames
    }

    // Если нет референсов — указываем на первую запись (= last_good_surface)
    if num_l0 == 0 {
        ref_list_0[0] = 0;
    }

    // LongSliceFlags bits (va/va_dec_hevc.h, struct VASliceLongFlags):
    // Бит 0:  LastSliceOfPic = 1 (единственный слайс)
    // Бит 1:  dependent_slice_segment_flag = 0
    // Бит 2:  slice_type (поле 2 бита): 01 = P
    // Бит 4:  color_plane_id = 0
    // Бит 5:  slice_sao_luma_flag = 0
    // Бит 6:  slice_sao_chroma_flag = 0
    // Бит 7:  mvd_l1_zero_flag = 0 (P-slice, L1 не используется)
    // Бит 8:  cabac_init_flag = 0
    // Бит 9:  slice_temporal_mvp_enabled_flag = 0
    // Бит 10: slice_deblocking_filter_disabled_flag = 0
    // Бит 11: collocated_from_l0_flag = 1
    // Бит 12: slice_loop_filter_across_slices_enabled_flag = 1
    let long_flags: u32 = (1 << 0)  // LastSliceOfPic
                        | (0b01 << 2)  // slice_type = P
                        | (1 << 11) // collocated_from_l0_flag
                        | (1 << 12); // loop_filter_across_slices

    // slice_data_byte_offset: смещение от начала slice NALU до данных.
    // = (header_bits + 7) / 8 + 2 (NAL header bytes)
    // Annex B start code (4 байта) НЕ считается — offset относительно NALU после start code.
    let header_bytes = (slice.header_bits + 7) / 8;

    let slice_data = &slice.nalu_bytes;

    let slice_data_size = slice_data.len() as u32;
    let slice_data_offset = 4; // пропускаем 00 00 00 01
    let byte_offset: u32 = (2 + header_bytes) as u32;

    va::VASliceParameterBufferHEVC {
        slice_data_size: slice_data_size, // без start code
        slice_data_offset: slice_data_offset,
        slice_data_flag: 0x01, // VA_SLICE_DATA_FLAG_ALL
        slice_data_byte_offset: 2,
        slice_segment_address: 0, // first slice
        RefPicList: [ref_list_0, [0xFF; 15]],
        LongSliceFlags: long_flags,
        collocated_ref_idx: 0xFF,
        num_ref_idx_l0_active_minus1: num_l0.saturating_sub(1).max(0) as u8,
        num_ref_idx_l1_active_minus1: 0,
        slice_qp_delta: 0,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        slice_beta_offset_div2: 0,
        slice_tc_offset_div2: 0,
        luma_log2_weight_denom: 0,
        delta_chroma_log2_weight_denom: 0,
        delta_luma_weight_l0: [0; 15],
        luma_offset_l0: [0; 15],
        delta_chroma_weight_l0: [[0; 2]; 15],
        ChromaOffsetL0: [[0; 2]; 15],
        delta_luma_weight_l1: [0; 15],
        luma_offset_l1: [0; 15],
        delta_chroma_weight_l1: [[0; 2]; 15],
        ChromaOffsetL1: [[0; 2]; 15],
        five_minus_max_num_merge_cand: 4,
        num_entry_point_offsets: 0,
        entry_offset_to_subset_array: 0,
        slice_data_num_emu_prevn_bytes: count_emulation_prevention_bytes(slice_data) as u16,
        va_reserved: [0u32; 2],
    }
}

/// Считает количество emulation prevention bytes (0x03) в NALU.
fn count_emulation_prevention_bytes(nalu: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 2usize; // начинаем с третьего байта (после NAL header)
    while i < nalu.len() {
        if i >= 2
            && nalu[i - 2] == 0x00
            && nalu[i - 1] == 0x00
            && nalu[i] == 0x03
        {
            count += 1;
            i += 1; // пропускаем 0x03
        }
        i += 1;
    }
    count
}

// ─────────────────────────────────────────────────────────────────────────────
// Шаг 4: Рендеринг — vaBeginPicture / vaRenderPicture / vaEndPicture
// ─────────────────────────────────────────────────────────────────────────────

/// Выполняет полный цикл VA-API рендеринга для concealment кадра.
///
/// # Контракт
///
/// - `render_target` = `last_good_surface`: кадр рендерится В предыдущий surface.
///   Это создаёт "freeze" эффект — iHD перезапишет surface skip MB данными,
///   фактически скопировав motion vectors из reference.
///
/// - Порядок буферов строго фиксирован (iHD требование):
///   1. VAPictureParameterBufferType
///   2. VASliceParameterBufferType  
///   3. VASliceDataBufferType
///   (VAIQMatrixBufferType не нужен для skip-frame с QP=0 delta)
///
/// # Safety
/// `display` и `context` должны быть валидны. `render_target` должен быть
/// в том же surface pool что и контекст.

pub unsafe fn render_concealment_frame(
    display:       va::VADisplay,
    context:       va::VAContextID,
    render_target: va::VASurfaceID,
    pic_param:     &va::VAPictureParameterBufferHEVC,
    slice_param:   &va::VASliceParameterBufferHEVC,
    slice_data:    &[u8], // NALU bytes без start code (4 байта)
) -> Result<(), String> {
    use std::ffi::c_void;
    va::vaSyncSurface(display, render_target);
    
    log::info!(
        "[Concealment] struct sizes: PicParam={} SliceParam={} VAPic={}",
        std::mem::size_of::<va::VAPictureParameterBufferHEVC>(),
        std::mem::size_of::<va::VASliceParameterBufferHEVC>(),
        std::mem::size_of::<va::VAPictureHEVC>(),
    );
    // ── vaBeginPicture ────────────────────────────────────────────────────────
    //
    // render_target = last_good_surface: iHD будет использовать его
    // как выходной буфер И как референс (через ReferenceFrames[0]).
    // На iHD это легально — surface может быть одновременно CurrPic и ref.
    let st = va::vaBeginPicture(display, context, render_target);
    if st != va::VA_STATUS_SUCCESS {
        return Err(format!("vaEndPicture: {} ({})", st, va_error_str(st)));
    }

    log::debug!(
        "[Concealment] vaBeginPicture: ctx={:#x} surface={:#x}",
        context, render_target
    );

    // ── Создаём VA буферы ─────────────────────────────────────────────────────

    let mut pic_buf_id:   va::VABufferID = va::VA_INVALID_ID;
    let mut slice_buf_id: va::VABufferID = va::VA_INVALID_ID;
    let mut data_buf_id:  va::VABufferID = va::VA_INVALID_ID;

    // 1. PictureParameter buffer
    let st = va::vaCreateBuffer(
        display,
        context,
        va::VAPictureParameterBufferType,
        std::mem::size_of::<va::VAPictureParameterBufferHEVC>() as u32,
        1,
        pic_param as *const _ as *mut c_void,
        &mut pic_buf_id,
    );
    if st != va::VA_STATUS_SUCCESS {
        va::vaEndPicture(display, context);
        return Err(format!("VaPic error: {} ({})", st, va_error_str(st)));
    }

    // 2. SliceParameter buffer
    let st = va::vaCreateBuffer(
        display,
        context,
        va::VASliceParameterBufferType,
        std::mem::size_of::<va::VASliceParameterBufferHEVC>() as u32,
        1,
        slice_param as *const _ as *mut c_void,
        &mut slice_buf_id,
    );
    if st != va::VA_STATUS_SUCCESS {
        va::vaDestroyBuffer(display, pic_buf_id);
        va::vaEndPicture(display, context);
        return Err(format!("VaCreateBufer error: {} ({})", st, va_error_str(st)));
    }

    // 3. SliceData buffer (сырые байты NALU без start code)
    let st = va::vaCreateBuffer(
        display,
        context,
        va::VASliceDataBufferType,
        slice_data.len() as u32,
        1,
        slice_data.as_ptr() as *mut c_void,
        &mut data_buf_id,
    );
    if st != va::VA_STATUS_SUCCESS {
        va::vaDestroyBuffer(display, pic_buf_id);
        va::vaDestroyBuffer(display, slice_buf_id);
        va::vaEndPicture(display, context);
        return Err(format!("VaCreateBuffer2 error: {} ({})", st, va_error_str(st)));
    }

    // ── vaRenderPicture ───────────────────────────────────────────────────────
    //
    // Порядок ВАЖЕН: PicParam → SliceParam → SliceData.
    // iHD парсит их в этом порядке; перепутать = VA_STATUS_ERROR_INVALID_BUFFER.

    let mut buffers = [pic_buf_id, slice_buf_id, data_buf_id];
    let st = va::vaRenderPicture(
        display,
        context,
        buffers.as_mut_ptr(),
        buffers.len() as i32,
    );

    // Освобождаем буферы сразу после vaRenderPicture — iHD скопировал данные.
    va::vaDestroyBuffer(display, pic_buf_id);
    va::vaDestroyBuffer(display, slice_buf_id);
    va::vaDestroyBuffer(display, data_buf_id);

    if st != va::VA_STATUS_SUCCESS {
        va::vaEndPicture(display, context);
        return Err(format!("vaEndPicture: {} ({})", st, va_error_str(st)));
    }

    log::debug!("[Concealment] vaRenderPicture: OK");

    // ── vaEndPicture ──────────────────────────────────────────────────────────
    //
    // Запускает аппаратное декодирование. После этого поверхность
    // асинхронно заполняется — нужен vaSyncSurface перед чтением.
    let st = va::vaEndPicture(display, context);
    if st != va::VA_STATUS_SUCCESS {
        return Err(format!("vaEndPicture: {} ({})", st, va_error_str(st)));
    }

    // ── Синхронизация ─────────────────────────────────────────────────────────
    //
    // Ждём завершения декодирования перед тем как отдать surface обратно
    // в FFmpeg pipeline. Без этого — data race на GPU.
    let st = va::vaSyncSurface(display, render_target);
    if st != va::VA_STATUS_SUCCESS {
        log::warn!("[Concealment] vaSyncSurface warning: {st}");
        // Не фатально — продолжаем
    }

    log::info!(
        "[Concealment] vaEndPicture + Sync OK: surface={:#x}",
        render_target
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Точка входа: высокоуровневый concealment
// ─────────────────────────────────────────────────────────────────────────────

/// Выполняет полный цикл VA-API error concealment.
///
/// Вызывается из `conceal_and_clear()` трейта VideoBackend вместо (или после)
/// существующего FFmpeg-пути.
///
/// # Использование в DesktopFfmpegBackend
///
/// ```rust
/// fn conceal_and_clear(&mut self, lost_frame_id: u64) {
///     // Существующий FFmpeg-путь (submit_to_decoder skip frame)
///     // ...
///
///     // VA-API путь (прямой freeze)
///     if let Err(e) = vaapi_concealment_freeze(
///         &mut self.concealment,
///         &self.hevc_state,
///         self.poc_lsb,
///         &self.skip_frame_template,
///     ) {
///         log::warn!("[Concealment] VA-API path failed: {e}");
///     }
///
///     self.clear_buffer();
/// }
/// ```
pub unsafe fn vaapi_concealment_freeze(
    concealment: &mut VaapiConcealment,
    state:       &HevcState,
    poc_lsb:     u32,
    template:    &Option<SkipFrameTemplate>,
    sps:         &Option<SpsFields>,
    pps:         &Option<PpsFields>,
    vps:         &Option<Vec<u8>>,
) -> Result<(), String> {
    // Проверяем готовность
    if concealment.last_good_surface == va::VA_INVALID_SURFACE {
        return Err("No good surface captured yet".into());
    }
    if concealment.va_display.is_null() {
        return Err("VADisplay not initialized".into());
    }
    if concealment.va_context == va::VA_INVALID_ID || concealment.va_context == 0 {
        return Err("VAContext not initialized".into());
    }
    let params = concealment.pic_params_template.as_ref()
        .ok_or("PicParams template not captured")?;

    if concealment.concealment_surface == va::VA_INVALID_SURFACE {
        return Err("Concealment output surface not created".into());
    }

    // Вычисляем POC следующего (потерянного) кадра
    let curr_abs_poc = state.derive_abs_poc(poc_lsb);
    let last_good_poc = concealment.last_good_abs_poc;
    let delta = last_good_poc - curr_abs_poc; // отрицательное, т.к. last < curr
    if delta == 0 {
        return Err("last_good_poc == curr_abs_poc, cannot build RPS".into());
    }
    log::info!("[Concealment] Delta is: {}", delta);
    // Берём RPS из состояния DPB
    let rps = ShortTermRps {
        negative: vec![crate::backend::RefPic {
            abs_poc: last_good_poc,
            delta_poc: delta,
            used_by_curr: true,
            is_long_term: false,
        }],
        positive: vec![],
    };

    log::info!(
        "[Concealment] RPS: curr_abs_poc={} last_good_poc={} delta={}",
        curr_abs_poc, last_good_poc, delta
    );

    // Генерируем slice NALU
    let slice = generate_concealment_slice(
        params.pps_id,
        poc_lsb,
        params.log2_max_poc_lsb_minus4 + 4,
        &rps,
        params,
    );

    log::debug!(
        "[Concealment] VA-API freeze: poc_lsb={} abs_poc={} surface={:#x} \
         st_rps_bits={} refs={}",
        poc_lsb,
        curr_abs_poc,
        concealment.last_good_surface,
        slice.st_rps_bits,
        rps.total_refs(),
    );

    // Заполняем параметры
    let pic_param = fill_pic_param_hevc_self(
        concealment.concealment_surface, // КУДА
        curr_abs_poc,
        concealment.last_good_surface,    // ОТКУДА (0x7 из твоего лога)
        last_good_poc,
        params,
        slice.st_rps_bits,
    );

    let slice_param = fill_slice_param_hevc(&slice, &rps, params);

    // Данные слайса без Annex B start code (4 байта)
    let slice_data = &slice.nalu_bytes[4..];
    let vps_raw = vps.as_deref();
    dump_debug_hevc(
        vps_raw.unwrap_or_default(),
        sps,
        pps,
        &slice.nalu_bytes
    );
    // Рендерим
    (unsafe { render_concealment_frame(
        concealment.va_display,
        concealment.va_context,
        concealment.concealment_surface,
        &pic_param,
        &slice_param,
        slice_data,
    ) })?;

    // Обновляем состояние concealment
    concealment.last_good_abs_poc = curr_abs_poc;

    Ok(())
}

unsafe fn fill_pic_param_hevc_self(
    curr_surface: va::VASurfaceID,     // Куда рендерим (Concealment Surface)
    curr_abs_poc: i32,
    last_surface: va::VASurfaceID,     // Откуда берем данные (Last Good Surface)
    last_abs_poc: i32,
    params: &ConcealmentPicParams,
    st_rps_bits: u32,
) -> va::VAPictureParameterBufferHEVC {
    let ref_pic = va::VAPictureHEVC {
        picture_id:    last_surface,
        pic_order_cnt: last_abs_poc,
        flags:         va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
        va_reserved: [0u32; 4]
    };
    
    // Инициализируем массив невалидными ID, кроме первого слота
    let mut ref_frames = [va::VAPictureHEVC {
        picture_id: va::VA_INVALID_ID,
        pic_order_cnt: 0,
        flags: va::VA_PICTURE_HEVC_INVALID,
        va_reserved: [0u32; 4]
    }; 15];
    
    ref_frames[0] = ref_pic;

    va::VAPictureParameterBufferHEVC {
        CurrPic: va::VAPictureHEVC {
            picture_id:    curr_surface,
            pic_order_cnt: curr_abs_poc,
            flags:         0,
            va_reserved: [0u32; 4]
        },
        ReferenceFrames: ref_frames,
        pic_width_in_luma_samples: params.width,
        pic_height_in_luma_samples: params.height,
        pic_fields: params.pic_fields,
        sps_max_dec_pic_buffering_minus1: 0,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        pcm_sample_bit_depth_luma_minus1: 7,
        pcm_sample_bit_depth_chroma_minus1: 7,
        log2_min_luma_coding_block_size_minus3: params.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: params.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: params.log2_min_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: params.log2_diff_max_min_transform_block_size,
        log2_min_pcm_luma_coding_block_size_minus3: 0,
        log2_diff_max_min_pcm_luma_coding_block_size: 0,
        max_transform_hierarchy_depth_intra: params.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: params.max_transform_hierarchy_depth_inter,
        init_qp_minus26: params.init_qp_minus26,
        diff_cu_qp_delta_depth: params.diff_cu_qp_delta_depth,
        pps_cb_qp_offset: params.pps_cb_qp_offset,
        pps_cr_qp_offset: params.pps_cr_qp_offset,
        log2_parallel_merge_level_minus2: params.log2_parallel_merge_level_minus2,
        num_tile_columns_minus1: 0,
        num_tile_rows_minus1: 0,
        column_width_minus1: [0; 19],
        row_height_minus1: [0; 21],
        slice_parsing_fields: params.slice_parsing_fields & !(1<<13) & !(1<<14) & !(1<<15),
        log2_max_pic_order_cnt_lsb_minus4: params.log2_max_poc_lsb_minus4,
        num_short_term_ref_pic_sets: params.num_short_term_ref_pic_sets,
        num_long_term_ref_pic_sps: 0,
        num_ref_idx_l0_default_active_minus1: params.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: 0,
        pps_beta_offset_div2: params.pps_beta_offset_div2,
        pps_tc_offset_div2: params.pps_tc_offset_div2,
        num_extra_slice_header_bits: params.num_extra_slice_header_bits,
        st_rps_bits,
        va_reserved: [0; 8],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Захват данных из успешно декодированных кадров
// ─────────────────────────────────────────────────────────────────────────────
//
// Вызывай эти функции в poll_output() при получении VAAPI frame:
//
// ```rust
// fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
//     let mut raw = Video::empty();
//     if self.decoder.receive_frame(&mut raw).is_err() {
//         return Ok(FrameOutput::Pending);
//     }
//
//     let fmt = unsafe { ... };
//
//     if fmt == Pixel::VAAPI {
//         // Захватываем данные для concealment
//         unsafe {
//             capture_concealment_state(
//                 &mut self.concealment,
//                 raw.as_ptr(),
//                 self.decoder.as_ptr(),  // *mut AVCodecContext
//                 self.hevc_state.derive_abs_poc(self.poc_lsb),
//             );
//         }
//     }
//
//     // ... остальная обработка
// }
// ```

/// Захватывает VASurfaceID, VADisplay и VAContext из успешно декодированного кадра.
///
/// Должна вызываться при каждом успешном receive_frame с VAAPI форматом.
///
/// # Safety
/// `frame` — валидный AVFrame(VAAPI), `codec_ctx` — открытый AVCodecContext.
pub unsafe fn capture_concealment_state(
    concealment: &mut VaapiConcealment,
    frame:       *const AVFrame,
    codec_ctx:   *const AVCodecContext,
    curr_abs_poc: i32,
) {
    // Захватываем VASurfaceID
    if let Some(surface) = extract_va_surface(frame) {
        concealment.last_good_surface = surface;
        concealment.last_good_abs_poc = curr_abs_poc;
    }

    // VADisplay захватываем один раз
    if concealment.va_display.is_null() {
        if let Some(display) = extract_va_display(codec_ctx) {
            concealment.va_display = display;
            log::info!("[Concealment] VADisplay captured: {:p}", display);
        }
    }

    // VAContext: если ещё не создан — создаём
    // (используем размер кадра из AVFrame)
    if (concealment.va_context == va::VA_INVALID_ID || concealment.va_context == 0)
        && !concealment.va_display.is_null()
        && !frame.is_null()
    {
        let w = (*frame).width;
        let h = (*frame).height;
        
        match create_concealment_context(concealment.va_display, w, h) {
            Ok((cfg, ctx, surf)) => {
                concealment.va_config           = cfg;
                concealment.va_context          = ctx;
                concealment.concealment_surface = surf;
            }
            Err(e) => {
                log::warn!("[Concealment] Failed to create VAContext: {e}");
            }
        }
    }
}

/// Захватывает параметры для ConcealmentPicParams из реального кадра.
///
/// В идеале вызывать ОДИН РАЗ при получении первого VAAPI кадра.
/// Параметры стабильны в рамках одной сессии.
///
/// # Как получить реальные значения VAPictureParameterBufferHEVC
///
/// FFmpeg не предоставляет доступа к уже отправленным VA буферам.
/// Альтернативы:
///
/// 1. **Parсить SPS/PPS напрямую** (рекомендуется):
///    У тебя уже есть `parse_sps_poc_bits` — расширь его для извлечения всех нужных полей.
///
/// 2. **Перехват через av_opt_set_callback** (сложно, нет публичного API).
///
/// 3. **Хранить значения по умолчанию для Main Profile** (для большинства кейсов достаточно).
///
/// Ниже — вариант 3 (разумные дефолты для H.265 Main Profile @ Level 4.1):
pub fn make_default_pic_params(
    width:  u16,
    height: u16,
    log2_max_poc_lsb: u8,
    pps_id: u8,
) -> ConcealmentPicParams {
    // pic_fields для HEVC Main Profile (разумные дефолты):
    // Бит  5: amp_enabled_flag = 1
    // Бит  6: strong_intra_smoothing_enabled_flag = 1
    // Бит 12: loop_filter_across_slices_enabled_flag = 1
    let pic_fields: u32 = (1 << 5) | (1 << 6) | (1 << 12);

    // slice_parsing_fields: всё 0 (temporal_mvp выключен, long_term выключен)
    // Это безопасный дефолт — если SPS/PPS ещё не пришли.
    let slice_parsing_fields: u32 = 0;

    ConcealmentPicParams {
        width,
        height,
        log2_max_poc_lsb_minus4: log2_max_poc_lsb.saturating_sub(4),
        log2_min_luma_coding_block_size_minus3:   0, // min CTB = 8
        log2_diff_max_min_luma_coding_block_size: 3, // max CTB = 64
        log2_min_transform_block_size_minus2:     0, // min TB = 4
        log2_diff_max_min_transform_block_size:   3, // max TB = 32
        max_transform_hierarchy_depth_intra: 2,
        max_transform_hierarchy_depth_inter: 2,
        init_qp_minus26: 0,
        num_short_term_ref_pic_sets: 0,
        pps_id,
        num_ref_idx_l0_default_active_minus1: 0,
        pic_fields,
        slice_parsing_fields,
        // Новые поля — консервативные дефолты:
        // Всё выключено, чтобы generate_concealment_slice не писал
        // conditional поля которых нет в реальном bitstream.
        deblocking_filter_override_enabled: false,
        pps_disable_deblocking_filter:      false,
        sps_temporal_mvp_enabled:           false,
        sao_enabled:                        false,
        cabac_init_present:                 false,
        slice_chroma_qp_offsets_present:    false,
        pps_cb_qp_offset:                   0,
        pps_cr_qp_offset:                   0,
        pps_beta_offset_div2:               0,
        pps_tc_offset_div2:                 0,
        log2_parallel_merge_level_minus2:   0,
        num_extra_slice_header_bits:        0,
        diff_cu_qp_delta_depth:             0,
    }
}

unsafe extern "C" {
    fn vaErrorStr(status: va::VAStatus) -> *const std::ffi::c_char;
}

fn va_error_str(status: va::VAStatus) -> String {
    unsafe {
        let ptr = vaErrorStr(status);
        if ptr.is_null() { return format!("unknown({})", status); }
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().to_string()
    }
}