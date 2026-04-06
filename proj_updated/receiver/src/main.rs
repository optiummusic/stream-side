// src/main.rs
//
// Десктопная точка входа (Linux / Windows).
// Компилируется ТОЛЬКО при `--features desktop`.
//
// Поток исполнения:
//   ┌─────────────────────────────────────────────────────┐
//   │  main()                                              │
//   │    │                                                 │
//   │    ├── thread::spawn → tokio RT → run_quic_receiver  │
//   │    │       (DesktopFfmpegBackend + mpsc::SyncSender) │
//   │    │                                                 │
//   │    └── EventLoop::run_app (winit, главный поток)     │
//   │            RedrawRequested → try_recv → upload YUV  │
//   │                           → wgpu render             │
//   └─────────────────────────────────────────────────────┘

use std::error::Error;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex};
use std::net::ToSocketAddrs;

use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

use stream_receiver::backend::{desktop::DesktopFfmpegBackend, VideoBackend, YuvFrame};
use stream_receiver::network::run_quic_receiver;

// ─────────────────────────────────────────────────────────────────────────────
// WGPU State (без изменений по сравнению с исходным кодом)
// ─────────────────────────────────────────────────────────────────────────────
struct WgpuState {
    surface:            wgpu::Surface<'static>,
    device:             wgpu::Device,
    queue:              wgpu::Queue,
    config:             wgpu::SurfaceConfiguration,
    render_pipeline:    wgpu::RenderPipeline,
    bind_group_layout:  wgpu::BindGroupLayout,
    textures:           Option<(wgpu::Texture, wgpu::Texture, wgpu::Texture, wgpu::BindGroup)>,
}

impl WgpuState {
    async fn new(window: Arc<Window>) -> Self {
        let size     = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface  = instance.create_surface(window.clone()).unwrap();

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

        let caps   = surface.get_capabilities(&adapter);
        let config = wgpu::SurfaceConfiguration {
            usage:                        wgpu::TextureUsages::RENDER_ATTACHMENT,
            format:                       caps.formats[0],
            width:                        size.width.max(1),
            height:                       size.height.max(1),
            present_mode:                 wgpu::PresentMode::Fifo,
            alpha_mode:                   wgpu::CompositeAlphaMode::Opaque,
            view_formats:                 vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label:  Some("YUV Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label:   Some("YUV Bind Group Layout"),
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
                        ty:    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
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
                        ty:    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding:    4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type:    wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled:   false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding:    5,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty:    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label:                Some("Render Pipeline Layout"),
                bind_group_layouts:   &[Some(&bind_group_layout)],
                immediate_size:       0,
            });

        let render_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label:  Some("Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module:               &shader,
                    entry_point:          Some("vs_main"),
                    buffers:              &[],
                    compilation_options:  Default::default(),
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
            textures: None,
        }
    }

    fn update_textures(&mut self, frame: &YuvFrame) {
        let needs_recreate = match &self.textures {
            None          => true,
            Some((ty, _, _, _)) => {
                ty.size().width != frame.width || ty.size().height != frame.height
            }
        };

        if needs_recreate {
            log::info!("Creating textures {}x{}", frame.width, frame.height);

            let create_tex = |label, w, h| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label:             Some(label),
                    size:              wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count:   1,
                    sample_count:      1,
                    dimension:         wgpu::TextureDimension::D2,
                    format:            wgpu::TextureFormat::R8Unorm,
                    usage:             wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats:      &[],
                })
            };

            let t_y = create_tex("Y", frame.width, frame.height);
            let t_u = create_tex("U", frame.width / 2, frame.height / 2);
            let t_v = create_tex("V", frame.width / 2, frame.height / 2);

            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter:     wgpu::FilterMode::Linear,
                min_filter:     wgpu::FilterMode::Linear,
                mipmap_filter:  wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            });

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.bind_group_layout,
                label:  Some("YUV Bind Group"),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&t_y.create_view(&Default::default())) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&t_u.create_view(&Default::default())) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&sampler) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&t_v.create_view(&Default::default())) },
                    wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(&sampler) },
                ],
            });

            self.textures = Some((t_y, t_u, t_v, bind_group));
        }

        if let Some((t_y, t_u, t_v, _)) = &self.textures {
            let write = |tex: &wgpu::Texture, data: &[u8], w, h, stride| {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture:    tex,
                        mip_level:  0,
                        origin:     wgpu::Origin3d::ZERO,
                        aspect:     wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset:          0,
                        bytes_per_row:   Some(stride),
                        rows_per_image:  Some(h),
                    },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                )
            };
            write(t_y, &frame.y, frame.width,     frame.height,     frame.y_stride);
            write(t_u, &frame.u, frame.width / 2, frame.height / 2, frame.u_stride);
            write(t_v, &frame.v, frame.width / 2, frame.height / 2, frame.v_stride);
        }
    }

    fn render(&mut self) {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)    => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                t
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };

        let view = surface_texture.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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

            if let Some((_, _, _, bind_group)) = &self.textures {
                pass.set_pipeline(&self.render_pipeline);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Winit Application Handler
// ─────────────────────────────────────────────────────────────────────────────

struct App {
    state:           Option<WgpuState>,
    window:          Option<Arc<Window>>,
    frame_rx:        mpsc::Receiver<YuvFrame>,
    frames_rendered: u64,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("StreamCapture Receiver")
                .with_inner_size(winit::dpi::LogicalSize::new(1280.0f64, 720.0f64));
            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            self.window = Some(window.clone());
            self.state  = Some(pollster::block_on(WgpuState::new(window)));
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state = match self.state.as_mut() { Some(s) => s, None => return };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size)  => {
                state.config.width  = size.width.max(1);
                state.config.height = size.height.max(1);
                state.surface.configure(&state.device, &state.config);
                if let Some(w) = &self.window { w.request_redraw(); }
            }
            WindowEvent::RedrawRequested => {
                // Берём только последний кадр (пропускаем накопившиеся)
                let mut latest = None;
                while let Ok(f) = self.frame_rx.try_recv() { latest = Some(f); }

                if let Some(frame) = latest {
                    state.update_textures(&frame);
                }

                self.frames_rendered += 1;
                if self.frames_rendered % 300 == 0 {
                    log::debug!("Rendered {} frames", self.frames_rendered);
                }

                state.render();
                if let Some(w) = &self.window { w.request_redraw(); }
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
    let input_addr = &args[1];
    let addr = input_addr
        .to_socket_addrs()
        .expect("Failed to resolve domain")
        .next() // Берем первый найденный IP
        .ok_or("Could not find any IP for this domain")?;

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    // Канал между сетевым потоком и рендер-потоком.
    // Размер 3: если рендер отстаёт, старые кадры выбрасываются (low-latency!).
    let (tx, rx) = mpsc::sync_channel::<YuvFrame>(3);

    // Инициализируем бекенд
    let backend = DesktopFfmpegBackend::new()?;
    let backend = Arc::new(Mutex::new(backend));

    let backend_clone = backend.clone();
    let tx_clone      = tx.clone();

    // Сетевой поток
    std::thread::Builder::new()
        .name("quic-receiver".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                // На десктопе передаём Some(tx) — YUV-кадры идут в рендер-поток
                run_quic_receiver(backend_clone, addr, Some(tx_clone)).await;
            }));
        })?;

    // Рендер-поток (главный поток, требование winit)
    let event_loop = EventLoop::new()?;
    let mut app = App {
        state:           None,
        window:          None,
        frame_rx:        rx,
        frames_rendered: 0,
    };
    event_loop.run_app(&mut app)?;

    Ok(())
}