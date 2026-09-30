use eframe::egui;
use mapforge_core::{
    load_project, save_project_atomic, sha256_file, Asset, AssetKind, Command, Envelope, Layer,
    PlayerState, ShowProject, PROTOCOL_VERSION,
};
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    time::Duration,
};
use uuid::Uuid;

struct ProducerApp {
    project: ShowProject,
    project_path: Option<PathBuf>,
    selected_layer: Option<Uuid>,
    player_address: String,
    player_state: Option<PlayerState>,
    status: String,
}

impl Default for ProducerApp {
    fn default() -> Self {
        Self {
            project: ShowProject::default(),
            project_path: None,
            selected_layer: None,
            player_address: "127.0.0.1:4777".into(),
            player_state: None,
            status: "New project".into(),
        }
    }
}

impl ProducerApp {
    fn send(&mut self, command: Command) {
        let address: SocketAddr = match self.player_address.parse() {
            Ok(v) => v,
            Err(_) => {
                self.status = "Invalid Player address".into();
                return;
            }
        };
        match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
            Ok(mut stream) => {
                let envelope = Envelope {
                    protocol_version: PROTOCOL_VERSION,
                    command_id: Uuid::new_v4(),
                    command,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                if writeln!(stream, "{}", serde_json::to_string(&envelope).unwrap()).is_err() {
                    self.status = "Could not send command".into();
                    return;
                }
                let mut response = String::new();
                if BufReader::new(stream).read_line(&mut response).is_ok() {
                    match serde_json::from_str::<PlayerState>(&response) {
                        Ok(state) => {
                            self.status = state.message.clone();
                            self.player_state = Some(state);
                        }
                        Err(_) => self.status = "Player returned an invalid response".into(),
                    }
                }
            }
            Err(e) => {
                self.player_state = None;
                self.status = format!("Player offline: {e}");
            }
        }
    }

    fn sync_project(&mut self) {
        self.send(Command::LoadProject {
            project: self.project.clone(),
        });
    }

    fn save(&mut self, choose: bool) {
        if choose || self.project_path.is_none() {
            self.project_path = rfd::FileDialog::new()
                .set_file_name("show.mapforge.json")
                .add_filter("MapForge project", &["json"])
                .save_file();
        }
        if let Some(path) = &self.project_path {
            match save_project_atomic(&self.project, path) {
                Ok(()) => self.status = format!("Saved {}", path.display()),
                Err(e) => self.status = format!("Save failed: {e}"),
            }
        }
    }

    fn open(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("MapForge project", &["json"])
            .pick_file()
        {
            match load_project(&path) {
                Ok(project) => {
                    self.project = project;
                    self.project_path = Some(path);
                    self.selected_layer = None;
                    self.status = "Project loaded".into();
                }
                Err(e) => self.status = format!("Open failed: {e}"),
            }
        }
    }

    fn import_asset(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Media",
                &["png", "jpg", "jpeg", "webp", "mp4", "mov", "mkv", "avi"],
            )
            .pick_file()
        else {
            return;
        };
        self.status = "Reading media metadata and checksum…".into();
        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = if ["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
            AssetKind::Image
        } else if ["mp4", "mov", "mkv", "avi"].contains(&ext.as_str()) {
            AssetKind::Video
        } else {
            AssetKind::Unknown
        };
        let mut width = None;
        let mut height = None;
        let mut duration_seconds = None;
        if kind == AssetKind::Image {
            if let Ok((w, h)) = image::image_dimensions(&path) {
                width = Some(w);
                height = Some(h);
            }
        }
        if kind == AssetKind::Video {
            (width, height, duration_seconds) = probe_video(&path);
        }
        match sha256_file(&path) {
            Ok(checksum_sha256) => {
                let id = Uuid::new_v4();
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                self.project.assets.push(Asset {
                    id,
                    path: path.to_string_lossy().to_string(),
                    name: name.clone(),
                    kind,
                    checksum_sha256,
                    width,
                    height,
                    duration_seconds,
                });
                let outputs = self.project.outputs.iter().map(|o| o.id).collect();
                let w = width.unwrap_or(800) as f32;
                let h = height.unwrap_or(450) as f32;
                let scale = (self.project.stage.width * 0.6 / w)
                    .min(self.project.stage.height * 0.6 / h)
                    .min(1.0);
                let layer = Layer {
                    id: Uuid::new_v4(),
                    asset_id: id,
                    name,
                    x: 200.0,
                    y: 150.0,
                    width: w * scale,
                    height: h * scale,
                    opacity: 1.0,
                    output_ids: outputs,
                };
                self.selected_layer = Some(layer.id);
                self.project.scenes[0].layers.push(layer);
                self.status = "Media imported and layer created".into();
            }
            Err(e) => self.status = format!("Import failed: {e}"),
        }
    }

    fn stage_ui(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let ratio = self.project.stage.width / self.project.stage.height;
        let size = egui::vec2(available.x, (available.x / ratio).min(available.y))
            .max(egui::vec2(200.0, 120.0));
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 2.0, egui::Color32::from_rgb(16, 20, 31));
        let sx = rect.width() / self.project.stage.width;
        let sy = rect.height() / self.project.stage.height;
        for (i, output) in self.project.outputs.iter().enumerate() {
            let r = egui::Rect::from_min_size(
                egui::pos2(
                    rect.left() + output.stage_x * sx,
                    rect.top() + output.stage_y * sy,
                ),
                egui::vec2(output.stage_width * sx, output.stage_height * sy),
            );
            ui.painter().rect_stroke(
                r,
                0.0,
                egui::Stroke::new(
                    2.0_f32,
                    if i == 0 {
                        egui::Color32::from_rgb(38, 132, 255)
                    } else {
                        egui::Color32::from_rgb(187, 79, 255)
                    },
                ),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                r.left_top() + egui::vec2(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                &output.name,
                egui::FontId::proportional(13.0),
                egui::Color32::GRAY,
            );
        }
        if let Some(scene) = self.project.scenes.first_mut() {
            for (i, layer) in scene.layers.iter_mut().enumerate() {
                let layer_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + layer.x * sx, rect.top() + layer.y * sy),
                    egui::vec2(layer.width * sx, layer.height * sy),
                );
                let response = ui.interact(
                    layer_rect,
                    egui::Id::new(layer.id),
                    egui::Sense::click_and_drag(),
                );
                if response.clicked() {
                    self.selected_layer = Some(layer.id);
                }
                if response.dragged() {
                    let d = response.drag_delta();
                    layer.x = (layer.x + d.x / sx).clamp(-layer.width, self.project.stage.width);
                    layer.y = (layer.y + d.y / sy).clamp(-layer.height, self.project.stage.height);
                }
                let color = [
                    egui::Color32::from_rgb(38, 132, 255),
                    egui::Color32::from_rgb(187, 79, 255),
                    egui::Color32::from_rgb(30, 190, 130),
                ][i % 3]
                    .gamma_multiply(layer.opacity);
                ui.painter().rect_filled(layer_rect, 3.0, color);
                ui.painter().rect_stroke(
                    layer_rect,
                    3.0,
                    egui::Stroke::new(
                        if self.selected_layer == Some(layer.id) {
                            3.0_f32
                        } else {
                            1.0_f32
                        },
                        egui::Color32::WHITE,
                    ),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    layer_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &layer.name,
                    egui::FontId::proportional(14.0),
                    egui::Color32::WHITE,
                );
            }
        }
    }
}

impl eframe::App for ProducerApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("MapForge Producer");
                if ui.button("New").clicked() {
                    *self = Self::default();
                }
                if ui.button("Open").clicked() {
                    self.open();
                }
                if ui.button("Save").clicked() {
                    self.save(false);
                }
                if ui.button("Save As").clicked() {
                    self.save(true);
                }
                if ui.button("Import Media").clicked() {
                    self.import_asset();
                }
            });
        });
        egui::SidePanel::left("layers")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("Scene 1 · Layers");
                for layer in &self.project.scenes[0].layers {
                    if ui
                        .selectable_label(self.selected_layer == Some(layer.id), &layer.name)
                        .clicked()
                    {
                        self.selected_layer = Some(layer.id);
                    }
                }
                ui.separator();
                ui.heading("Player");
                ui.text_edit_singleline(&mut self.player_address);
                if ui.button("Connect / Sync Project").clicked() {
                    self.sync_project();
                }
                let scene_id = self.project.scenes[0].id;
                ui.horizontal(|ui| {
                    if ui.button("Prepare").clicked() {
                        self.send(Command::Prepare { scene_id });
                    }
                    if ui.button("▶ Play").clicked() {
                        self.send(Command::Play);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Pause").clicked() {
                        self.send(Command::Pause);
                    }
                    if ui.button("Stop").clicked() {
                        self.send(Command::Stop);
                    }
                });
                if ui.button("Blackout").clicked() {
                    self.send(Command::SetBlackout { value: true });
                }
                if ui.button("Restore").clicked() {
                    self.send(Command::SetBlackout { value: false });
                }
                if let Some(s) = &self.player_state {
                    ui.label(format!(
                        "{:?} · volume {}%",
                        s.transport,
                        (s.volume * 100.0) as u32
                    ));
                }
            });
        egui::SidePanel::right("properties")
            .resizable(true)
            .default_width(250.0)
            .show(ctx, |ui| {
                ui.heading("Layer properties");
                if let Some(id) = self.selected_layer {
                    if let Some(layer) = self.project.scenes[0]
                        .layers
                        .iter_mut()
                        .find(|l| l.id == id)
                    {
                        egui::Grid::new("props").show(ui, |ui| {
                            ui.label("X");
                            ui.add(egui::DragValue::new(&mut layer.x));
                            ui.end_row();
                            ui.label("Y");
                            ui.add(egui::DragValue::new(&mut layer.y));
                            ui.end_row();
                            ui.label("Width");
                            ui.add(egui::DragValue::new(&mut layer.width).range(1.0..=32768.0));
                            ui.end_row();
                            ui.label("Height");
                            ui.add(egui::DragValue::new(&mut layer.height).range(1.0..=32768.0));
                            ui.end_row();
                        });
                        ui.add(egui::Slider::new(&mut layer.opacity, 0.0..=1.0).text("Opacity"));
                        ui.separator();
                        ui.label("Route to outputs");
                        for output in &self.project.outputs {
                            let mut enabled = layer.output_ids.contains(&output.id);
                            if ui.checkbox(&mut enabled, &output.name).changed() {
                                if enabled && !layer.output_ids.contains(&output.id) {
                                    layer.output_ids.push(output.id);
                                } else if !enabled {
                                    layer.output_ids.retain(|id| id != &output.id);
                                }
                            }
                        }
                    }
                } else {
                    ui.label("Select or import a layer.");
                }
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!(
                "{} · {} × {}",
                self.project.name, self.project.stage.width, self.project.stage.height
            ));
            self.stage_ui(ui);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });
    }
}

fn probe_video(path: &Path) -> (Option<u32>, Option<u32>, Option<f64>) {
    let output = ProcessCommand::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return (None, None, None);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return (None, None, None);
    };
    let stream = value.get("streams").and_then(|v| v.get(0));
    let width = stream
        .and_then(|v| v.get("width"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let height = stream
        .and_then(|v| v.get("height"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let duration = value
        .get("format")
        .and_then(|v| v.get("duration"))
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse().ok());
    (width, height, duration)
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MapForge Producer",
        options,
        Box::new(|_| Ok(Box::new(ProducerApp::default()))),
    )
}
