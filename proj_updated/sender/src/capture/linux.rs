//! Linux screen-capture sender: XDG Desktop Portal + PipeWire + VAAPI HEVC.
//!
//! # Architecture
//!
//! ```text
//!   XDG Portal (ashpd)
//!       │  open_pipe_wire_remote()
//!       ▼
//!   PipeWire stream (SpA BGRA frames, ~60 fps)
//!       │  process callback (sync OS thread)
//!       ▼
//!   Encoder::encode(&[u8])          ← double-buffered, drop-if-full
//!       │  SyncSender<Vec<u8>>
//!       ▼
//!   Encoder worker thread
//!       │  VAAPI: BGRA→NV12→HW→hevc_vaapi
//!       │  mpsc::Sender<(Vec<u8>, bool)>
//!       ▼
//!   QuicServer serialiser task      ← downstream of VideoSender::run()
//! ```
//!
//! The PipeWire mainloop runs on a dedicated OS thread because `mainloop.run()`
//! is a blocking C call that cannot be `await`-ed.  A `oneshot` channel bridges
//! it back to the async world so that fatal errors propagate correctly.

use std::{
    os::{fd::FromRawFd, unix::io::IntoRawFd},
    sync::Arc,
    time::Duration,
};

use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode,
};
use pipewire as pw;
use pipewire::{
    context::ContextBox,
    main_loop::MainLoopBox,
    spa::pod::PropertyFlags,
};
use pw::properties::properties;
use tokio::sync::{mpsc, oneshot};

use super::SenderError;
use super::super::encode::Encoder;

// ─────────────────────────────────────────────────────────────────────────────
// Public type
// ─────────────────────────────────────────────────────────────────────────────

/// Linux screen-capture sender backed by XDG Portal, PipeWire, and VAAPI HEVC.
///
/// Construct with [`LinuxPipeWireSender::new`], then pass to [`crate::capture::VideoSender::run`].
pub struct LinuxPipeWireSender {
    width:  u32,
    height: u32,
}

impl LinuxPipeWireSender {
    /// Create a new sender that will capture and encode at `width × height`.
    ///
    /// Resolution should match the actual monitor / window dimensions; a
    /// mismatch causes the BGRA→NV12 scaler to stretch or crop silently.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VideoSender impl
// ─────────────────────────────────────────────────────────────────────────────

impl super::VideoSender for LinuxPipeWireSender {
    type Error = SenderError;

    async fn run(
        self,
        sink: mpsc::Sender<(Vec<u8>, bool)>,
    ) -> Result<(), SenderError> {
        // 1. Negotiate the XDG Portal screencast session.
        let (node_id, raw_fd) = run_portal()
            .await
            .map_err(|e| SenderError::CaptureInit(e.to_string()))?;

        // 2. The PipeWire mainloop is blocking — run it on a dedicated OS thread.
        //    A oneshot channel carries any fatal error back to the async world.
        let (err_tx, err_rx) = oneshot::channel::<SenderError>();
        let width  = self.width;
        let height = self.height;

        // Propagate the current Tokio runtime handle so the Encoder worker thread
        // (inside Encoder::new) can use tokio primitives without a new runtime.
        let rt_handle = tokio::runtime::Handle::current();

        std::thread::Builder::new()
            .name("pipewire-capture".into())
            .spawn(move || {
                let _guard = rt_handle.enter();
                if let Err(e) = run_pipewire(node_id, raw_fd, width, height, sink) {
                    let _ = err_tx.send(e);
                }
            })
            .map_err(|e| SenderError::CaptureInit(format!("thread spawn: {e}")))?;

        // 3. Wait for the PipeWire thread to signal a fatal error.
        //    If the future is dropped (Ctrl-C), the thread is left to die on its
        //    own when the process exits — PipeWire has no remote-stop API.
        match err_rx.await {
            Ok(e) => Err(e),
            // Sender dropped without sending ⟹ thread finished cleanly.
            Err(_) => Ok(()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Portal negotiation
// ─────────────────────────────────────────────────────────────────────────────

async fn run_portal() -> ashpd::Result<(u32, i32)> {
    let proxy   = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;

    proxy.select_sources(
        &session,
        SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(SourceType::Monitor | SourceType::Window)
            .set_multiple(false)
            .set_persist_mode(PersistMode::DoNot),
    )
    .await?;

    let response = proxy.start(&session, None, Default::default()).await?.response()?;
    let node_id  = response.streams()[0].pipe_wire_node_id();
    let fd       = proxy.open_pipe_wire_remote(&session, Default::default()).await?;

    Ok((node_id, fd.into_raw_fd()))
}

// ─────────────────────────────────────────────────────────────────────────────
// PipeWire capture loop (blocking OS thread)
// ─────────────────────────────────────────────────────────────────────────────

fn run_pipewire(
    node_id: u32,
    raw_fd:  i32,
    width:   u32,
    height:  u32,
    sink:    mpsc::Sender<(Vec<u8>, bool)>,
) -> Result<(), SenderError> {
    pw::init();

    let mainloop = MainLoopBox::new(None)
        .map_err(|e| SenderError::CaptureInit(format!("pw mainloop: {e}")))?;
    let context  = ContextBox::new(&mainloop.loop_(), None)
        .map_err(|e| SenderError::CaptureInit(format!("pw context: {e}")))?;

    let core = context
        .connect_fd(
            unsafe { std::os::unix::io::OwnedFd::from_raw_fd(raw_fd) },
            None,
        )
        .map_err(|e| SenderError::CaptureInit(format!("pw connect_fd: {e}")))?;

    let stream = pw::stream::StreamBox::new(&core, "capture", properties! {
        *pw::keys::MEDIA_TYPE     => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Screen",
    })
    .map_err(|e| SenderError::CaptureInit(format!("pw stream: {e}")))?;

    // Create the encoder once; it spawns its own worker thread internally.
    let mut encoder: Option<Encoder> = None;
    let mut fps_counter = 0u32;
    let mut fps_tick    = std::time::Instant::now();

    let _listener = stream
        .add_local_listener::<()>()
        .process(move |stream, _| {
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() { return; }

            unsafe {
                crate::encode::process_frame_from_pw_buffer(raw, |src| {
                    let enc = encoder.get_or_insert_with(|| {
                        Encoder::new(width, height, sink.clone())
                    });
                    enc.encode(src);
                    fps_counter += 1;
                });

                if fps_tick.elapsed() >= Duration::from_secs(1) {
                    log::debug!("[PipeWire] FPS: {fps_counter}");
                    fps_counter = 0;
                    fps_tick    = std::time::Instant::now();
                }

                stream.queue_raw_buffer(raw);
            }
        })
        .register()
        .map_err(|e| SenderError::CaptureInit(format!("pw listener: {e}")))?;

    let spa_params = build_spa_video_params();
    let mut params = [pw::spa::pod::Pod::from_bytes(&spa_params)
        .ok_or_else(|| SenderError::CaptureInit("invalid SPA params".into()))?];

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| SenderError::CaptureInit(format!("pw stream connect: {e}")))?;

    log::info!("[PipeWire] Capture stream started (node_id={node_id}).");
    mainloop.run();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SPA video format pod
// ─────────────────────────────────────────────────────────────────────────────

fn build_spa_video_params() -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::sys::*;

    let value = pw::spa::pod::Value::Object(pw::spa::pod::Object {
        type_: SPA_TYPE_OBJECT_Format,
        id:    SPA_PARAM_EnumFormat,
        properties: vec![
            pw::spa::pod::Property {
                key:   SPA_FORMAT_mediaType,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_TYPE_video)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_mediaSubtype,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_SUBTYPE_raw)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_VIDEO_format,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_VIDEO_FORMAT_BGRA)),
            },
        ],
    });

    let mut bytes = Vec::new();
    PodSerializer::serialize(&mut std::io::Cursor::new(&mut bytes), &value).unwrap();
    bytes
}
