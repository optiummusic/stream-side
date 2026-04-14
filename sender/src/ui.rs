
use eframe::egui;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub enum UserEvent {
    Quit,
}
pub struct App {
    bitrate: Arc<AtomicU64>,
    ui_bitrate_mbps: f32,
}

impl App {
    pub fn new(bitrate: Arc<AtomicU64>) -> Self {
        // Читаем начальное значение из атомика
        let current_mbps = bitrate.load(Ordering::Relaxed) as f32 / 1_000_000.0;
        Self {
            bitrate,
            ui_bitrate_mbps: current_mbps,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Streaming control panel");
            ui.add_space(10.0);

            ui.label("Bitrate settings:");
            // Слайдер напрямую меняет ui_bitrate_mbps
            let slider = ui.add(
                egui::Slider::new(&mut self.ui_bitrate_mbps, 0.01..=75.0)
                    .text("Мбит/с")
                    .suffix(" Mbps")
            );

            // Если слайдер сдвинули, обновляем общий атомик
            if slider.changed() {
                let new_bitrate = (self.ui_bitrate_mbps * 1_000_000.0) as u64;
                self.bitrate.store(new_bitrate, Ordering::Relaxed);
            }

            ui.add_space(20.0);
            ui.separator();
            
            if ui.button("Close window").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Можно оставить пустым, если логика в update
    }
}