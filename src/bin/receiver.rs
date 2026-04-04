use quinn::{Endpoint, ServerConfig};
use quinn::crypto::rustls::QuicServerConfig;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};

use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling;
use ffmpeg_next::util::frame::video::Video;

// ============================================================================
// 1. СТРУКТУРЫ ДАННЫХ
// ============================================================================

pub struct YuvFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
}

// ============================================================================
// 2. ГРАФИЧЕСКИЙ ДВИЖОК (WGPU)
// ============================================================================

struct WgpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    render_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    textures: Option<(wgpu::Texture, wgpu::Texture, wgpu::Texture, wgpu::BindGroup)>,
}

impl WgpuState {
    async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }).await.unwrap();

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();

        let caps = surface.get_capabilities(&adapter);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("YUV Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("YUV Bind Group Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None },
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
                wgpu::BindGroupLayoutEntry { binding: 2, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None },
                wgpu::BindGroupLayoutEntry { binding: 3, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
                wgpu::BindGroupLayoutEntry { binding: 4, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None },
                wgpu::BindGroupLayoutEntry { binding: 5, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Render Pipeline Layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState { count: 1, mask: !0, alpha_to_coverage_enabled: false },
            multiview_mask: None,
            cache: None,
        });

        Self { surface, device, queue, config, render_pipeline, bind_group_layout, textures: None }
    }

    fn update_textures(&mut self, frame: &YuvFrame) {
        // Пересоздаём текстуры при первом кадре или смене разрешения
        let needs_recreate = match &self.textures {
            None => true,
            Some((t_y, _, _, _)) => {
                t_y.size().width != frame.width || t_y.size().height != frame.height
            }
        };

        if needs_recreate {
            eprintln!("🖼  Создаём текстуры {}x{}", frame.width, frame.height);

            let create_tex = |label, w, h| self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let t_y = create_tex("Y", frame.width, frame.height);
            let t_u = create_tex("U", frame.width / 2, frame.height / 2);
            let t_v = create_tex("V", frame.width / 2, frame.height / 2);

            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            });

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.bind_group_layout,
                label: Some("YUV Bind Group"),
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
            let write_tex = |tex: &wgpu::Texture, data: &[u8], width, height, stride| {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride),
                        rows_per_image: Some(height),
                    },
                    wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                );
            };
            write_tex(t_y, &frame.y, frame.width, frame.height, frame.y_stride);
            write_tex(t_u, &frame.u, frame.width / 2, frame.height / 2, frame.u_stride);
            write_tex(t_v, &frame.v, frame.width / 2, frame.height / 2, frame.v_stride);
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
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => return,
        };

        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });

            if let Some((_, _, _, bind_group)) = &self.textures {
                render_pass.set_pipeline(&self.render_pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.draw(0..3, 0..1);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
    }
}

// ============================================================================
// 3. WINIT EVENT LOOP
// ============================================================================

struct App {
    state: Option<WgpuState>,
    window: Option<Arc<Window>>,
    frame_rx: mpsc::Receiver<YuvFrame>,
    frames_rendered: u64,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attributes = Window::default_attributes()
                .with_title("StreamCapture WGPU Receiver")
                .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));

            let window = Arc::new(event_loop.create_window(attributes).unwrap());
            self.window = Some(window.clone());
            self.state = Some(pollster::block_on(WgpuState::new(window)));
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let state = match self.state.as_mut() {
            Some(s) => s,
            None => return,
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(physical_size) => {
                state.config.width = physical_size.width.max(1);
                state.config.height = physical_size.height.max(1);
                state.surface.configure(&state.device, &state.config);
                if let Some(window) = &self.window { window.request_redraw(); }
            }
            WindowEvent::RedrawRequested => {
                let mut video_frames_this_tick = 0u32;
                let mut latest_frame = None;
                while let Ok(frame) = self.frame_rx.try_recv() {
                    video_frames_this_tick += 1;
                    latest_frame = Some(frame);
                }

                if let Some(frame) = latest_frame {
                    state.update_textures(&frame);
                }

                self.frames_rendered += 1;
                if self.frames_rendered % 300 == 0 {
                    eprintln!(
                        "🎬 render#{} | video_this_tick={} | has_texture={}",
                        self.frames_rendered, video_frames_this_tick, state.textures.is_some()
                    );
                }

                state.render();
                if let Some(window) = &self.window { window.request_redraw(); }
            }
            _ => {}
        }
    }
}

// ============================================================================
// 4. MAIN & NETWORKING
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (tx, rx) = mpsc::sync_channel::<YuvFrame>(3);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let local = tokio::task::LocalSet::new();

        rt.block_on(local.run_until(async move {
            run_quic_server(tx).await;
        }));
    });

    let event_loop = EventLoop::new().unwrap();
    let mut app = App {
        state: None,
        window: None,
        frame_rx: rx,
        frames_rendered: 0,
    };
    event_loop.run_app(&mut app)?;

    Ok(())
}

async fn run_quic_server(tx: mpsc::SyncSender<YuvFrame>) {
    ffmpeg_next::init().unwrap();
    rustls::crypto::ring::default_provider().install_default().ok();

    let addr: SocketAddr = "0.0.0.0:4433".parse().unwrap();
    let (cert, key) = generate_self_signed_cert().unwrap();

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key).unwrap();
    tls_config.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicServerConfig::try_from(tls_config).unwrap();
    let server_config = ServerConfig::with_crypto(Arc::new(quic_crypto));
    let endpoint = Endpoint::server(server_config, addr).unwrap();

    eprintln!("🚀 Ресивер запущен на {}", addr);

    while let Some(conn) = endpoint.accept().await {
        let connection = conn.await.unwrap();
        eprintln!("✅ Соединение установлено: {}", connection.remote_address());

        let tx_clone = tx.clone();
        tokio::task::spawn_local(async move {
            handle_connection(connection, tx_clone).await;
        });
    }
}

async fn handle_connection(connection: quinn::Connection, tx: mpsc::SyncSender<YuvFrame>) {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC)
        .expect("HEVC не найден");
    let decoder_ctx = ffmpeg_next::codec::context::Context::new();
    let mut decoder = decoder_ctx.decoder().open_as(codec).unwrap().video().unwrap();

    // Конвертер decoded_format → YUV420P.
    // VAAPI кодирует из NV12; software-декодер может отдавать как YUV420P,
    // так и NV12 — шейдер ждёт три раздельных плоскости, поэтому нормализуем.
    let mut scaler: Option<scaling::Context> = None;
    let mut last_fmt = Pixel::None;

    let mut packets_in: u64 = 0;
    let mut frames_out: u64 = 0;

    while let Ok(mut stream) = connection.accept_uni().await {
        eprintln!("📥 Новый QUIC-поток");
        let mut len_buf = [0u8; 4];

        loop {
            if stream.read_exact(&mut len_buf).await.is_err() {
                eprintln!("📭 Поток закрыт");
                break;
            }

            let len = u32::from_le_bytes(len_buf) as usize;
            if len == 0 || len > 10_000_000 {
                // Скорее всего сломалось QUIC-обрамление на стороне отправителя
                eprintln!("⚠️  Некорректная длина пакета {} — проверь QuicSender::send() на наличие 4-байтового префикса длины", len);
                break;
            }

            let mut frame_buf = vec![0u8; len];
            if stream.read_exact(&mut frame_buf).await.is_err() {
                eprintln!("📭 Поток закрыт при чтении тела");
                break;
            }

            packets_in += 1;
            if packets_in <= 5 || packets_in % 200 == 0 {
                eprintln!("📦 Пакет #{}: {} байт", packets_in, len);
            }

            let mut packet = ffmpeg_next::Packet::new(len);
            if let Some(dst) = packet.data_mut() {
                dst.copy_from_slice(&frame_buf);
            }

            if let Err(e) = decoder.send_packet(&packet) {
                eprintln!("❌ send_packet #{}: {:?}", packets_in, e);
                continue;
            }

            let mut raw = Video::empty();
            loop {
                match decoder.receive_frame(&mut raw) {
                    Ok(()) => {}
                    Err(_) => break,
                }

                frames_out += 1;
                let fmt = raw.format();
                let w = raw.width();
                let h = raw.height();

                if frames_out <= 3 {
                    eprintln!("🎞  Кадр #{}: {}x{} fmt={:?}", frames_out, w, h, fmt);
                }

                // Пересоздаём scaler если формат изменился
                if fmt != last_fmt {
                    eprintln!("🔄 Формат декодера: {:?} → создаём swscale в YUV420P", fmt);
                    last_fmt = fmt;
                    scaler = if fmt == Pixel::YUV420P {
                        None // не нужен
                    } else {
                        Some(
                            scaling::Context::get(fmt, w, h, Pixel::YUV420P, w, h, scaling::Flags::BILINEAR)
                                .expect("swscale init failed")
                        )
                    };
                }

                // Получаем YUV420P-кадр (либо оригинал, либо сконвертированный)
                let yuv420 = if let Some(ref mut sc) = scaler {
                    let mut converted = Video::new(Pixel::YUV420P, w, h);
                    if sc.run(&raw, &mut converted).is_err() { continue; }
                    converted
                } else {
                    // Уже YUV420P — клонируем данные через временный буфер
                    // (raw не реализует Clone, поэтому копируем вручную)
                    let mut copy = Video::new(Pixel::YUV420P, w, h);
                    let y_stride = raw.stride(0);
                    let u_stride = raw.stride(1);
                    let v_stride = raw.stride(2);
                    copy.data_mut(0)[..y_stride * h as usize]
                        .copy_from_slice(&raw.data(0)[..y_stride * h as usize]);
                    copy.data_mut(1)[..u_stride * (h / 2) as usize]
                        .copy_from_slice(&raw.data(1)[..u_stride * (h / 2) as usize]);
                    copy.data_mut(2)[..v_stride * (h / 2) as usize]
                        .copy_from_slice(&raw.data(2)[..v_stride * (h / 2) as usize]);
                    copy
                };

                let y_stride = yuv420.stride(0) as u32;
                let u_stride = yuv420.stride(1) as u32;
                let v_stride = yuv420.stride(2) as u32;

                let y = yuv420.data(0)[..(y_stride * h) as usize].to_vec();
                let u = yuv420.data(1)[..(u_stride * h / 2) as usize].to_vec();
                let v = yuv420.data(2)[..(v_stride * h / 2) as usize].to_vec();

                let frame = YuvFrame { width: w, height: h, y, u, v, y_stride, u_stride, v_stride };

                match tx.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(_)) => {} // рендер не успевает — ок
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        eprintln!("💀 Рендер-канал разорван");
                        return;
                    }
                }
            }
        }
    }
}

fn generate_self_signed_cert() -> Result<(rustls::pki_types::CertificateDer<'static>, rustls::pki_types::PrivateKeyDer<'static>), Box<dyn Error>> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    Ok((cert_der, key))
}