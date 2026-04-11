// src/main.rs
//
// Десктопная точка входа (Linux / Windows).
//
// ## Потоки
//
// ```text
// main()
//   ├── thread::spawn → tokio RT → run_quic_receiver
//   │       (DesktopFfmpegBackend + mpsc::SyncSender<DecodedFrame>)
//   │
//   └── EventLoop::run_app (winit, главный поток)
//           about_to_wait → try_recv DecodedFrame:
//             ├── Yuv(YuvFrame)    → queue.write_texture (старый CPU-путь)
//             └── DmaBuf(frame)   → Vulkan import fd → wgpu::Texture (zero-copy)
// ```
//
// ## Добавить в Cargo.toml
//
// ```toml
// [dependencies]
// ash = "0.38"           # Vulkan bindings (должна совпадать с версией внутри wgpu)
// ```
//
// ## DMA-BUF → Vulkan import
//
// 1. При старте WgpuState::new() извлекаем из wgpu::Device raw ash::Device +
//    ash::Instance через адаптер. Строим DmaBufAllocator.
// 2. На каждый DmaBufFrame дважды дублируем fd (Y и UV плоскости) и вызываем
//    `vkImportMemoryFdKHR` для каждой плоскости.
// 3. Создаём два VkImage (R8Unorm/Rg8Unorm) и привязываем к ним память
//    через vkBindImageMemory с соответствующими offset.
// 4. Оборачиваем VkImage в wgpu::Texture через device.create_texture_from_hal.
//    drop_guard в wgpu::hal::vulkan::Texture уничтожает VkImage + VkDeviceMemory
//    при drop().
// 5. Создаём BindGroup и рендерим — шейдер не меняется.

use std::error::Error;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
#[cfg(target_os = "linux")]
use ash::vk;
use common::FrameTrace;
use tokio::sync::mpsc;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

use stream_receiver::backend::YuvFrame;

#[cfg(target_os = "macos")]
use stream_receiver::backend::macos::MacosFfmpegBackend;

#[cfg(not(target_os = "macos"))]
use stream_receiver::backend::desktop::DesktopFfmpegBackend;
use stream_receiver::network::run_quic_receiver;

#[cfg(target_os = "linux")]
use stream_receiver::types::DmaBufFrame;
use stream_receiver::types::{DecodedFrame};
use stream_receiver::UserEvent;

// ─────────────────────────────────────────────────────────────────────────────
// Канальный тип: CPU-кадр или zero-copy DMA-BUF кадр
// ─────────────────────────────────────────────────────────────────────────────

/// Декодированный кадр, передаваемый из сетевого потока в рендер-поток.
///
/// Вариант определяется в рантайме: если `DesktopFfmpegBackend` смог поднять
/// zero-copy путь (DRM-производный VAAPI + DMA-BUF), то приходит `DmaBuf`.
/// Иначе — `Yuv` (CPU-копирование, старый путь).


// ─────────────────────────────────────────────────────────────────────────────
// DMA-BUF Allocator (Vulkan import, Linux-only)
// ─────────────────────────────────────────────────────────────────────────────

/// Держит raw Vulkan handles для импорта DMA-BUF fd в wgpu-текстуры.
///
/// Создаётся один раз при инициализации WgpuState если необходимые
/// Vulkan-расширения доступны:
/// - `VK_KHR_external_memory`
/// - `VK_KHR_external_memory_fd`
/// - `VK_EXT_external_memory_dma_buf`
#[cfg(target_os = "linux")]
struct DmaBufAllocator {
    device:    ash::Device,
    ext_mem:   ash::khr::external_memory_fd::Device,
    mem_props: vk::PhysicalDeviceMemoryProperties,
}

/// Drop-guard передаётся в wgpu-текстуру через `texture_from_raw`.
/// Освобождает VkImage + VkDeviceMemory когда wgpu уничтожает текстуру.
#[cfg(target_os = "linux")]
struct VkTextureDropGuard {
    device: ash::Device,
    image:  vk::Image,
    memory: vk::DeviceMemory,
}

#[cfg(target_os = "linux")]
impl Drop for VkTextureDropGuard {
    fn drop(&mut self) {
        unsafe {
            // Порядок важен: сначала memory, потом image (image не должен быть
            // bound к существующей памяти при уничтожении — хотя Vulkan
            // допускает и обратный порядок, строгий порядок безопаснее).
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

// SAFETY: raw Vulkan handles (vk::Image, vk::DeviceMemory) являются числами;
// ash::Device — тонкая обёртка над указателем, не имеет собственных данных.
#[cfg(target_os = "linux")]
unsafe impl Send for VkTextureDropGuard {}

#[cfg(target_os = "linux")]
unsafe impl Sync for VkTextureDropGuard {}

#[cfg(target_os = "linux")]
impl DmaBufAllocator {
    /// Инициализируем из raw Vulkan handles, извлечённых из wgpu HAL.
    ///
    /// Возвращает None если Vulkan не активен или расширения недоступны.
    unsafe fn new(
        device:    ash::Device,
        instance:  &ash::Instance,
        phys_dev:  vk::PhysicalDevice,
    ) -> Option<Self> {
        // Проверяем, что нужные расширения задекларированы в устройстве.
        let ext_mem = ash::khr::external_memory_fd::Device::new(instance, &device);
        let mem_props = instance.get_physical_device_memory_properties(phys_dev);
        Some(Self { device, ext_mem, mem_props })
    }

    /// Импортирует одну плоскость DMA-BUF как wgpu::Texture.
    ///
    /// - `fd`         — исходный fd (будет dup-нут внутри; оригинал не тронут)
    /// - `total_size` — полный размер DMA-BUF объекта (из AVDRMObjectDescriptor.size)
    /// - `bind_offset`— смещение в байтах до начала плоскости (y_offset / uv_offset)
    /// - `modifier`   — DRM format modifier (0 = LINEAR)
    /// - `vk_format`  — VK_FORMAT_R8_UNORM (Y) / VK_FORMAT_R8G8_UNORM (UV)
    /// - `wgpu_format`— соответствующий wgpu формат
    ///
    /// # Safety
    /// `fd` должен быть валидным DMA-BUF дескриптором.
    /// `wgpu_device` должен использовать Vulkan бэкенд.
    /// 
    #[cfg(target_os = "linux")]
    unsafe fn import_plane(
        &self,
        wgpu_device: &wgpu::Device,
        fd:          std::os::unix::io::RawFd,
        total_size:  usize,
        bind_offset: u64,
        row_pitch:   u64,
        modifier:    u64,
        width:       u32,
        height:      u32,
        vk_format:   vk::Format,
        wgpu_format: wgpu::TextureFormat,
    ) -> Option<wgpu::Texture> {
        // ── 1. Tiling ────────────────────────────────────────────────────────
        //
        // DRM_FORMAT_MOD_LINEAR = 0: линейный layout, VK_IMAGE_TILING_LINEAR.
        // Другие модификаторы (Intel Y-тайл, AFBC и т.д.) требуют
        // VK_EXT_image_drm_format_modifier — это расширение опционально.
        // Для простоты текущей реализации поддерживаем только LINEAR.
        // Нелинейные модификаторы возвращают None → fallback на CPU путь.

        const DRM_FORMAT_MOD_LINEAR:  u64 = 0;
        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

        // 1. Выбираем Tiling
        let is_linear = modifier == DRM_FORMAT_MOD_LINEAR || modifier == DRM_FORMAT_MOD_INVALID;
        let tiling = if is_linear {
            vk::ImageTiling::LINEAR
        } else {
            // Для тайловых форматов (Intel Tile4 и др.) используем расширение
            vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
        };

        // 2. Подготавливаем расширенную информацию о формате
        let plane_layouts = [vk::SubresourceLayout {
            offset:    0,         // Смещение будем указывать при бинде памяти
            size:      0,         // Вычисляется драйвером
            row_pitch,            // Наш y_pitch или uv_pitch
            array_pitch: 0,
            depth_pitch: 0,
        }];

        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);

        let mut ext_image_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        // Собираем цепочку pNext
        let mut image_info = vk::ImageCreateInfo::default()
            .push_next(&mut ext_image_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(tiling)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        if !is_linear {
            image_info = image_info.push_next(&mut modifier_info);
        }

        let vk_image = match self.device.create_image(&image_info, None) {
            Ok(img) => img,
            Err(e) => {
                log::error!("[DMA-BUF] vkCreateImage failed (mod {modifier:#x}): {e}");
                return None;
            }
        };

        // ── 3. Требования к памяти ───────────────────────────────────────────

        let mem_reqs = self.device.get_image_memory_requirements(vk_image);

        // Ищем тип памяти: совместимый с image + DEVICE_LOCAL.
        let mem_type = match find_memory_type(
            &self.mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) {
            Some(t) => t,
            None => {
                log::warn!("[DMA-BUF] нет подходящего memory type (DEVICE_LOCAL)");
                self.device.destroy_image(vk_image, None);
                return None;
            }
        };

        // ── 4. Проверяем необходимость dedicated allocation ──────────────────

        let mut dedicated_reqs = vk::MemoryDedicatedRequirements::default();
        let mut mem_reqs2 = vk::MemoryRequirements2::default().push_next(&mut dedicated_reqs);
        self.device.get_image_memory_requirements2(
            &vk::ImageMemoryRequirementsInfo2::default().image(vk_image),
            &mut mem_reqs2,
        );
        let needs_dedicated = dedicated_reqs.requires_dedicated_allocation != 0
            || dedicated_reqs.prefers_dedicated_allocation != 0;

        // ── 5. Импортируем DMA-BUF fd как VkDeviceMemory ─────────────────────
        //
        // `vkImportMemoryFdKHR` потребляет fd (ядро закрывает его при
        // уничтожении VkDeviceMemory). Поэтому мы dup-аем fd здесь, чтобы
        // исходный `frame.fd` (OwnedFd) остался валидным для второй плоскости.

        let dup_fd = libc::dup(fd);
        if dup_fd < 0 {
            log::warn!("[DMA-BUF] dup(fd) failed");
            self.device.destroy_image(vk_image, None);
            return None;
        }

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);

        // allocation_size = размер всего DMA-BUF объекта (не только плоскости!).
        // Vulkan ожидает полный размер импортируемого буфера.
        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .push_next(&mut import_info)
            .allocation_size(total_size as u64)
            .memory_type_index(mem_type);

        // Dedicated allocation struct живёт в том же scope.
        let mut dedicated_info;
        if needs_dedicated {
            dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(vk_image);
            // Нужно вставить перед import_info в цепочку pNext.
            // ash 0.38 поддерживает множественные push_next через .push_next().
            alloc_info = alloc_info.push_next(&mut dedicated_info);
        }

        let vk_memory = match self.device.allocate_memory(&alloc_info, None) {
            Ok(m) => m,
            Err(e) => {
                // dup_fd уже передан Vulkan-у и будет закрыт ядром при неудаче
                // (если ошибка после импорта), но если ошибка до — закрываем сами.
                // Безопаснее всего: при ошибке allocate_memory fd НЕ закрывается
                // автоматически, поэтому закрываем его явно.
                libc::close(dup_fd);
                log::warn!("[DMA-BUF] vkAllocateMemory failed: {e}");
                self.device.destroy_image(vk_image, None);
                return None;
            }
        };
        // С этого момента dup_fd принадлежит vk_memory (Vulkan закроет его при free).

        // ── 6. Привязываем память к image с нужным offset ────────────────────
        //
        // bind_offset — это смещение начала плоскости внутри DMA-BUF буфера.
        // Для Y-плоскости обычно 0, для UV — stride * height (или с выравниванием).
        // Требование: bind_offset должен быть кратен mem_reqs.alignment.

        let mem_reqs = unsafe { self.device.get_image_memory_requirements(vk_image) };
        let aligned_offset = align_up(bind_offset, mem_reqs.alignment);

        if let Err(e) = unsafe { self.device.bind_image_memory(vk_image, vk_memory, aligned_offset) } {
            log::error!("[DMA-BUF] vkBindImageMemory failed: {e}");
            unsafe { self.device.destroy_image(vk_image, None) };
            unsafe { self.device.free_memory(vk_memory, None) };
            return None;
        }

        // ── 7. Оборачиваем VkImage в wgpu::Texture ───────────────────────────
        //
        // wgpu::hal::vulkan::Device::texture_from_raw:
        //   - drop_guard=Some(...)  → wgpu НЕ вызывает vkDestroyImage;
        //     деструктор drop_guard отвечает за полную очистку.
        //   - drop_guard=None       → wgpu вызывает vkDestroyImage сам.
        //
        // Нам нужен Some(drop_guard) потому что мы сами управляем памятью.

        let drop_guard: Box<dyn std::any::Any + Send + Sync> = Box::new(VkTextureDropGuard {
            device: self.device.clone(),
            image:  vk_image,
            memory: vk_memory,
        });

        let hal_tex_desc = wgpu::hal::TextureDescriptor {
            label:         Some("DMA-BUF plane"),
            size:          wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count:  1,
            dimension:     wgpu::TextureDimension::D2,
            format:        wgpu_format,
            usage:         wgpu::TextureUses::RESOURCE,
            memory_flags:  wgpu::hal::MemoryFlags::empty(),
            view_formats:  vec![],
        };

        let hal_device = unsafe { wgpu_device.as_hal::<wgpu::hal::api::Vulkan>() }?;
        let hal_memory = wgpu::hal::vulkan::TextureMemory::External; // Unit-вариант
        
        let hal_texture = unsafe { 
            (&*hal_device).texture_from_raw(
                vk_image,
                &hal_tex_desc,
                Some(Box::new(move || { drop(drop_guard); }) as Box<dyn FnOnce() + Send + Sync>),
                hal_memory,
            ) 
        };

        let wgpu_tex_desc = wgpu::TextureDescriptor {
            label:           Some("DMA-BUF plane"),
            size:            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count:    1,
            dimension:       wgpu::TextureDimension::D2,
            format:          wgpu_format,
            usage:           wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats:    &[],
        };

        Some(unsafe {
            wgpu_device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(hal_texture, &wgpu_tex_desc)
        })
    }

    /// Импортирует NV12 DMA-BUF кадр как пару wgpu::Texture (Y + UV).
    ///
    /// Возвращает (y_tex, uv_tex) или None при любой ошибке.
    /// При None вызывающий должен откатиться на CPU-путь.
    
    #[cfg(target_os = "linux")]
    pub unsafe fn import_nv12(
        &self,
        wgpu_device: &wgpu::Device,
        frame:       &DmaBufFrame,
    ) -> Option<(wgpu::Texture, wgpu::Texture)> {
        use std::os::unix::io::AsRawFd;
        let raw_fd = frame.fd.as_raw_fd();

        // Y-плоскость: R8Unorm, полный размер
        let y_tex = unsafe { self.import_plane(
            wgpu_device,
            raw_fd,
            frame.total_size,
            frame.y_offset  as u64,
            frame.y_pitch  as u64,
            frame.modifier,
            frame.width,
            frame.height,
            vk::Format::R8_UNORM,
            wgpu::TextureFormat::R8Unorm,
        ) }?;

        // UV-плоскость: Rg8Unorm, половинный размер
        let uv_tex = unsafe { self.import_plane(
            wgpu_device,
            raw_fd,
            frame.total_size,
            frame.uv_offset as u64,
            frame.uv_pitch  as u64,
            frame.modifier,
            frame.width  / 2,
            frame.height / 2,
            vk::Format::R8G8_UNORM,
            wgpu::TextureFormat::Rg8Unorm,
        ) }?;

        Some((y_tex, uv_tex))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Вспомогательные функции
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    for i in 0..props.memory_type_count {
        let mt = &props.memory_types[i as usize];
        if (type_bits & (1 << i)) != 0 && mt.property_flags.contains(required) {
            return Some(i);
        }
    }
    None
}

#[inline]
fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 { return value; }
    (value + align - 1) & !(align - 1)
}

// ─────────────────────────────────────────────────────────────────────────────
// WGPU State
// ─────────────────────────────────────────────────────────────────────────────

struct WgpuState {
    surface:           wgpu::Surface<'static>,
    device:            wgpu::Device,
    queue:             wgpu::Queue,
    config:            wgpu::SurfaceConfiguration,
    render_pipeline:   wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler:           wgpu::Sampler,
    textures:          Option<(wgpu::Texture, wgpu::Texture, wgpu::BindGroup)>,

    // ── Zero-copy DMA-BUF ────────────────────────────────────────────────────

    /// None — Vulkan-расширения недоступны или бэкенд не Vulkan.
    #[cfg(target_os = "linux")]
    dmabuf: Option<DmaBufAllocator>,

    /// Удерживает wgpu-текстуры предыдущего DMA-BUF кадра до тех пор, пока
    /// GPU гарантированно завершит чтение (swap происходит на следующем кадре).
    /// Необходимо, потому что queue.submit() — неблокирующий вызов.
    #[cfg(target_os = "linux")]
    _prev_dmabuf_textures: Option<(wgpu::Texture, wgpu::Texture)>,
}

impl WgpuState {
    async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        // Требуем Vulkan-бэкенд явно — DMA-BUF import работает только через Vulkan.
        // Если Vulkan недоступен, wgpu автоматически выберет другой бэкенд,
        // а DmaBufAllocator останется None.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: Default::default()
        });

        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference:       wgpu::PowerPreference::HighPerformance,
                compatible_surface:     Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();

        // ── Извлекаем raw Vulkan handles для DMA-BUF allocator ───────────────
        //
        // adapter.as_hal   → ash::Instance + vk::PhysicalDevice + mem_props
        // device.as_hal    → ash::Device
        //
        // Все three нужны для vkImportMemoryFdKHR и vkAllocateMemory.

        #[cfg(target_os = "linux")]
        let dmabuf: Option<DmaBufAllocator> = unsafe {
            // Извлекаем device handle из device closure.
            let mut ash_device_opt: Option<ash::Device> = None;
            unsafe {
                if let Some(hal_dev) = device.as_hal::<wgpu::hal::api::Vulkan>() {
                    ash_device_opt = Some(hal_dev.raw_device().clone());
                }
            }

            // Извлекаем instance + phys_device из adapter closure.
            let mut adapter_data: Option<(ash::Instance, vk::PhysicalDevice)> = None;
            unsafe {
                if let Some(hal_adapter) = adapter.as_hal::<wgpu::hal::api::Vulkan>() {
                    let instance = hal_adapter.shared_instance().raw_instance().clone();
                    let phys = hal_adapter.raw_physical_device();
                    adapter_data = Some((instance, phys));
                }
            }

            match (ash_device_opt, adapter_data) {
                (Some(dev), Some((inst, phys))) => {
                    match DmaBufAllocator::new(dev, &inst, phys) {
                        Some(alloc) => {
                            log::info!("[Render] DMA-BUF Vulkan allocator: READY");
                            Some(alloc)
                        }
                        None => {
                            log::warn!("[Render] DMA-BUF allocator init failed — CPU-path only");
                            None
                        }
                    }
                }
                _ => {
                    log::warn!("[Render] wgpu backend is not Vulkan — DMA-BUF import unavailable");
                    None
                }
            }
        };

        // ── Шейдер, pipeline, bind_group_layout (без изменений) ─────────────

        let caps   = surface.get_capabilities(&adapter);
        let config = wgpu::SurfaceConfiguration {
            usage:                         wgpu::TextureUsages::RENDER_ATTACHMENT,
            format:                        caps.formats[0],
            width:                         size.width.max(1),
            height:                        size.height.max(1),
            present_mode:                  wgpu::PresentMode::Immediate,
            alpha_mode:                    wgpu::CompositeAlphaMode::Opaque,
            view_formats:                  vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label:  Some("YUV Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label:   Some("NV12 Bind Group Layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding:    0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type:    wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled:   false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding:    1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding:    2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type:    wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled:   false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding:    3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter:     wgpu::FilterMode::Linear,
            min_filter:     wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label:              Some("Render Pipeline Layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size:     0,
            });

        let render_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label:  Some("Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module:              &shader,
                    entry_point:         Some("vs_main"),
                    buffers:             &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module:              &shader,
                    entry_point:         Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format:     config.format,
                        blend:      Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology:           wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face:         wgpu::FrontFace::Ccw,
                    cull_mode:          None,
                    unclipped_depth:    false,
                    polygon_mode:       wgpu::PolygonMode::Fill,
                    conservative:       false,
                },
                depth_stencil: None,
                multisample:   wgpu::MultisampleState {
                    count:                     1,
                    mask:                      !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview_mask: None,
                cache:          None,
            });

        Self {
            surface,
            device,
            queue,
            config,
            render_pipeline,
            bind_group_layout,
            sampler,
            textures:              None,
            #[cfg(target_os = "linux")]
            dmabuf,
            #[cfg(target_os = "linux")]
            _prev_dmabuf_textures: None,
        }
    }

    // ── CPU-путь: загружаем NV12 плоскости через queue.write_texture ─────────

    fn update_textures_cpu(&mut self, frame: &YuvFrame) {
        let needs_recreate = match &self.textures {
            None => true,
            Some((t_y, _, _)) => {
                t_y.size().width != frame.width || t_y.size().height != frame.height
            }
        };

        if needs_recreate {
            log::info!("[Render] Creating CPU NV12 textures {}×{}", frame.width, frame.height);

            let t_y = self.device.create_texture(&wgpu::TextureDescriptor {
                label:           Some("Y Texture"),
                size:            wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count:    1,
                dimension:       wgpu::TextureDimension::D2,
                format:          wgpu::TextureFormat::R8Unorm,
                usage:           wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats:    &[],
            });

            let t_uv = self.device.create_texture(&wgpu::TextureDescriptor {
                label:           Some("UV Texture"),
                size:            wgpu::Extent3d { width: frame.width / 2, height: frame.height / 2, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count:    1,
                dimension:       wgpu::TextureDimension::D2,
                format:          wgpu::TextureFormat::Rg8Unorm,
                usage:           wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats:    &[],
            });

            let bg = self.make_bind_group(&t_y, &t_uv);
            self.textures = Some((t_y, t_uv, bg));
        }

        let (t_y, t_uv, _) = self.textures.as_ref().unwrap();

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: t_y, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &frame.y,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frame.y_stride), rows_per_image: None },
            wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 },
        );
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: t_uv, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &frame.uv,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frame.uv_stride), rows_per_image: None },
            wgpu::Extent3d { width: frame.width / 2, height: frame.height / 2, depth_or_array_layers: 1 },
        );
    }

    // ── Zero-copy путь: импортируем DMA-BUF fd как Vulkan текстуры ───────────
    #[cfg(target_os = "linux")]
    fn update_textures_dmabuf(&mut self, frame: &DmaBufFrame) -> bool {
        let alloc = match &self.dmabuf {
            Some(a) => a,
            None    => return false,   // Vulkan недоступен — вернёмся к CPU пути
        };

        let import_result = unsafe {
            alloc.import_nv12(&self.device, frame)
        };

        let (y_tex, uv_tex) = match import_result {
            Some(pair) => pair,
            None => return false,   // import упал — CPU fallback
        };

        // Создаём bind group из новых Vulkan-текстур.
        let bg = self.make_bind_group(&y_tex, &uv_tex);

        // Старые текстуры переезжают в _prev_dmabuf_textures.
        // Они будут удалены только при следующем кадре (после queue.submit).
        // Это даёт GPU время завершить предыдущий draw call до освобождения
        // VkDeviceMemory (VkTextureDropGuard).
        if let Some((t_y, t_uv, _)) = self.textures.take() {
            // Если предыдущие были тоже DMA-BUF — достаточно их просто дропнуть.
            // Если CPU — тоже нормально.
            self._prev_dmabuf_textures = Some((t_y, t_uv));
        }

        self.textures = Some((y_tex, uv_tex, bg));
        true
    }

    fn make_bind_group(&self, t_y: &wgpu::Texture, t_uv: &wgpu::Texture) -> wgpu::BindGroup {
        let v_y  = t_y .create_view(&Default::default());
        let v_uv = t_uv.create_view(&Default::default());

        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label:   Some("NV12 Bind Group"),
            layout:  &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&v_y)  },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&v_uv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    fn render(&mut self) {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t, // Можно рисовать, но лучше обновить конфиг
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return; 
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                // Здесь по-хорошему нужно пересоздать surface, 
                // но для начала попробуем просто переконфигурировать
                self.surface.configure(&self.device, &self.config);
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // Пропускаем кадр
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("WGPU Validation error on get_current_texture");
                return;
            }
        };

        // При swap кадров освобождаем _prev_dmabuf_textures ПОСЛЕ submit,
        // поэтому достаточно дропнуть их здесь — queue.submit из предыдущего
        // кадра к этому моменту уже «закоммичен» в драйвер.
        #[cfg(target_os = "linux")]
        {
        self._prev_dmabuf_textures = None;
        }

        let view    = surface_texture.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view:           &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load:  wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment:  None,
                occlusion_query_set:       None,
                timestamp_writes:          None,
                multiview_mask:            None,
            });

            if let Some((_, _, bg)) = &self.textures {
                pass.set_pipeline(&self.render_pipeline);
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..3, 0..1);
            }
        }

        self.queue.submit(std::iter::once(enc.finish()));
        surface_texture.present();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Winit Application Handler
// ─────────────────────────────────────────────────────────────────────────────

struct App {
    state:           Option<WgpuState>,
    window:          Option<Arc<Window>>,
    frame_rx:        mpsc::Receiver<DecodedFrame>,
    trace_tx:        watch::Sender<Option<(u64, FrameTrace)>>,
    frames_rendered: u64,
}

impl App {
    /// Дренируем канал, возвращаем самый свежий кадр.
    fn poll_latest_frame(&mut self) -> Option<DecodedFrame> {
        let mut latest = None;
        while let Ok(frame) = self.frame_rx.try_recv() {
            latest = Some(frame);
        }
        latest
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("StreamCapture Receiver")
                .with_inner_size(winit::dpi::LogicalSize::new(1280.0f64, 720.0f64));

            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            self.state  = Some(pollster::block_on(WgpuState::new(window.clone())));
            self.window = Some(window);
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::NewFrame => {
                if let Some(w) = &self.window {
                    w.request_redraw(); // Просто будим окно
                }
            }
        }
    }

    // fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    // }

    fn window_event(
        &mut self,
        event_loop:  &ActiveEventLoop,
        _id:         WindowId,
        event:       WindowEvent,
    ) {
        let state = match self.state.as_mut() {
            Some(s) => s,
            None    => return,
        };

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.config.width  = size.width.max(1);
                state.config.height = size.height.max(1);
                state.surface.configure(&state.device, &state.config);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(decoded) = self.poll_latest_frame() {
                    if let Some(state) = self.state.as_mut() {
                        
                        let (frame_id, frame_trace) = match &decoded {
                            DecodedFrame::Yuv(f) => (f.frame_id, f.trace),
                            #[cfg(target_os = "linux")]
                            DecodedFrame::DmaBuf(f) => (f.frame_id, f.trace),
                            #[cfg(not(target_os = "linux"))]
                            DecodedFrame::DmaBuf(f) => (f.frame_id, f.trace),
                        };

                        match decoded {
                            DecodedFrame::Yuv(frame) => state.update_textures_cpu(&frame),
                            #[cfg(target_os = "linux")]
                            DecodedFrame::DmaBuf(frame) => {
                                if !state.update_textures_dmabuf(&frame) { return; }
                            }
                            #[cfg(not(target_os = "linux"))]
                            DecodedFrame::DmaBuf(_frame) => {
                                log::warn!("[Render] DMA-BUF frame received on non-Linux build");
                                return;
                            }
                        }

                        let mut t = frame_trace;
                        if t.capture_us != 0 {
                            t.present_us = FrameTrace::now_us();
                            let _ = self.trace_tx.send(Some((frame_id, t)));
                        }

                        state.render();
                        if !self.frame_rx.is_empty() { if let Some(w) = &self.window { w.request_redraw() }; }
                    }
                }

                // Draw initial window
                if let Some(state) = self.state.as_mut() {
                    state.render();
                }

                // If queue is not empty, plan for a render
                if !self.frame_rx.is_empty() {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }   
}

// ─────────────────────────────────────────────────────────────────────────────
// Точка входа
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <ip:port>", args[0]);
        eprintln!("Example: {} 192.168.1.5:5000", args[0]);
        std::process::exit(1);
    }

    let addr: SocketAddr = args[1]
        .to_socket_addrs()?
        .next()
        .ok_or("Could not resolve address")?;

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Канал несёт DecodedFrame (CPU или DMA-BUF).
    // Capacity=3: при отставании рендера старые кадры выбрасываются → low-latency.
    let (tx, rx) = mpsc::channel::<DecodedFrame>(3);

    #[cfg(target_os = "macos")]
    let backend = MacosFfmpegBackend::new()?;

    #[cfg(not(target_os = "macos"))]
    let backend = DesktopFfmpegBackend::new()?;
    let backend = Arc::new(Mutex::new(backend));
    let backend_clone = backend.clone();
    let tx_clone      = tx.clone();

    let (trace_tx, trace_rx) = watch::channel::<Option<(u64, FrameTrace)>>(None);
    
    // Рендер-поток (главный поток — требование winit).
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event().build()?;

    #[cfg(not(target_os = "android"))]
    let proxy = Some(event_loop.create_proxy());
    #[cfg(target_os = "android")]
    let proxy = None;

    std::thread::Builder::new()
        .name("quic-receiver".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                run_quic_receiver(backend_clone, addr, Some(tx_clone), trace_rx, proxy).await;
            }));
        })?;


    let mut app = App {
        state:           None,
        window:          None,
        frame_rx:        rx,
        trace_tx:        trace_tx,
        frames_rendered: 0,
    };
    event_loop.run_app(&mut app)?;

    Ok(())
}

fn log_trace(frame_id: u64, t: &FrameTrace) {
    log::info!(
        "\n#{frame_id}: capture→encode={:.1}ms encode→serial={:.1}ms \
         serial→recv={:.1}ms recv→reassem={:.1}ms reassem→decode={:.1}ms \
         decode→present={:.1}ms  TOTAL={:.1}ms",
        FrameTrace::ms(t.capture_us,    t.encode_us),
        FrameTrace::ms(t.encode_us,     t.serialize_us),
        FrameTrace::ms(t.serialize_us,  t.receive_us),
        FrameTrace::ms(t.receive_us,    t.reassembled_us),
        FrameTrace::ms(t.reassembled_us, t.decode_us),
        FrameTrace::ms(t.decode_us,     t.present_us),
        FrameTrace::ms(t.capture_us,    t.present_us),
    );
}