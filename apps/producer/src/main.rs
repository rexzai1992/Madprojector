// Release builds on Windows open no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod history;
mod network;
mod timeline;

use eframe::egui::{self, Color32, RichText};
use mapforge_core::{
    load_project, save_project_atomic, scene_hotkey, sha256_file, tool_command, Asset, AssetKind,
    Command, Cue, DisplayInfo, EdgeBlend, EndAction, Layer, LoopRegion, OutputColor, PlayerRole,
    PlayerState, ProjectorOutput, Scene, SceneButton, ShowProject, TestPattern, Transport,
};
use network::{unix_time_ms, PlayerLink};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const ACCENT: Color32 = Color32::from_rgb(66, 126, 255);
const LIVE: Color32 = Color32::from_rgb(31, 157, 98);
const DANGER: Color32 = Color32::from_rgb(212, 61, 81);
const WARN: Color32 = Color32::from_rgb(240, 170, 40);
const MUTED: Color32 = Color32::from_gray(140);
const LOOP_COLOR: Color32 = Color32::from_rgb(79, 209, 197);
const SCHEDULE_LEAD_MS: u64 = 1_500;
const PANEL: Color32 = Color32::from_rgb(20, 23, 31);
const CANVAS: Color32 = Color32::from_rgb(12, 14, 20);
const STAGE: Color32 = Color32::from_rgb(6, 8, 12);
const OUTPUT_COLORS: [Color32; 6] = [
    Color32::from_rgb(38, 132, 255),
    Color32::from_rgb(187, 79, 255),
    Color32::from_rgb(30, 190, 130),
    Color32::from_rgb(255, 160, 40),
    Color32::from_rgb(255, 90, 120),
    Color32::from_rgb(80, 210, 230),
];
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];
const VIDEO_EXTENSIONS: [&str; 6] = ["mp4", "mov", "mkv", "avi", "webm", "m4v"];
const AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "wav", "aac", "m4a", "flac", "ogg", "aiff"];
const STAGE_PRESETS: [(&str, f32, f32); 6] = [
    ("10400 × 1080 (wide show canvas)", 10400.0, 1080.0),
    ("1920 × 1080 (HD)", 1920.0, 1080.0),
    ("3840 × 1080 (2 × HD wide)", 3840.0, 1080.0),
    ("5760 × 1080 (3 × HD wide)", 5760.0, 1080.0),
    ("3840 × 2160 (4K)", 3840.0, 2160.0),
    ("1080 × 1920 (portrait)", 1080.0, 1920.0),
];
const RESOLUTION_PRESETS: [(u32, u32); 6] = [
    (1920, 1080),
    (1920, 1200),
    (1280, 800),
    (1400, 1050),
    (2560, 1600),
    (3840, 2160),
];
/// Keys the Producer itself uses, which scenes and cues cannot take.
const RESERVED_KEYS: [&str; 8] = [
    "Space",
    "Escape",
    "B",
    "M",
    "L",
    "Enter",
    "Delete",
    "Backspace",
];

// ---------------------------------------------------------------------------
// Thumbnails are decoded off the UI thread; videos use one FFmpeg frame.

struct Thumbnails {
    tx: Sender<(Uuid, Option<egui::ColorImage>)>,
    rx: Receiver<(Uuid, Option<egui::ColorImage>)>,
    textures: HashMap<Uuid, egui::TextureHandle>,
    requested: HashSet<Uuid>,
}

impl Thumbnails {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            textures: HashMap::new(),
            requested: HashSet::new(),
        }
    }

    fn request(&mut self, asset: &Asset, ctx: &egui::Context) {
        if !self.requested.insert(asset.id) {
            return;
        }
        let tx = self.tx.clone();
        let asset = asset.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send((asset.id, make_thumbnail(&asset)));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((id, image)) = self.rx.try_recv() {
            if let Some(image) = image {
                let texture =
                    ctx.load_texture(format!("thumb-{id}"), image, egui::TextureOptions::LINEAR);
                self.textures.insert(id, texture);
            }
        }
    }
}

fn make_thumbnail(asset: &Asset) -> Option<egui::ColorImage> {
    let image = match asset.kind {
        AssetKind::Image => image::open(&asset.path).ok()?,
        AssetKind::Video => {
            let at = asset.duration_seconds.map_or(0.0, |d| (d * 0.1).min(1.0));
            let output = tool_command("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-ss",
                    &format!("{at:.2}"),
                ])
                .arg("-i")
                .arg(&asset.path)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=640:-2",
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "png",
                    "pipe:1",
                ])
                .output()
                .ok()?;
            image::load_from_memory(&output.stdout).ok()?
        }
        AssetKind::Audio => {
            let output = tool_command("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&asset.path)
                .args([
                    "-filter_complex",
                    "showwavespic=s=640x80:colors=#7fb0ff",
                    "-frames:v",
                    "1",
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "png",
                    "pipe:1",
                ])
                .output()
                .ok()?;
            image::load_from_memory(&output.stdout).ok()?
        }
        AssetKind::Unknown => return None,
    };
    let rgba = image.thumbnail(640, 640).to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum EditMode {
    Layers,
    Projectors,
}

struct ProducerApp {
    ctx: egui::Context,
    project: ShowProject,
    project_path: Option<PathBuf>,
    updater: mapforge_core::update::Updater,
    last_saved: ShowProject,
    sync_due: Option<Instant>,
    live_sync: bool,
    scene: usize,
    selected_layer: Option<Uuid>,
    selected_output: Option<Uuid>,
    selected_cue: Option<Uuid>,
    selected_loop: Option<Uuid>,
    mode: EditMode,
    auto_blend: bool,
    overlap_percent: f32,
    /// One connection per Player PC, in projector order.
    links: Vec<PlayerLink>,
    online: bool,
    player: Option<PlayerState>,
    last_player_position: f64,
    thumbs: Thumbnails,
    status: String,
    playhead_seconds: f64,
    timeline_scale: f32,
    last_frame: Instant,
    confirm_new: bool,
    /// Width and height edits keep the media's shape.
    keep_aspect: bool,
    history: history::History,
    clipboard: Option<Layer>,
    show_controller: bool,
    capture: Option<HotkeyTarget>,
    new_projector: Option<NewProjector>,
    layout_count: usize,
    timeline_zone: Option<timeline::DropZone>,
    window_title: String,
}

/// Something waiting for the next key press to become its hotkey.
#[derive(Clone, Copy, PartialEq)]
enum HotkeyTarget {
    Scene(Uuid),
    Cue(Uuid, Uuid),
    Loop(Uuid, Uuid),
    LoopExit(Uuid, Uuid),
}

/// What a hotkey does.
#[derive(Clone, Copy)]
enum HotAction {
    /// Scene index, optionally starting from a cue or loop start.
    Go(usize, Option<Uuid>),
    ExitLoop,
}

/// Settings in the "Add projector" dialog.
struct NewProjector {
    name: String,
    width: u32,
    height: u32,
    player: String,
    overlap_percent: f32,
    expand_stage: bool,
}

impl ProducerApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        apply_theme(&cc.egui_ctx);
        let project = ShowProject::default();
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            last_saved: project.clone(),
            project,
            project_path: None,
            updater: {
                let ctx = cc.egui_ctx.clone();
                mapforge_core::update::Updater::start(move || ctx.request_repaint())
            },
            sync_due: None,
            live_sync: true,
            scene: 0,
            selected_layer: None,
            selected_output: None,
            selected_cue: None,
            selected_loop: None,
            mode: EditMode::Layers,
            auto_blend: true,
            overlap_percent: 11.8,
            links: Vec::new(),
            online: false,
            player: None,
            last_player_position: -1.0,
            thumbs: Thumbnails::new(),
            status: "Welcome — drop images, videos or music on the timeline".into(),
            playhead_seconds: 0.0,
            timeline_scale: 55.0,
            last_frame: Instant::now(),
            confirm_new: false,
            keep_aspect: true,
            history: history::History::new(&ShowProject::default()),
            clipboard: None,
            show_controller: false,
            capture: None,
            new_projector: None,
            layout_count: 6,
            timeline_zone: None,
            window_title: String::new(),
        };
        app.apply_default_canvas();
        app.last_saved = app.project.clone();
        app.history.reset(&app.project);
        app.refresh_links();
        app
    }

    fn scene(&self) -> &Scene {
        &self.project.scenes[self.scene]
    }

    fn scene_mut(&mut self) -> &mut Scene {
        &mut self.project.scenes[self.scene]
    }

    fn dirty(&self) -> bool {
        self.project != self.last_saved
    }

    fn player_transport(&self) -> Option<Transport> {
        self.player.as_ref().map(|s| s.transport.clone())
    }

    fn player_scene(&self) -> Option<Uuid> {
        self.player.as_ref().and_then(|s| s.scene_id)
    }

    fn asset(&self, id: Uuid) -> Option<&Asset> {
        self.project.assets.iter().find(|a| a.id == id)
    }

    // --- Player control ----------------------------------------------------

    /// Keeps one link per Player address used by the projectors and reads
    /// their latest state.
    fn refresh_links(&mut self) {
        let wanted = self.project.players();
        self.links.retain(|l| wanted.contains(&l.address));
        for address in &wanted {
            if !self.links.iter().any(|l| &l.address == address) {
                self.links
                    .push(PlayerLink::spawn(address.clone(), self.ctx.clone()));
            }
        }
        self.links
            .sort_by_key(|l| wanted.iter().position(|a| a == &l.address));

        let mut online_states = Vec::new();
        for link in &mut self.links {
            let (online, state, reply, sync_failed) = {
                let mut status = link.status.lock().unwrap();
                let failed = std::mem::take(&mut status.sync_failed);
                (
                    status.online,
                    status.state.clone(),
                    status.reply.take(),
                    failed,
                )
            };
            // A Player that just came online may have restarted without the show.
            if (online && !link.was_online) || sync_failed {
                link.synced = None;
            }
            link.was_online = online;
            if let Some(reply) = reply {
                self.status = reply;
            }
            if let Some(state) = state.filter(|_| online) {
                online_states.push(state);
            }
        }
        self.online = !online_states.is_empty();
        // Prefer a Player that has a scene loaded for the transport display.
        self.player = online_states
            .iter()
            .find(|s| s.scene_id.is_some())
            .or(online_states.first())
            .cloned();
    }

    fn broadcast(&self, command: Command) {
        for link in &self.links {
            link.send(command.clone());
        }
    }

    fn player_start_time(link: &PlayerLink, producer_start_ms: u64) -> u64 {
        let offset = link
            .status
            .lock()
            .unwrap()
            .clock_offset_ms
            .unwrap_or(0.0)
            .round() as i64;
        producer_start_ms.saturating_add_signed(offset)
    }

    /// Gives every Player time to prepare, then starts against its locally
    /// adjusted wall clock so network arrival order does not affect sync.
    fn schedule_cue(&self, scene_id: Uuid, seconds: f64) {
        let producer_start = unix_time_ms() + SCHEDULE_LEAD_MS;
        for link in &self.links {
            link.send(Command::CueAt {
                scene_id,
                seconds,
                start_time_unix_ms: Self::player_start_time(link, producer_start),
            });
        }
    }

    fn schedule_play(&self) {
        let producer_start = unix_time_ms() + SCHEDULE_LEAD_MS;
        for link in &self.links {
            link.send(Command::PlayAt {
                start_time_unix_ms: Self::player_start_time(link, producer_start),
            });
        }
    }

    /// Sends the show to every Player with the projectors assigned to it.
    fn sync_now(&mut self) {
        for link in &mut self.links {
            let outputs = self
                .project
                .outputs
                .iter()
                .filter(|o| o.player_address() == link.address)
                .map(|o| o.id)
                .collect();
            link.send_quiet(Command::LoadProject {
                project: self.project.clone(),
                outputs: Some(outputs),
                player: Some(link.address.clone()),
            });
            link.synced = Some(self.project.clone());
        }
        self.sync_due = None;
    }

    fn needs_sync(&self) -> bool {
        self.links
            .iter()
            .any(|l| l.synced.as_ref() != Some(&self.project))
    }

    fn ensure_synced(&mut self) {
        if self.needs_sync() {
            self.sync_now();
        }
    }

    fn auto_sync(&mut self) {
        if !self.live_sync || !self.online || !self.needs_sync() {
            self.sync_due = None;
            return;
        }
        let due = *self
            .sync_due
            .get_or_insert_with(|| Instant::now() + Duration::from_millis(120));
        if Instant::now() >= due {
            self.sync_now();
        }
    }

    fn go_live(&mut self, index: usize) {
        self.go_to(index, None);
    }

    /// Takes a scene live from its start or from one of its cues.
    fn go_to(&mut self, index: usize, cue: Option<Uuid>) {
        let Some(scene) = self.project.scenes.get(index) else {
            return;
        };
        // `cue` is a cue or a loop; a loop starts from its beginning.
        let point = cue.and_then(|id| {
            scene
                .cues
                .iter()
                .find(|c| c.id == id)
                .map(|c| (c.time, c.name.clone()))
                .or_else(|| {
                    scene
                        .loops
                        .iter()
                        .find(|l| l.id == id)
                        .map(|l| (l.start, format!("⟲ {}", l.name)))
                })
        });
        let seconds = point.as_ref().map_or(0.0, |(t, _)| *t);
        let label = match &point {
            Some((_, name)) => format!("{} › {name}", scene.name),
            None => scene.name.clone(),
        };
        let scene_id = scene.id;
        if self.scene != index {
            self.scene = index;
            self.selected_layer = None;
        }
        self.ensure_synced();
        self.schedule_cue(scene_id, seconds);
        self.playhead_seconds = seconds;
        self.status = format!("{label} is live");
    }

    fn toggle_play(&mut self) {
        if self.player_transport() == Some(Transport::Playing) {
            self.broadcast(Command::Pause);
            return;
        }
        let scene_id = self.scene().id;
        self.ensure_synced();
        if self.player_scene() != Some(scene_id) {
            self.schedule_cue(scene_id, self.playhead_seconds);
        } else {
            self.schedule_play();
        }
    }

    fn stop(&mut self) {
        self.broadcast(Command::Stop);
        self.playhead_seconds = 0.0;
    }

    fn toggle_blackout(&mut self) {
        let value = !self.player.as_ref().is_some_and(|s| s.blackout);
        self.broadcast(Command::SetBlackout { value });
    }

    fn seek(&mut self, seconds: f64) {
        self.playhead_seconds = seconds;
        if self.player_scene() == Some(self.scene().id) {
            self.broadcast(Command::Seek { seconds });
        }
    }

    // --- Undo and clipboard --------------------------------------------------

    fn undo(&mut self) {
        if self.history.undo(&mut self.project) {
            self.after_history_jump("Undone");
        } else {
            self.status = "Nothing to undo".into();
        }
    }

    fn redo(&mut self) {
        if self.history.redo(&mut self.project) {
            self.after_history_jump("Redone");
        } else {
            self.status = "Nothing to redo".into();
        }
    }

    fn after_history_jump(&mut self, message: &str) {
        self.scene = self.scene.min(self.project.scenes.len().saturating_sub(1));
        self.sync_overlap_percent();
        self.status = message.into();
    }

    fn copy_layer(&mut self) {
        let id = self.selected_layer;
        self.clipboard = self
            .scene()
            .layers
            .iter()
            .find(|l| Some(l.id) == id)
            .cloned();
        if let Some(layer) = &self.clipboard {
            self.status = format!("Copied {}", layer.name);
        }
    }

    /// Pastes the copied layer as a new top track at the playhead, in any scene.
    fn paste_layer(&mut self) {
        let Some(mut layer) = self.clipboard.clone() else {
            return;
        };
        layer.id = Uuid::new_v4();
        layer.timeline_start = (self.playhead_seconds * 10.0).round() / 10.0;
        // Keep only projectors that still exist.
        let outputs: HashSet<Uuid> = self.project.outputs.iter().map(|o| o.id).collect();
        layer.output_ids.retain(|id| outputs.contains(id));
        self.selected_layer = Some(layer.id);
        self.status = format!("Pasted {}", layer.name);
        self.scene_mut().layers.push(layer);
    }

    // --- Files ---------------------------------------------------------------

    fn new_project(&mut self) {
        self.project = ShowProject::default();
        self.apply_default_canvas();
        self.last_saved = self.project.clone();
        self.project_path = None;
        self.sync_overlap_percent();
        self.scene = 0;
        self.selected_layer = None;
        self.selected_output = None;
        self.playhead_seconds = 0.0;
        self.history.reset(&self.project);
        self.status = "New project".into();
    }

    fn save(&mut self, choose: bool) {
        if choose || self.project_path.is_none() {
            let name = format!("{}.mapforge.json", self.project.name);
            match rfd::FileDialog::new()
                .set_file_name(name)
                .add_filter("MapForge project", &["json"])
                .save_file()
            {
                Some(path) => self.project_path = Some(path),
                None => return,
            }
        }
        if let Some(path) = &self.project_path {
            match save_project_atomic(&self.project, path) {
                Ok(()) => {
                    self.last_saved = self.project.clone();
                    self.status = format!("Saved {}", path.display());
                }
                Err(e) => self.status = format!("Save failed: {e}"),
            }
        }
    }

    fn open(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("MapForge project", &["json"])
            .pick_file()
        {
            self.open_path(path);
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        match load_project(&path) {
            Ok(project) => {
                self.last_saved = project.clone();
                self.project = project;
                self.project_path = Some(path);
                self.sync_overlap_percent();
                self.scene = 0;
                self.selected_layer = None;
                self.selected_output = None;
                self.history.reset(&self.project);
                self.status = "Project opened".into();
            }
            Err(e) => self.status = format!("Open failed: {e}"),
        }
    }

    fn pick_media(&mut self) {
        let mut all: Vec<&str> = IMAGE_EXTENSIONS.to_vec();
        all.extend(VIDEO_EXTENSIONS);
        all.extend(AUDIO_EXTENSIONS);
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images, videos and music", &all)
            .add_filter("Images", &IMAGE_EXTENSIONS)
            .add_filter("Videos", &VIDEO_EXTENSIONS)
            .add_filter("Music and sound", &AUDIO_EXTENSIONS)
            .pick_files()
        {
            let at = self.playhead_seconds;
            for path in paths {
                self.import_path(&path, at, None);
            }
        }
    }

    /// Imports a file and places it on the timeline at `start` seconds, on a
    /// new track above layer index `track` (top of the scene when `None`).
    fn import_path(&mut self, path: &Path, start: f64, track: Option<usize>) {
        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            AssetKind::Image
        } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
            AssetKind::Video
        } else if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
            AssetKind::Audio
        } else {
            self.status = format!(
                "{} is not a supported image, video or sound file",
                path.display()
            );
            return;
        };
        // The same file imported twice reuses its asset.
        let existing = self
            .project
            .assets
            .iter()
            .find(|a| Path::new(&a.path) == path)
            .map(|a| a.id);
        let asset_id = match existing {
            Some(id) => id,
            None => {
                let (width, height, duration_seconds) = match kind {
                    AssetKind::Image => match image::image_dimensions(path) {
                        Ok((w, h)) => (Some(w), Some(h), None),
                        Err(_) => (None, None, None),
                    },
                    _ => probe_media(path),
                };
                let checksum_sha256 = match sha256_file(path) {
                    Ok(v) => v,
                    Err(e) => {
                        self.status = format!("Import failed: {e}");
                        return;
                    }
                };
                let asset = Asset {
                    id: Uuid::new_v4(),
                    path: path.to_string_lossy().to_string(),
                    name: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    kind,
                    checksum_sha256,
                    width,
                    height,
                    duration_seconds,
                    size_bytes: std::fs::metadata(path).ok().map(|m| m.len()),
                };
                let id = asset.id;
                self.project.assets.push(asset);
                id
            }
        };
        self.add_layer_at(asset_id, start, track);
        self.status = format!("Added {}", path.display());
    }

    /// Adds a layer for an asset to the current scene at the playhead.
    fn add_layer(&mut self, asset_id: Uuid) {
        self.add_layer_at(asset_id, self.playhead_seconds, None);
    }

    fn add_layer_at(&mut self, asset_id: Uuid, start: f64, track: Option<usize>) {
        let Some(asset) = self.asset(asset_id) else {
            return;
        };
        let visual = asset.kind != AssetKind::Audio;
        // Media goes in at its real pixel size, centred on the canvas.
        let (w, h) = asset_size(asset);
        let stage = &self.project.stage;
        let layer = Layer {
            id: Uuid::new_v4(),
            asset_id,
            name: asset.name.clone(),
            x: if visual {
                ((stage.width - w) / 2.0).round()
            } else {
                0.0
            },
            y: if visual {
                ((stage.height - h) / 2.0).round()
            } else {
                0.0
            },
            width: if visual { w } else { 1.0 },
            height: if visual { h } else { 1.0 },
            opacity: 1.0,
            output_ids: if visual {
                self.project.outputs.iter().map(|o| o.id).collect()
            } else {
                vec![]
            },
            timeline_start: (start.max(0.0) * 10.0).round() / 10.0,
            timeline_duration: asset.duration_seconds.unwrap_or(10.0).max(0.1),
            source_offset: 0.0,
            looping: asset.kind == AssetKind::Video,
            volume: 1.0,
            audio: true,
        };
        self.selected_layer = Some(layer.id);
        if visual {
            self.mode = EditMode::Layers;
        }
        let layers = &mut self.project.scenes[self.scene].layers;
        let index = track.map_or(layers.len(), |t| (t + 1).min(layers.len()));
        layers.insert(index, layer);
    }

    fn remove_asset(&mut self, asset_id: Uuid) {
        self.project.assets.retain(|a| a.id != asset_id);
        for scene in &mut self.project.scenes {
            scene.layers.retain(|l| l.asset_id != asset_id);
        }
        self.thumbs.textures.remove(&asset_id);
    }

    fn delete_selected(&mut self) {
        if let Some(cue) = self.selected_cue.take() {
            self.scene_mut().cues.retain(|c| c.id != cue);
            return;
        }
        if let Some(region) = self.selected_loop.take() {
            self.scene_mut().loops.retain(|l| l.id != region);
            return;
        }
        match self.mode {
            EditMode::Layers => {
                if let Some(id) = self.selected_layer.take() {
                    self.scene_mut().layers.retain(|l| l.id != id);
                }
            }
            EditMode::Projectors => {
                if let Some(id) = self.selected_output {
                    self.remove_output(id);
                }
            }
        }
    }

    fn duplicate_selected_layer(&mut self) {
        let Some(id) = self.selected_layer else {
            return;
        };
        let layers = &mut self.project.scenes[self.scene].layers;
        if let Some(index) = layers.iter().position(|l| l.id == id) {
            let mut copy = layers[index].clone();
            copy.id = Uuid::new_v4();
            copy.timeline_start = copy.timeline_end();
            self.selected_layer = Some(copy.id);
            layers.insert(index + 1, copy);
        }
    }

    // --- Scenes and cues -----------------------------------------------------

    fn add_scene(&mut self, duplicate: bool) {
        let mut scene = if duplicate {
            let mut copy = self.scene().clone();
            for layer in &mut copy.layers {
                layer.id = Uuid::new_v4();
            }
            for cue in &mut copy.cues {
                cue.id = Uuid::new_v4();
                cue.hotkey.clear();
            }
            for region in &mut copy.loops {
                region.id = Uuid::new_v4();
                region.hotkey.clear();
                region.exit_hotkey.clear();
            }
            copy.name = format!("{} copy", copy.name);
            copy.hotkey.clear();
            copy
        } else {
            Scene::new(format!("Scene {}", self.project.scenes.len() + 1))
        };
        scene.id = Uuid::new_v4();
        self.project.scenes.insert(self.scene + 1, scene);
        self.scene += 1;
        self.selected_layer = None;
        self.selected_cue = None;
    }

    fn delete_scene(&mut self) {
        if self.project.scenes.len() <= 1 {
            return;
        }
        self.project.scenes.remove(self.scene);
        self.scene = self.scene.min(self.project.scenes.len() - 1);
        self.selected_layer = None;
        self.selected_cue = None;
    }

    fn add_cue(&mut self, time: f64) {
        let number = self.scene().cues.len() + 1;
        let cue = Cue {
            id: Uuid::new_v4(),
            name: format!("Cue {number}"),
            time: (time.max(0.0) * 10.0).round() / 10.0,
            hotkey: String::new(),
            button: SceneButton::default(),
        };
        self.selected_cue = Some(cue.id);
        let cues = &mut self.scene_mut().cues;
        cues.push(cue);
        cues.sort_by(|a, b| a.time.total_cmp(&b.time));
        self.status = "Cue added — give it a name and a hotkey on the left".into();
    }

    /// Drops a loop point at the playhead. The show jumps back from there
    /// to the cue before it (or the scene start) until the loop is exited
    /// or the scene changes.
    fn add_loop(&mut self) {
        let scene = self.scene();
        let mut end = self.playhead_seconds;
        if end < 0.2 {
            end = if scene.duration() > 0.0 {
                scene.duration()
            } else {
                5.0
            };
        }
        let start = scene
            .cues
            .iter()
            .map(|c| c.time)
            .filter(|t| *t < end - 0.1)
            .fold(0.0, f64::max);
        let number = scene.loops.len() + 1;
        let region = LoopRegion {
            id: Uuid::new_v4(),
            name: format!("Loop {number}"),
            start: (start * 10.0).round() / 10.0,
            end: (end * 10.0).round() / 10.0,
            count: 0,
            hotkey: String::new(),
            exit_hotkey: String::new(),
            button: SceneButton::default(),
        };
        self.selected_loop = Some(region.id);
        self.selected_cue = None;
        let loops = &mut self.scene_mut().loops;
        loops.push(region);
        loops.sort_by(|a, b| a.end.total_cmp(&b.end));
        self.status =
            "Loop point added — drag the ⟲ marker to move it; it repeats until you change scene or press Enter"
                .into();
    }

    fn exit_loop(&mut self) {
        self.broadcast(Command::ReleaseLoop);
    }

    /// Every hotkey in the show and what it triggers.
    fn hotkey_actions(&self) -> Vec<(String, HotAction)> {
        let mut actions = Vec::new();
        for (index, scene) in self.project.scenes.iter().enumerate() {
            let key = scene_hotkey(index, scene);
            if !key.is_empty() {
                actions.push((key, HotAction::Go(index, None)));
            }
            for cue in &scene.cues {
                if !cue.hotkey.is_empty() {
                    actions.push((cue.hotkey.clone(), HotAction::Go(index, Some(cue.id))));
                }
            }
            for region in &scene.loops {
                if !region.hotkey.is_empty() {
                    actions.push((region.hotkey.clone(), HotAction::Go(index, Some(region.id))));
                }
                if !region.exit_hotkey.is_empty() {
                    actions.push((region.exit_hotkey.clone(), HotAction::ExitLoop));
                }
            }
        }
        actions
    }

    fn hotkey_in_use(&self, key: &str) -> bool {
        self.hotkey_actions()
            .iter()
            .filter(|(k, _)| k == key)
            .count()
            > 1
    }

    // --- Projectors ----------------------------------------------------------

    /// Reads the side-by-side overlap back from the first projector's feather.
    fn sync_overlap_percent(&mut self) {
        if let Some(first) = self.project.outputs.first() {
            if self.project.outputs.len() > 1 && first.stage_width > 0.0 {
                self.overlap_percent = first.blend.right / first.stage_width * 100.0;
            }
        }
    }

    fn open_add_projector(&mut self) {
        let last = self.project.outputs.last();
        self.new_projector = Some(NewProjector {
            name: format!("Projector {}", self.project.outputs.len() + 1),
            width: last.map_or(1920, |o| o.resolution[0]),
            height: last.map_or(1080, |o| o.resolution[1]),
            player: last.map_or("127.0.0.1".into(), |o| {
                mapforge_core::player_host(&o.player_address())
            }),
            overlap_percent: self.overlap_percent.clamp(0.0, 40.0),
            expand_stage: false,
        });
    }

    /// Adds a projector at its native size, to the right of the last one,
    /// without moving or resizing the others.
    fn add_output(&mut self, spec: &NewProjector) {
        let last = self.project.outputs.last().cloned();
        let width = spec.width as f32;
        let height = spec.height as f32;
        let overlap = width * spec.overlap_percent / 100.0;
        let (x, y) = last.as_ref().map_or((0.0, 0.0), |o| {
            (o.stage_x + o.stage_width - overlap, o.stage_y)
        });
        let output = ProjectorOutput {
            id: Uuid::new_v4(),
            name: spec.name.clone(),
            player: mapforge_core::normalize_player_address(&spec.player),
            resolution: [spec.width, spec.height],
            display_index: None,
            warp_corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            mask: mapforge_core::OutputMask::default(),
            stage_x: x.max(0.0),
            stage_y: y,
            stage_width: width,
            stage_height: height,
            blend: last
                .as_ref()
                .map(|o| EdgeBlend {
                    left: 0.0,
                    right: 0.0,
                    top: 0.0,
                    bottom: 0.0,
                    ..o.blend.clone()
                })
                .unwrap_or_default(),
            color: OutputColor::default(),
        };
        if spec.expand_stage {
            let stage = &mut self.project.stage;
            stage.width = stage.width.max((output.stage_x + width).ceil());
        }
        let id = output.id;
        self.project.outputs.push(output);
        self.add_output_to_layers(id);
        if self.auto_blend {
            self.project.auto_blend();
        }
        self.selected_output = Some(id);
        self.mode = EditMode::Projectors;
        self.status = format!("Added {} ({}×{})", spec.name, spec.width, spec.height);
    }

    fn add_output_to_layers(&mut self, id: Uuid) {
        let visual: HashSet<Uuid> = self
            .project
            .assets
            .iter()
            .filter(|a| a.kind != AssetKind::Audio)
            .map(|a| a.id)
            .collect();
        for scene in &mut self.project.scenes {
            for layer in &mut scene.layers {
                if visual.contains(&layer.asset_id) {
                    layer.output_ids.push(id);
                }
            }
        }
    }

    fn remove_output(&mut self, id: Uuid) {
        if self.project.outputs.len() <= 1 {
            self.status = "A show needs at least one projector".into();
            return;
        }
        self.project.outputs.retain(|o| o.id != id);
        for scene in &mut self.project.scenes {
            for layer in &mut scene.layers {
                layer.output_ids.retain(|o| *o != id);
            }
        }
        self.selected_output = None;
        if self.auto_blend {
            self.project.auto_blend();
        }
    }

    /// Overlap in stage pixels when `count` projectors of the first
    /// projector's resolution exactly cover the stage width at native scale.
    fn layout_overlap(&self, count: usize) -> (f32, f32, f32) {
        let res = self
            .project
            .outputs
            .first()
            .map_or([1920, 1080], |o| o.resolution);
        let width = res[0] as f32;
        let n = count.max(1) as f32;
        let overlap = if count > 1 {
            (n * width - self.project.stage.width) / (n - 1.0)
        } else {
            0.0
        };
        (width, res[1] as f32, overlap)
    }

    /// Places `count` projectors at native size so they span the stage,
    /// keeping the names and Player PCs of existing projectors.
    fn layout_projectors(&mut self, count: usize) {
        let (width, height, overlap) = self.layout_overlap(count);
        let template = self.project.outputs.first().cloned();
        while self.project.outputs.len() < count {
            let mut output = template
                .clone()
                .unwrap_or_else(|| ShowProject::default().outputs[0].clone());
            output.id = Uuid::new_v4();
            output.name = format!("Projector {}", self.project.outputs.len() + 1);
            let id = output.id;
            self.project.outputs.push(output);
            self.add_output_to_layers(id);
        }
        let removed: Vec<Uuid> = self
            .project
            .outputs
            .iter()
            .skip(count)
            .map(|o| o.id)
            .collect();
        for id in removed {
            self.remove_output(id);
        }
        for (i, output) in self.project.outputs.iter_mut().enumerate() {
            output.resolution = [width as u32, height as u32];
            output.stage_x = i as f32 * (width - overlap);
            output.stage_y = 0.0;
        }
        self.project.lock_output_sizes();
        self.project.auto_blend();
        self.sync_overlap_percent();
        self.status = format!(
            "{count} projectors of {width:.0}×{height:.0} with {overlap:.0} px overlap ({:.1}%)",
            overlap / width * 100.0
        );
    }

    /// New shows start on the 10400 × 1080 canvas with six 1920 × 1080
    /// projectors overlapping by 224 px.
    fn apply_default_canvas(&mut self) {
        self.project.stage.width = 10400.0;
        self.project.stage.height = 1080.0;
        self.layout_projectors(6);
        self.status = "New show: 10400 × 1080 canvas, 6 projectors (224 px blends)".into();
    }

    /// Changes the canvas size only; projectors and layers keep their size.
    fn resize_stage(&mut self, width: f32, height: f32) {
        self.project.stage.width = width.clamp(16.0, 32768.0).round();
        self.project.stage.height = height.clamp(16.0, 32768.0).round();
    }

    fn selected_asset_size(&self) -> Option<(f32, f32)> {
        let layer = self
            .scene()
            .layers
            .iter()
            .find(|l| Some(l.id) == self.selected_layer)?;
        let asset = self
            .project
            .assets
            .iter()
            .find(|a| a.id == layer.asset_id)?;
        Some((asset.width? as f32, asset.height? as f32))
    }

    fn match_stage_to_selected(&mut self) {
        let Some((w, h)) = self.selected_asset_size() else {
            return;
        };
        self.resize_stage(w, h);
        let stage = self.project.stage.clone();
        if let Some(layer) = self.selected_layer_mut() {
            layer.x = 0.0;
            layer.y = 0.0;
            layer.width = stage.width;
            layer.height = stage.height;
        }
        self.status = format!(
            "Stage is now {} × {} and the media fills it across all projectors",
            stage.width, stage.height
        );
    }

    fn selected_layer_mut(&mut self) -> Option<&mut Layer> {
        let id = self.selected_layer?;
        self.project.scenes[self.scene]
            .layers
            .iter_mut()
            .find(|l| l.id == id)
    }

    // --- Keyboard and drag-and-drop -----------------------------------------

    fn handle_input(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            // Files dropped over the timeline land at that time and track.
            let pointer = ctx.input(|i| i.pointer.latest_pos());
            let (start, track) = pointer
                .and_then(|p| self.timeline_drop_target(p))
                .unwrap_or((self.playhead_seconds, None));
            for path in dropped {
                self.import_path(&path, start, track);
            }
        }

        use egui::{Key, Modifiers};
        let pressed: Vec<Key> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } if !modifiers.command && !modifiers.ctrl && !modifiers.alt => Some(*key),
                    _ => None,
                })
                .collect()
        });

        if let Some(target) = self.capture {
            if let Some(key) = pressed.first().copied() {
                ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key));
                self.capture = None;
                let name = key.name().to_string();
                let value = match name.as_str() {
                    "Escape" => return,
                    "Delete" | "Backspace" => String::new(),
                    reserved if RESERVED_KEYS.contains(&reserved) => {
                        self.status =
                            format!("{reserved} is used by the Producer; pick another key");
                        return;
                    }
                    _ => name,
                };
                self.set_hotkey(target, value);
            }
            return;
        }

        let command = Modifiers::COMMAND;
        let (save, save_as, open, import, duplicate) = ctx.input_mut(|i| {
            (
                i.consume_key(command, Key::S),
                i.consume_key(command | Modifiers::SHIFT, Key::S),
                i.consume_key(command, Key::O),
                i.consume_key(command, Key::I),
                i.consume_key(command, Key::D),
            )
        });
        if save_as {
            self.save(true);
        } else if save {
            self.save(false);
        }
        if open {
            self.open();
        }
        if import {
            self.pick_media();
        }
        if duplicate {
            self.duplicate_selected_layer();
        }
        // Text boxes keep their own undo and copy/paste while typing.
        if !ctx.wants_keyboard_input() {
            let (undo, redo, copy, paste) = ctx.input_mut(|i| {
                let redo = i.consume_key(command | Modifiers::SHIFT, Key::Z)
                    || i.consume_key(command, Key::Y);
                let undo = i.consume_key(command, Key::Z);
                // The OS turns ⌘C / ⌘V into copy and paste events.
                let copy = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
                let paste = i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)));
                (undo, redo, copy, paste)
            });
            if redo {
                self.redo();
            } else if undo {
                self.undo();
            }
            if copy {
                self.copy_layer();
            }
            if paste {
                self.paste_layer();
            }
        }

        // Scene and cue hotkeys. Function keys work even while typing.
        let typing = ctx.wants_keyboard_input();
        let actions = self.hotkey_actions();
        for key in &pressed {
            let name = key.name();
            let function_key = name.starts_with('F') && name.len() > 1;
            if typing && !function_key {
                continue;
            }
            if let Some((_, action)) = actions.iter().find(|(k, _)| k == name) {
                ctx.input_mut(|i| i.consume_key(Modifiers::NONE, *key));
                match *action {
                    HotAction::Go(scene, point) => self.go_to(scene, point),
                    HotAction::ExitLoop => self.exit_loop(),
                }
            }
        }

        if typing {
            return;
        }
        let (space, escape, blackout, delete, cue, add_loop, exit_loop) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Space),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::B),
                i.consume_key(Modifiers::NONE, Key::Delete)
                    || i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::NONE, Key::M),
                i.consume_key(Modifiers::NONE, Key::L),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        if add_loop {
            self.add_loop();
        }
        if exit_loop {
            self.exit_loop();
        }
        if space {
            self.toggle_play();
        }
        if escape {
            self.stop();
        }
        if blackout {
            self.toggle_blackout();
        }
        if delete {
            self.delete_selected();
        }
        if cue {
            self.add_cue(self.playhead_seconds);
        }
    }

    fn set_hotkey(&mut self, target: HotkeyTarget, key: String) {
        match target {
            HotkeyTarget::Scene(id) => {
                if let Some(scene) = self.project.scenes.iter_mut().find(|s| s.id == id) {
                    scene.hotkey = key.clone();
                }
            }
            HotkeyTarget::Cue(scene_id, cue_id) => {
                if let Some(cue) = self
                    .project
                    .scenes
                    .iter_mut()
                    .find(|s| s.id == scene_id)
                    .and_then(|s| s.cues.iter_mut().find(|c| c.id == cue_id))
                {
                    cue.hotkey = key.clone();
                }
            }
            HotkeyTarget::Loop(scene_id, loop_id) | HotkeyTarget::LoopExit(scene_id, loop_id) => {
                if let Some(region) = self
                    .project
                    .scenes
                    .iter_mut()
                    .find(|s| s.id == scene_id)
                    .and_then(|s| s.loops.iter_mut().find(|l| l.id == loop_id))
                {
                    if matches!(target, HotkeyTarget::Loop(..)) {
                        region.hotkey = key.clone();
                    } else {
                        region.exit_hotkey = key.clone();
                    }
                }
            }
        }
        self.status = if key.is_empty() {
            "Hotkey cleared".into()
        } else if self.hotkey_in_use(&key) {
            format!("{key} is now used twice — the first one in the list wins")
        } else {
            format!("Hotkey set to {key}")
        };
    }

    /// A button that shows a hotkey and records a new one when clicked.
    fn hotkey_button(&mut self, ui: &mut egui::Ui, target: HotkeyTarget, current: &str) {
        let capturing = self.capture == Some(target);
        let text = if capturing {
            RichText::new("press a key…").color(WARN)
        } else if current.is_empty() {
            RichText::new("set key").color(MUTED)
        } else {
            RichText::new(current).monospace().strong().color(ACCENT)
        };
        if ui
            .add(
                egui::Button::new(text)
                    .small()
                    .min_size(egui::vec2(44.0, 0.0)),
            )
            .on_hover_text("Click, then press any key (Delete clears, Esc cancels)")
            .clicked()
        {
            self.capture = if capturing { None } else { Some(target) };
        }
    }

    // --- Panels --------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("MapForge").strong().size(18.0).color(ACCENT));
            ui.add(
                egui::TextEdit::singleline(&mut self.project.name)
                    .desired_width(150.0)
                    .hint_text("Show name"),
            );
            if self.dirty() {
                ui.label(RichText::new("● unsaved").small().color(WARN));
            }
            ui.separator();
            ui.menu_button("File", |ui| {
                if ui.button("New show").clicked() {
                    if self.dirty() {
                        self.confirm_new = true;
                    } else {
                        self.new_project();
                    }
                    ui.close_menu();
                }
                if ui.button("Open…   ⌘O").clicked() {
                    self.open();
                    ui.close_menu();
                }
                if ui.button("Save   ⌘S").clicked() {
                    self.save(false);
                    ui.close_menu();
                }
                if ui.button("Save as…   ⇧⌘S").clicked() {
                    self.save(true);
                    ui.close_menu();
                }
                ui.separator();
                if ui
                    .button(format!(
                        "Check for updates (this is {})",
                        mapforge_core::update::current_version()
                    ))
                    .clicked()
                {
                    self.updater.check_now();
                    ui.close_menu();
                }
            });
            let can_undo = self.history.can_undo(&self.project);
            let can_redo = self.history.can_redo();
            let has_layer = self.selected_layer.is_some();
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(can_undo, egui::Button::new("Undo   ⌘Z"))
                    .clicked()
                {
                    self.undo();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(can_redo, egui::Button::new("Redo   ⇧⌘Z"))
                    .clicked()
                {
                    self.redo();
                    ui.close_menu();
                }
                ui.separator();
                if ui
                    .add_enabled(has_layer, egui::Button::new("Copy layer   ⌘C"))
                    .clicked()
                {
                    self.copy_layer();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(
                        self.clipboard.is_some(),
                        egui::Button::new("Paste layer   ⌘V"),
                    )
                    .on_hover_text("Pastes at the playhead, also into another scene")
                    .clicked()
                {
                    self.paste_layer();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(has_layer, egui::Button::new("Duplicate   ⌘D"))
                    .clicked()
                {
                    self.duplicate_selected_layer();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(
                        has_layer || self.selected_cue.is_some(),
                        egui::Button::new("Delete   ⌫"),
                    )
                    .clicked()
                {
                    self.delete_selected();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Add cue at playhead   M").clicked() {
                    self.add_cue(self.playhead_seconds);
                    ui.close_menu();
                }
            });
            if ui
                .add_enabled(can_undo, egui::Button::new("⟲ Undo"))
                .on_hover_text("⌘Z")
                .clicked()
            {
                self.undo();
            }
            if ui
                .add_enabled(can_redo, egui::Button::new("⟳ Redo"))
                .on_hover_text("⇧⌘Z")
                .clicked()
            {
                self.redo();
            }
            if ui
                .add(egui::Button::new(RichText::new("+ Add media").strong()).fill(ACCENT))
                .on_hover_text("Images, videos and music (⌘I) — or drag files onto the timeline")
                .clicked()
            {
                self.pick_media();
            }
            if ui
                .button("+ Projector")
                .on_hover_text("Add a projector with its own resolution and Player PC")
                .clicked()
            {
                self.open_add_projector();
            }
            if ui
                .button("📱 Controller")
                .on_hover_text(
                    "Show PC (master, autoplay, sound) and the phone/tablet control page",
                )
                .clicked()
            {
                self.show_controller = !self.show_controller;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.player_pill(ui);
                ui.separator();
                let blackout = self.player.as_ref().is_some_and(|s| s.blackout);
                if ui
                    .add(
                        egui::Button::new(RichText::new("Blackout").strong()).fill(if blackout {
                            DANGER
                        } else {
                            Color32::from_gray(45)
                        }),
                    )
                    .on_hover_text("B")
                    .clicked()
                {
                    self.toggle_blackout();
                }
                if ui.button("■ Stop").on_hover_text("Esc").clicked() {
                    self.stop();
                }
                if let Some(name) = self.player.as_ref().and_then(|s| s.loop_name.clone()) {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!("⏭ Exit loop “{name}”")).strong(),
                            )
                            .fill(LOOP_COLOR.gamma_multiply(0.55)),
                        )
                        .on_hover_text("Enter — lets the show continue past the loop")
                        .clicked()
                    {
                        self.exit_loop();
                    }
                }
                let playing = self.player_transport() == Some(Transport::Playing);
                let (label, fill) = if playing {
                    ("⏸ Pause", Color32::from_gray(60))
                } else {
                    ("▶ Play", LIVE)
                };
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).strong().size(15.0))
                            .fill(fill)
                            .min_size(egui::vec2(96.0, 28.0)),
                    )
                    .on_hover_text("Space · plays the scene you are editing from the playhead")
                    .clicked()
                {
                    self.toggle_play();
                }
                if !self.live_sync
                    && self.needs_sync()
                    && ui
                        .add(egui::Button::new("Send changes").fill(WARN.gamma_multiply(0.6)))
                        .clicked()
                {
                    self.sync_now();
                }
            });
        });
    }

    fn player_pill(&mut self, ui: &mut egui::Ui) {
        let online = self
            .links
            .iter()
            .filter(|l| l.status.lock().unwrap().online)
            .count();
        let total = self.links.len();
        let (text, color) = match (&self.player, online) {
            (Some(state), n) if n > 0 => {
                let color = if n < total {
                    WARN
                } else {
                    match state.transport {
                        Transport::Playing => LIVE,
                        Transport::Paused => WARN,
                        Transport::Ready => ACCENT,
                        Transport::Stopped => MUTED,
                    }
                };
                (
                    format!("● {n}/{total} Players · {:?}", state.transport),
                    color,
                )
            }
            _ => (format!("● 0/{total} Players online"), DANGER),
        };
        let response = ui
            .add(egui::Button::new(RichText::new(text).color(color)).frame(false))
            .on_hover_text("Player PCs");
        let popup_id = ui.make_persistent_id("player_popup");
        if response.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup_id));
        }
        egui::popup_below_widget(
            ui,
            popup_id,
            &response,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_min_width(280.0);
                for link in &self.links {
                    let status = link.status.lock().unwrap();
                    let (dot, color) = if status.online {
                        ("●", LIVE)
                    } else {
                        ("●", DANGER)
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(dot).color(color));
                        ui.label(RichText::new(&link.address).monospace());
                        if let Some(ms) = status.latency_ms {
                            ui.label(RichText::new(format!("{ms:.0} ms")).small().color(MUTED));
                        }
                        if let Some(offset) = status.clock_offset_ms {
                            ui.label(
                                RichText::new(format!("clock {offset:+.0} ms"))
                                    .small()
                                    .color(MUTED),
                            );
                        }
                    });
                    if let Some(state) = &status.state {
                        if let Some(start) = state.scheduled_start_unix_ms {
                            ui.small(format!("Prepared for synchronized start at {start}"));
                        }
                        if let Some(error) = state.start_error_ms {
                            ui.small(format!("Last scheduled-start error: {error:.1} ms"));
                        }
                    }
                }
                let positions: Vec<f64> = self
                    .links
                    .iter()
                    .filter_map(|link| {
                        let status = link.status.lock().unwrap();
                        if status.online {
                            status.state.as_ref().map(|state| state.position_seconds)
                        } else {
                            None
                        }
                    })
                    .collect();
                if positions.len() > 1 {
                    let min = positions.iter().copied().fold(f64::INFINITY, f64::min);
                    let max = positions.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                    ui.label(format!(
                        "Reported playback spread: {:.1} ms",
                        (max - min) * 1000.0
                    ));
                }
                ui.small("Set each projector's Player PC in Edit projectors.");
                ui.separator();
                ui.checkbox(&mut self.live_sync, "Live sync")
                    .on_hover_text("Send every change to the Players immediately");
                if ui.button("Send show to all Players now").clicked() {
                    self.sync_now();
                }
            },
        );
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.scenes_ui(ui);
            ui.add_space(8.0);
            self.cues_ui(ui);
            ui.add_space(8.0);
            self.loops_ui(ui);
            ui.add_space(8.0);
            self.layers_ui(ui);
            ui.add_space(8.0);
            self.media_ui(ui);
        });
    }

    fn scenes_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Scenes");
        let live_scene = self.player_scene();
        let playing = self.player_transport() == Some(Transport::Playing);
        let mut go = None;
        let rows: Vec<(usize, Uuid, String, String)> = self
            .project
            .scenes
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.id, s.name.clone(), scene_hotkey(i, s)))
            .collect();
        for (index, id, name, key) in rows {
            let editing = index == self.scene;
            let live = live_scene == Some(id);
            ui.horizontal(|ui| {
                self.hotkey_button(ui, HotkeyTarget::Scene(id), &key);
                if ui.selectable_label(editing, &name).clicked() {
                    self.scene = index;
                    self.selected_layer = None;
                    self.selected_cue = None;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("▶")
                        .on_hover_text("Go live with this scene")
                        .clicked()
                    {
                        go = Some(index);
                    }
                    if live {
                        ui.label(
                            RichText::new(if playing { "LIVE" } else { "READY" })
                                .small()
                                .color(if playing { LIVE } else { WARN }),
                        );
                    }
                });
            });
        }
        if let Some(index) = go {
            self.go_live(index);
        }
        ui.horizontal(|ui| {
            if ui.small_button("+ New").clicked() {
                self.add_scene(false);
            }
            if ui.small_button("Duplicate").clicked() {
                self.add_scene(true);
            }
            if ui
                .add_enabled(
                    self.project.scenes.len() > 1,
                    egui::Button::new("Delete").small(),
                )
                .clicked()
            {
                self.delete_scene();
            }
        });
        let scene = &mut self.project.scenes[self.scene];
        ui.horizontal(|ui| {
            ui.label(RichText::new("Name").small().color(MUTED));
            ui.add(egui::TextEdit::singleline(&mut scene.name).desired_width(f32::INFINITY));
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("At the end").small().color(MUTED));
            egui::ComboBox::from_id_salt("end_action")
                .selected_text(end_action_name(scene.end_action))
                .show_ui(ui, |ui| {
                    for action in [
                        EndAction::Loop,
                        EndAction::Hold,
                        EndAction::Stop,
                        EndAction::Next,
                    ] {
                        ui.selectable_value(&mut scene.end_action, action, end_action_name(action));
                    }
                });
        });
    }

    fn cues_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Cues (start points) in this scene");
        let scene_id = self.scene().id;
        let scene_index = self.scene;
        if self.scene().cues.is_empty() {
            ui.label(
                RichText::new("Press M or “+ Cue” to mark a start point at the playhead.")
                    .color(MUTED),
            );
        }
        let cues: Vec<Cue> = self.scene().cues.clone();
        let mut go = None;
        let mut remove = None;
        for cue in &cues {
            ui.horizontal(|ui| {
                self.hotkey_button(ui, HotkeyTarget::Cue(scene_id, cue.id), &cue.hotkey);
                let selected = self.selected_cue == Some(cue.id);
                if ui
                    .selectable_label(
                        selected,
                        format!("◆ {}  {}", cue.name, format_duration_precise(cue.time)),
                    )
                    .clicked()
                {
                    self.selected_cue = Some(cue.id);
                    self.playhead_seconds = cue.time;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text("Remove cue").clicked() {
                        remove = Some(cue.id);
                    }
                    if ui
                        .small_button("▶")
                        .on_hover_text("Go live from this cue")
                        .clicked()
                    {
                        go = Some(cue.id);
                    }
                });
            });
        }
        if let Some(id) = go {
            self.go_to(scene_index, Some(id));
        }
        if let Some(id) = remove {
            self.scene_mut().cues.retain(|c| c.id != id);
        }
        if ui.small_button("+ Cue at playhead  (M)").clicked() {
            self.add_cue(self.playhead_seconds);
        }
        let selected = self.selected_cue;
        if let Some(cue) = self
            .scene_mut()
            .cues
            .iter_mut()
            .find(|c| Some(c.id) == selected)
        {
            egui::Grid::new("cue_edit").num_columns(2).show(ui, |ui| {
                ui.label(RichText::new("Name").small().color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut cue.name).desired_width(150.0));
                ui.end_row();
                ui.label(RichText::new("Time").small().color(MUTED));
                ui.add(
                    egui::DragValue::new(&mut cue.time)
                        .speed(0.1)
                        .range(0.0..=86400.0)
                        .suffix(" s"),
                );
                ui.end_row();
                ui.label(RichText::new("iPad button").small().color(MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut cue.button.label)
                        .desired_width(150.0)
                        .hint_text(&cue.name),
                );
                ui.end_row();
            });
        }
    }

    fn loops_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Loops in this scene");
        let scene_id = self.scene().id;
        let scene_index = self.scene;
        if self.scene().loops.is_empty() {
            ui.label(
                RichText::new(
                    "Press L or “⟲ + Loop” to drop a loop point at the playhead. The scene \
                     repeats up to it until you change scene or press Enter.",
                )
                .color(MUTED),
            );
        }
        let live_loop = self.player.as_ref().and_then(|s| s.loop_name.clone());
        let loops = self.scene().loops.clone();
        let mut go = None;
        let mut remove = None;
        for region in &loops {
            ui.horizontal(|ui| {
                self.hotkey_button(ui, HotkeyTarget::Loop(scene_id, region.id), &region.hotkey);
                let back_to = if region.start <= 0.0 {
                    "start".to_string()
                } else {
                    format_duration_precise(region.start)
                };
                let mut text = RichText::new(format!(
                    "⟲ {}  at {}, back to {back_to}{}",
                    region.name,
                    format_duration_precise(region.end),
                    if region.count > 0 {
                        format!(" ×{}", region.count)
                    } else {
                        String::new()
                    }
                ));
                if live_loop.as_deref() == Some(region.name.as_str()) {
                    text = text.color(LOOP_COLOR).strong();
                }
                if ui
                    .selectable_label(self.selected_loop == Some(region.id), text)
                    .clicked()
                {
                    self.selected_loop = Some(region.id);
                    self.selected_cue = None;
                    self.playhead_seconds = region.start;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text("Remove loop").clicked() {
                        remove = Some(region.id);
                    }
                    if ui
                        .small_button("▶")
                        .on_hover_text("Go live from the start of this loop")
                        .clicked()
                    {
                        go = Some(region.id);
                    }
                });
            });
        }
        if let Some(id) = go {
            self.go_to(scene_index, Some(id));
        }
        if let Some(id) = remove {
            self.scene_mut().loops.retain(|l| l.id != id);
        }
        if ui.small_button("⟲ + Loop  (L)").clicked() {
            self.add_loop();
        }
        let Some(selected) = self.selected_loop else {
            return;
        };
        let Some(region) = self
            .scene()
            .loops
            .iter()
            .find(|l| l.id == selected)
            .cloned()
        else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Exit key").small().color(MUTED));
            self.hotkey_button(
                ui,
                HotkeyTarget::LoopExit(scene_id, region.id),
                &region.exit_hotkey,
            );
            ui.label(RichText::new("(Enter always exits)").small().color(MUTED));
        });
        let cues = self.scene().cues.clone();
        if let Some(region) = self.scene_mut().loops.iter_mut().find(|l| l.id == selected) {
            egui::Grid::new("loop_edit").num_columns(2).show(ui, |ui| {
                ui.label(RichText::new("Name").small().color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut region.name).desired_width(150.0));
                ui.end_row();
                ui.label(RichText::new("Loop point").small().color(MUTED));
                let min_end = region.start + 0.1;
                ui.add(
                    egui::DragValue::new(&mut region.end)
                        .speed(0.1)
                        .range(min_end..=86400.0)
                        .suffix(" s"),
                );
                ui.end_row();
                ui.label(RichText::new("Jumps back to").small().color(MUTED));
                ui.horizontal(|ui| {
                    let current = if region.start <= 0.0 {
                        "Scene start".to_string()
                    } else {
                        cues.iter()
                            .find(|c| (c.time - region.start).abs() < 0.05)
                            .map(|c| format!("◆ {}", c.name))
                            .unwrap_or_else(|| "Custom time".into())
                    };
                    egui::ComboBox::from_id_salt("loop_back_to")
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(region.start <= 0.0, "Scene start")
                                .clicked()
                            {
                                region.start = 0.0;
                            }
                            for cue in cues.iter().filter(|c| c.time < region.end - 0.1) {
                                let label = format!("◆ {}  ({:.1}s)", cue.name, cue.time);
                                if ui
                                    .selectable_label((cue.time - region.start).abs() < 0.05, label)
                                    .clicked()
                                {
                                    region.start = cue.time;
                                }
                            }
                        });
                    let max_start = region.end - 0.1;
                    ui.add(
                        egui::DragValue::new(&mut region.start)
                            .speed(0.1)
                            .range(0.0..=max_start)
                            .suffix(" s"),
                    );
                });
                ui.end_row();
                ui.label(RichText::new("Plays").small().color(MUTED));
                ui.add(
                    egui::DragValue::new(&mut region.count)
                        .range(0..=999)
                        .custom_formatter(|v, _| {
                            if v == 0.0 {
                                "until exited".into()
                            } else {
                                format!("{v:.0} times")
                            }
                        }),
                );
                ui.end_row();
                ui.label(RichText::new("iPad button").small().color(MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut region.button.label)
                        .desired_width(150.0)
                        .hint_text(&region.name),
                );
                ui.end_row();
            });
            region.end = region.end.max(region.start + 0.1);
        }
    }

    fn layers_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Layers in this scene");
        let count = self.scene().layers.len();
        if count == 0 {
            ui.label(RichText::new("Empty scene. Drag media onto the timeline.").color(MUTED));
        }
        let mut action: Option<(usize, i32)> = None;
        // Top-most layer first, like most editors.
        for index in (0..count).rev() {
            let layer = &self.project.scenes[self.scene].layers[index];
            let (id, asset_id) = (layer.id, layer.asset_id);
            let icon = self.asset(asset_id).map_or("?", |a| kind_icon(&a.kind));
            let mut label = format!("{icon} {}", layer.name);
            if layer.looping {
                label.push_str("  ⟲");
            }
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.selected_layer == Some(id), label)
                    .clicked()
                {
                    self.selected_layer = Some(id);
                    self.selected_cue = None;
                    self.mode = EditMode::Layers;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text("Remove layer").clicked() {
                        action = Some((index, 0));
                    }
                    if ui
                        .add_enabled(index > 0, egui::Button::new("⏷").small())
                        .on_hover_text("Move down")
                        .clicked()
                    {
                        action = Some((index, -1));
                    }
                    if ui
                        .add_enabled(index + 1 < count, egui::Button::new("⏶").small())
                        .on_hover_text("Move up")
                        .clicked()
                    {
                        action = Some((index, 1));
                    }
                });
            });
        }
        if let Some((index, direction)) = action {
            let layers = &mut self.project.scenes[self.scene].layers;
            match direction {
                0 => {
                    let removed = layers.remove(index);
                    if self.selected_layer == Some(removed.id) {
                        self.selected_layer = None;
                    }
                }
                1 => layers.swap(index, index + 1),
                _ => layers.swap(index, index - 1),
            }
        }
    }

    fn media_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Media library");
        if self.project.assets.is_empty() {
            ui.label(
                RichText::new("No media yet. Click “+ Add media” or drop files on the timeline.")
                    .color(MUTED),
            );
        } else {
            ui.label(
                RichText::new("Drag a thumbnail onto the timeline or preview.")
                    .small()
                    .color(MUTED),
            );
        }
        let mut add = None;
        let mut remove = None;
        for asset in &self.project.assets {
            egui::Frame::group(ui.style()).fill(CANVAS).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let size = egui::vec2(72.0, 40.0);
                    let texture = self.thumbs.textures.get(&asset.id).map(|t| t.id());
                    ui.dnd_drag_source(
                        egui::Id::new(("media_drag", asset.id)),
                        MediaDrag(asset.id),
                        |ui| match texture {
                            Some(tex) => {
                                ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(
                                    tex, size,
                                )));
                            }
                            None => {
                                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                                ui.painter().rect_filled(rect, 3.0, Color32::from_gray(30));
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    kind_icon(&asset.kind),
                                    egui::FontId::proportional(18.0),
                                    Color32::GRAY,
                                );
                            }
                        },
                    )
                    .response
                    .on_hover_cursor(egui::CursorIcon::Grab);
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new(&asset.name).strong()).truncate());
                        ui.label(RichText::new(asset_details(asset)).small().color(MUTED));
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("Add at playhead")
                                .on_hover_text("Adds a new track in this scene")
                                .clicked()
                            {
                                add = Some(asset.id);
                            }
                            if ui
                                .small_button("×")
                                .on_hover_text("Remove from show (all scenes)")
                                .clicked()
                            {
                                remove = Some(asset.id);
                            }
                        });
                    });
                });
            });
        }
        if let Some(id) = add {
            self.add_layer(id);
        }
        if let Some(id) = remove {
            self.remove_asset(id);
        }
    }

    fn right_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| match self.mode {
            EditMode::Layers => {
                self.layer_properties(ui);
                ui.add_space(8.0);
                self.stage_ui(ui);
            }
            EditMode::Projectors => {
                self.projectors_ui(ui);
                ui.add_space(8.0);
                self.lan_ui(ui);
                ui.add_space(8.0);
                self.stage_ui(ui);
            }
        });
    }

    fn layer_properties(&mut self, ui: &mut egui::Ui) {
        section(ui, "Layer");
        let stage = self.project.stage.clone();
        let asset_size = self
            .scene()
            .layers
            .iter()
            .find(|l| Some(l.id) == self.selected_layer)
            .and_then(|l| self.project.assets.iter().find(|a| a.id == l.asset_id))
            .map(asset_size);
        let kind = self
            .scene()
            .layers
            .iter()
            .find(|l| Some(l.id) == self.selected_layer)
            .and_then(|l| self.project.assets.iter().find(|a| a.id == l.asset_id))
            .map_or(AssetKind::Unknown, |a| a.kind.clone());
        let visual = matches!(kind, AssetKind::Image | AssetKind::Video);
        let outputs: Vec<(Uuid, String)> = self
            .project
            .outputs
            .iter()
            .map(|o| (o.id, o.name.clone()))
            .collect();
        let mut duplicate = false;
        let mut delete = false;
        let mut keep = self.keep_aspect;
        let Some(layer) = self.selected_layer_mut() else {
            ui.label(RichText::new("Click a layer in the preview or the layer list.").color(MUTED));
            return;
        };
        ui.add(egui::TextEdit::singleline(&mut layer.name).desired_width(f32::INFINITY));
        ui.add_space(4.0);
        if visual {
            // The media's own shape; falls back to the layer's current one.
            let (media_w, media_h) = asset_size.unwrap_or((layer.width, layer.height));
            let aspect = media_w / media_h.max(1.0);
            egui::Grid::new("transform")
                .num_columns(4)
                .spacing([6.0, 4.0])
                .show(ui, |ui| {
                    ui.label("X");
                    ui.add(egui::DragValue::new(&mut layer.x).speed(1.0).suffix(" px"));
                    ui.label("Y");
                    ui.add(egui::DragValue::new(&mut layer.y).speed(1.0).suffix(" px"));
                    ui.end_row();
                    ui.label("W");
                    if ui
                        .add(
                            egui::DragValue::new(&mut layer.width)
                                .speed(1.0)
                                .range(1.0..=32768.0)
                                .suffix(" px"),
                        )
                        .changed()
                        && keep
                    {
                        layer.height = (layer.width / aspect).round();
                    }
                    ui.label("H");
                    if ui
                        .add(
                            egui::DragValue::new(&mut layer.height)
                                .speed(1.0)
                                .range(1.0..=32768.0)
                                .suffix(" px"),
                        )
                        .changed()
                        && keep
                    {
                        layer.width = (layer.height * aspect).round();
                    }
                    ui.end_row();
                });
            ui.horizontal(|ui| {
                ui.checkbox(&mut keep, "🔗 Keep shape").on_hover_text(
                    "Width and height change together so the media is never distorted",
                );
                ui.label(
                    RichText::new(format!(
                        "{:.0}% of original",
                        layer.width / media_w.max(1.0) * 100.0
                    ))
                    .small()
                    .color(MUTED),
                );
            });
            let distorted = ((layer.width / layer.height.max(1.0)) / aspect - 1.0).abs() > 0.01;
            if distorted {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚠ Stretched out of shape").color(WARN));
                    if ui.small_button("Fix shape").clicked() {
                        layer.height = (layer.width / aspect).round();
                    }
                });
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("1:1 Original")
                    .on_hover_text("The media's real pixel size")
                    .clicked()
                {
                    let (cx, cy) = (layer.x + layer.width / 2.0, layer.y + layer.height / 2.0);
                    layer.width = media_w;
                    layer.height = media_h;
                    layer.x = (cx - media_w / 2.0).round();
                    layer.y = (cy - media_h / 2.0).round();
                }
                if ui
                    .button("Fit inside")
                    .on_hover_text(
                        "Largest size that shows the whole media on the canvas, keeping its shape",
                    )
                    .clicked()
                {
                    let scale = (stage.width / media_w).min(stage.height / media_h);
                    layer.width = (media_w * scale).round();
                    layer.height = (media_h * scale).round();
                    layer.x = ((stage.width - layer.width) / 2.0).round();
                    layer.y = ((stage.height - layer.height) / 2.0).round();
                }
                if ui
                    .button("Cover")
                    .on_hover_text(
                        "Fills the whole canvas keeping its shape; edges may go past the canvas",
                    )
                    .clicked()
                {
                    let scale = (stage.width / media_w).max(stage.height / media_h);
                    layer.width = (media_w * scale).round();
                    layer.height = (media_h * scale).round();
                    layer.x = ((stage.width - layer.width) / 2.0).round();
                    layer.y = ((stage.height - layer.height) / 2.0).round();
                }
                if ui.button("Center").clicked() {
                    layer.x = ((stage.width - layer.width) / 2.0).round();
                    layer.y = ((stage.height - layer.height) / 2.0).round();
                }
                if ui
                    .add(egui::Button::new(RichText::new("Stretch").color(WARN)))
                    .on_hover_text(
                        "Distorts the media to exactly the canvas size — usually not what you want",
                    )
                    .clicked()
                {
                    layer.x = 0.0;
                    layer.y = 0.0;
                    layer.width = stage.width;
                    layer.height = stage.height;
                }
            });
            ui.add(
                egui::Slider::new(&mut layer.opacity, 0.0..=1.0)
                    .text("Opacity")
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            );
        }
        if matches!(kind, AssetKind::Video | AssetKind::Audio) {
            ui.checkbox(&mut layer.looping, "Loop inside the clip")
                .on_hover_text(
                    "Repeats the media while the clip is on the timeline. Off: plays once \
                     (video holds its last frame)",
                );
        }
        if kind == AssetKind::Video {
            ui.checkbox(&mut layer.audio, "Play the video's sound");
        }
        if kind == AssetKind::Audio || (kind == AssetKind::Video && layer.audio) {
            ui.add(
                egui::Slider::new(&mut layer.volume, 0.0..=2.0)
                    .text("Volume")
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            );
        }

        if visual {
            ui.add_space(6.0);
            ui.label(RichText::new("Show on projectors").small().color(MUTED));
            ui.horizontal_wrapped(|ui| {
                for (index, (id, name)) in outputs.iter().enumerate() {
                    let mut enabled = layer.output_ids.contains(id);
                    let text =
                        RichText::new(name).color(OUTPUT_COLORS[index % OUTPUT_COLORS.len()]);
                    if ui.checkbox(&mut enabled, text).changed() {
                        if enabled {
                            layer.output_ids.push(*id);
                        } else {
                            layer.output_ids.retain(|o| o != id);
                        }
                    }
                }
            });
        }

        egui::CollapsingHeader::new("Timing")
            .default_open(true)
            .show(ui, |ui| {
                ui.add(
                    egui::DragValue::new(&mut layer.timeline_start)
                        .speed(0.1)
                        .range(0.0..=86400.0)
                        .prefix("Start ")
                        .suffix(" s"),
                );
                ui.add(
                    egui::DragValue::new(&mut layer.timeline_duration)
                        .speed(0.1)
                        .range(0.1..=86400.0)
                        .prefix("Duration ")
                        .suffix(" s"),
                );
                ui.add(
                    egui::DragValue::new(&mut layer.source_offset)
                        .speed(0.1)
                        .range(0.0..=86400.0)
                        .prefix("Media starts at ")
                        .suffix(" s"),
                );
            });
        ui.horizontal(|ui| {
            duplicate = ui.button("Duplicate  ⌘D").clicked();
            delete = ui
                .add(egui::Button::new("Delete layer").fill(DANGER.gamma_multiply(0.5)))
                .clicked();
        });
        self.keep_aspect = keep;
        if duplicate {
            self.duplicate_selected_layer();
        }
        if delete {
            self.delete_selected();
        }
    }

    fn stage_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Stage (whole canvas)");
        ui.label(
            RichText::new("Changing the canvas size never resizes projectors or media.")
                .small()
                .color(MUTED),
        );
        let mut width = self.project.stage.width;
        let mut height = self.project.stage.height;
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui
                .add(
                    egui::DragValue::new(&mut width)
                        .speed(4.0)
                        .range(16.0..=32768.0),
                )
                .changed();
            ui.label("×");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut height)
                        .speed(4.0)
                        .range(16.0..=32768.0),
                )
                .changed();
            ui.label("px");
        });
        let mut preset = None;
        egui::ComboBox::from_id_salt("stage_preset")
            .selected_text("Presets…")
            .show_ui(ui, |ui| {
                for (label, w, h) in STAGE_PRESETS {
                    if ui.selectable_label(false, label).clicked() {
                        preset = Some((w, h));
                    }
                }
            });
        if let Some((w, h)) = preset {
            (width, height, changed) = (w, h, true);
        }
        if changed {
            self.resize_stage(width, height);
        }
        let can_match = self.selected_asset_size().is_some();
        if ui
            .add_enabled(can_match, egui::Button::new("Fit stage to selected media"))
            .on_hover_text(
                "Makes the stage the media's exact size and fills it, e.g. a 3840×600 video \
                 made for two screens",
            )
            .on_disabled_hover_text("Select an image or video layer first")
            .clicked()
        {
            self.match_stage_to_selected();
        }
    }

    fn projectors_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Projectors");
        let mut remove = None;
        let online: HashMap<String, bool> = self
            .links
            .iter()
            .map(|l| (l.address.clone(), l.status.lock().unwrap().online))
            .collect();
        for (index, output) in self.project.outputs.iter().enumerate() {
            let color = OUTPUT_COLORS[index % OUTPUT_COLORS.len()];
            let address = output.player_address();
            let up = online.get(&address).copied().unwrap_or(false);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{}", index + 1))
                        .strong()
                        .color(color),
                );
                if ui
                    .selectable_label(
                        self.selected_output == Some(output.id),
                        format!(
                            "{} · {}×{}",
                            output.name, output.resolution[0], output.resolution[1]
                        ),
                    )
                    .clicked()
                {
                    self.selected_output = Some(output.id);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("×")
                        .on_hover_text("Remove projector")
                        .clicked()
                    {
                        remove = Some(output.id);
                    }
                    ui.label(
                        RichText::new(mapforge_core::player_host(&address))
                            .small()
                            .monospace()
                            .color(if up { LIVE } else { DANGER }),
                    )
                    .on_hover_text(if up {
                        "Player online"
                    } else {
                        "Player offline"
                    });
                });
            });
        }
        if let Some(id) = remove {
            self.remove_output(id);
        }
        if ui.button("+ Add projector…").clicked() {
            self.open_add_projector();
        }

        section(ui, "Layout calculator");
        let res = self
            .project
            .outputs
            .first()
            .map_or([1920, 1080], |o| o.resolution);
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut self.layout_count).range(1..=32));
            ui.label(format!(
                "projectors of {}×{} across {:.0}×{:.0}",
                res[0], res[1], self.project.stage.width, self.project.stage.height
            ));
        });
        let (width, _, overlap) = self.layout_overlap(self.layout_count);
        if overlap >= 0.0 {
            ui.label(
                RichText::new(format!(
                    "= {overlap:.0} px overlap per seam ({:.1}% of each projector)",
                    overlap / width * 100.0
                ))
                .color(LIVE),
            );
        } else {
            ui.label(
                RichText::new(format!(
                    "= {:.0} px gaps between projectors: add more projectors",
                    -overlap
                ))
                .color(DANGER),
            );
        }
        if ui
            .button("Apply layout")
            .on_hover_text(
                "Places the projectors at native size across the canvas and blends every \
                 overlap. Names and Player PCs are kept; nothing is stretched.",
            )
            .clicked()
        {
            self.layout_projectors(self.layout_count);
        }
        ui.checkbox(
            &mut self.auto_blend,
            "Auto-blend overlaps when moving projectors",
        );
        section(ui, "Calibrate");
        let identifying = self.project.test_pattern == TestPattern::Identify;
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new("🔢 Identify projectors").selected(identifying))
                .on_hover_text(
                    "Every projector shows its number, name, Player IP and resolution with a \
                     border and corner marks, so you can match and align them",
                )
                .clicked()
            {
                self.project.test_pattern = if identifying {
                    TestPattern::Off
                } else {
                    TestPattern::Identify
                };
            }
            egui::ComboBox::from_id_salt("test_pattern")
                .selected_text(pattern_name(self.project.test_pattern))
                .show_ui(ui, |ui| {
                    for pattern in [
                        TestPattern::Off,
                        TestPattern::Identify,
                        TestPattern::Grid,
                        TestPattern::White,
                        TestPattern::Gray,
                    ] {
                        ui.selectable_value(
                            &mut self.project.test_pattern,
                            pattern,
                            pattern_name(pattern),
                        );
                    }
                });
        });

        let Some(index) = self
            .project
            .outputs
            .iter()
            .position(|o| Some(o.id) == self.selected_output)
        else {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Select a projector to set its Player PC, resolution and blend.")
                    .color(MUTED),
            );
            return;
        };
        let color = OUTPUT_COLORS[index % OUTPUT_COLORS.len()];
        // Screens the projector's Player PC reports, to pick from by name.
        let address = self.project.outputs[index].player_address();
        let detected: Vec<DisplayInfo> = self
            .links
            .iter()
            .find(|l| l.address == address)
            .and_then(|l| {
                let status = l.status.lock().unwrap();
                status.state.as_ref().map(|s| s.displays.clone())
            })
            .unwrap_or_default();
        let output = &mut self.project.outputs[index];
        section(ui, &format!("Projector {}", index + 1));
        let mut geometry_changed = false;
        egui::Grid::new("output_info")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Name").color(color));
                ui.add(egui::TextEdit::singleline(&mut output.name).desired_width(170.0));
                ui.end_row();
                ui.label("Player PC");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut output.player)
                            .desired_width(120.0)
                            .hint_text("192.168.1.50"),
                    )
                    .on_hover_text(
                        "IP address of the PC this projector is plugged into (port 4777)",
                    );
                    this_pc_button(ui, &mut output.player);
                });
                ui.end_row();
                ui.label("Resolution");
                ui.horizontal(|ui| {
                    let mut changed = ui
                        .add(egui::DragValue::new(&mut output.resolution[0]).range(320..=8192))
                        .changed();
                    ui.label("×");
                    changed |= ui
                        .add(egui::DragValue::new(&mut output.resolution[1]).range(200..=8192))
                        .changed();
                    egui::ComboBox::from_id_salt("resolution_preset")
                        .selected_text("")
                        .width(20.0)
                        .show_ui(ui, |ui| {
                            for (w, h) in RESOLUTION_PRESETS {
                                if ui.selectable_label(false, format!("{w} × {h}")).clicked() {
                                    output.resolution = [w, h];
                                    changed = true;
                                }
                            }
                        });
                    if changed {
                        geometry_changed = true;
                    }
                });
                ui.end_row();
                ui.label("Player display");
                egui::ComboBox::from_id_salt("player_display")
                    .width(220.0)
                    .selected_text(match output.display_index {
                        None => "Automatic (next free screen)".to_owned(),
                        Some(display) => detected.iter().find(|d| d.index == display).map_or_else(
                            || {
                                if detected.is_empty() {
                                    format!("Display {}", display + 1)
                                } else {
                                    format!("Display {} (not connected)", display + 1)
                                }
                            },
                            DisplayInfo::label,
                        ),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut output.display_index,
                            None,
                            "Automatic (next free screen)",
                        )
                        .on_hover_text(
                            "Fills the next extra screen, left to right; never the main screen",
                        );
                        if detected.is_empty() {
                            // Player offline or older: offer plain numbers.
                            for display in 0..16_u32 {
                                ui.selectable_value(
                                    &mut output.display_index,
                                    Some(display),
                                    format!("Display {}", display + 1),
                                );
                            }
                        }
                        for display in &detected {
                            ui.selectable_value(
                                &mut output.display_index,
                                Some(display.index),
                                display.label(),
                            );
                        }
                    });
                ui.end_row();
            });
        ui.label(
            RichText::new(
                "The list shows the screens connected to that Player PC. Or, on the Player PC, \
                 drag a preview window onto its projector and double-click it. Use Identify to \
                 check.",
            )
            .small()
            .color(MUTED),
        );
        ui.label(RichText::new("Position on the canvas").small().color(MUTED));
        egui::Grid::new("output_geometry")
            .num_columns(4)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                ui.label("X");
                geometry_changed |= ui
                    .add(egui::DragValue::new(&mut output.stage_x).speed(1.0))
                    .changed();
                ui.label("Y");
                geometry_changed |= ui
                    .add(egui::DragValue::new(&mut output.stage_y).speed(1.0))
                    .changed();
                ui.end_row();
            });
        ui.label(
            RichText::new(format!(
                "Size on the canvas is fixed at its native {} × {} px — move it, never stretch it.",
                output.resolution[0], output.resolution[1]
            ))
            .small()
            .color(MUTED),
        );

        section(ui, "Corner correction");
        ui.label(
            RichText::new(
                "Move each output corner as a percentage of the projector image. Use the Grid \
                 test pattern while aligning the physical surface.",
            )
            .small()
            .color(MUTED),
        );
        let (warp_preview, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 150.0),
            egui::Sense::hover(),
        );
        let warp_preview = warp_preview.shrink(10.0);
        ui.painter()
            .rect_filled(warp_preview, 3.0, Color32::from_gray(8));
        ui.painter().rect_stroke(
            warp_preview,
            3.0,
            egui::Stroke::new(1.0_f32, Color32::from_gray(70)),
            egui::StrokeKind::Inside,
        );
        let mut corner_positions = output.warp_corners.map(|corner| {
            egui::pos2(
                warp_preview.left() + corner[0] * warp_preview.width(),
                warp_preview.top() + corner[1] * warp_preview.height(),
            )
        });
        ui.painter().add(egui::Shape::closed_line(
            corner_positions.to_vec(),
            egui::Stroke::new(2.0_f32, color),
        ));
        for (corner_index, position) in corner_positions.iter_mut().enumerate() {
            let handle = egui::Rect::from_center_size(*position, egui::vec2(16.0, 16.0));
            let response = ui
                .interact(
                    handle,
                    egui::Id::new(("warp_corner", output.id, corner_index)),
                    egui::Sense::drag(),
                )
                .on_hover_cursor(egui::CursorIcon::Crosshair);
            if let Some(pointer) = response
                .interact_pointer_pos()
                .filter(|_| response.dragged())
            {
                output.warp_corners[corner_index] = [
                    ((pointer.x - warp_preview.left()) / warp_preview.width()).clamp(-0.5, 1.5),
                    ((pointer.y - warp_preview.top()) / warp_preview.height()).clamp(-0.5, 1.5),
                ];
                *position = pointer;
            }
            ui.painter().circle_filled(*position, 6.0, Color32::WHITE);
            ui.painter()
                .circle_stroke(*position, 6.0, egui::Stroke::new(2.0_f32, color));
        }
        egui::Grid::new("warp_corners")
            .num_columns(3)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                for (corner, label) in ["Top left", "Top right", "Bottom right", "Bottom left"]
                    .into_iter()
                    .enumerate()
                {
                    ui.label(label);
                    ui.add(
                        egui::DragValue::new(&mut output.warp_corners[corner][0])
                            .range(-0.5..=1.5)
                            .speed(0.001)
                            .custom_formatter(|v, _| format!("X {:.1}%", v * 100.0)),
                    );
                    ui.add(
                        egui::DragValue::new(&mut output.warp_corners[corner][1])
                            .range(-0.5..=1.5)
                            .speed(0.001)
                            .custom_formatter(|v, _| format!("Y {:.1}%", v * 100.0)),
                    );
                    ui.end_row();
                }
            });
        if ui.button("Reset corners").clicked() {
            output.warp_corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        }

        section(ui, "Polygon mask");
        ui.checkbox(&mut output.mask.enabled, "Enable hard-edged mask");
        ui.label(
            RichText::new(
                "Points are percentages of the physical output. Keep them ordered around the \
                 visible area; everything outside the polygon is black.",
            )
            .small()
            .color(MUTED),
        );
        let mut remove_mask_point = None;
        let mask_point_count = output.mask.points.len();
        for (point_index, point) in output.mask.points.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}", point_index + 1));
                ui.add(
                    egui::DragValue::new(&mut point[0])
                        .range(0.0..=1.0)
                        .speed(0.001)
                        .custom_formatter(|v, _| format!("X {:.1}%", v * 100.0)),
                );
                ui.add(
                    egui::DragValue::new(&mut point[1])
                        .range(0.0..=1.0)
                        .speed(0.001)
                        .custom_formatter(|v, _| format!("Y {:.1}%", v * 100.0)),
                );
                if mask_point_count > 3 && ui.small_button("×").clicked() {
                    remove_mask_point = Some(point_index);
                }
            });
        }
        if let Some(point_index) = remove_mask_point {
            output.mask.points.remove(point_index);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(output.mask.points.len() < 64, egui::Button::new("+ Point"))
                .clicked()
            {
                let previous = output.mask.points.last().copied().unwrap_or([0.5, 0.5]);
                output.mask.points.push(previous);
            }
            if ui.button("Reset mask").clicked() {
                output.mask.points = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
            }
        });

        section(ui, "Edge blending");
        ui.label(
            RichText::new(
                "Where projectors overlap their light adds up, so each one fades out across \
                 the overlap. Blend width = how much of this projector fades.",
            )
            .small()
            .color(MUTED),
        );
        let (w, h) = (output.stage_width, output.stage_height);
        percent_slider(ui, &mut output.blend.left, w, "Left");
        percent_slider(ui, &mut output.blend.right, w, "Right");
        percent_slider(ui, &mut output.blend.top, h, "Top");
        percent_slider(ui, &mut output.blend.bottom, h, "Bottom");
        ui.add(
            egui::Slider::new(&mut output.blend.luminance, 0.2..=0.8)
                .text("Seam level")
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
        )
        .on_hover_text(
            "Light from each projector at the middle of the overlap. 50% means both add up to \
             exactly 100%. Raise it if the seam looks dark, lower it if it looks bright.",
        );
        ui.add(egui::Slider::new(&mut output.blend.power, 0.5..=5.0).text("Curve"))
            .on_hover_text("Shape of the fade: 1 = linear, 2 = smooth S-curve (default)");
        ui.add(egui::Slider::new(&mut output.blend.gamma, 1.0..=3.0).text("Projector gamma"))
            .on_hover_text("Display gamma of the projector, usually 2.2");
        blend_curve_preview(ui, &output.blend);

        section(ui, "Brightness & black level");
        ui.add(
            egui::Slider::new(&mut output.color.brightness, 0.3..=1.0)
                .text("Brightness")
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
        )
        .on_hover_text("Dim a brighter projector so all projectors match");
        ui.add(
            egui::Slider::new(&mut output.color.black_lift, 0.0..=0.2)
                .text("Black lift")
                .custom_formatter(|v, _| format!("{:.1}%", v * 100.0)),
        )
        .on_hover_text(
            "Projector black is never fully black, so the overlap shows a brighter grey band \
             on dark content. Lift the non-overlap area until the band disappears.",
        );
        if ui.button("Reset blend & colour").clicked() {
            output.blend.luminance = 0.5;
            output.blend.power = 2.0;
            output.blend.gamma = 2.2;
            output.color = OutputColor::default();
        }
        if geometry_changed && self.auto_blend {
            self.project.auto_blend();
        }
        if ui
            .button("Auto-blend all overlaps now")
            .on_hover_text("Sets every projector's blend widths to its real overlap")
            .clicked()
        {
            self.project.auto_blend();
        }
    }

    fn lan_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "LAN & media");
        ui.label(
            RichText::new(
                "Each Player PC needs its own copy of the media. Test the network, then send \
                 the files before the show.",
            )
            .small()
            .color(MUTED),
        );
        let assets = self.project.assets.clone();
        let total_bytes: u64 = assets.iter().filter_map(|a| a.size_bytes).sum();
        ui.label(
            RichText::new(format!(
                "Show media: {} file(s), {}",
                assets.len(),
                format_bytes(total_bytes)
            ))
            .small(),
        );
        let mut send_to = Vec::new();
        for link in &self.links {
            let (online, latency) = {
                let status = link.status.lock().unwrap();
                (status.online, status.latency_ms)
            };
            egui::Frame::group(ui.style()).fill(CANVAS).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(if online { LIVE } else { DANGER }));
                    ui.label(RichText::new(&link.address).monospace().strong());
                    if let Some(ms) = latency {
                        ui.label(RichText::new(format!("{ms:.0} ms")).small().color(MUTED));
                    }
                });
                let lan = link.lan.lock().unwrap();
                let busy = lan.busy.clone();
                if let Some(speed) = lan.speed_mbps {
                    ui.label(
                        RichText::new(format!(
                            "Speed {speed:.0} Mbit/s — {}",
                            network::speed_hint(speed)
                        ))
                        .small(),
                    );
                    if total_bytes > 0 {
                        let seconds = total_bytes as f64 * 8.0 / (speed * 1_000_000.0);
                        ui.label(
                            RichText::new(format!(
                                "All media would take about {}",
                                format_duration(seconds)
                            ))
                            .small()
                            .color(MUTED),
                        );
                    }
                }
                if let Some(media) = &lan.media {
                    let ok = media.ready == media.total;
                    ui.label(
                        RichText::new(format!(
                            "Media on this PC: {} / {}",
                            media.ready, media.total
                        ))
                        .small()
                        .color(if ok { LIVE } else { WARN }),
                    );
                }
                if let Some(transfer) = &lan.transfer {
                    let fraction = transfer.done as f32 / transfer.total.max(1) as f32;
                    ui.add(egui::ProgressBar::new(fraction).text(format!(
                        "{} ({}/{}) · {:.0} Mbit/s",
                        transfer.name,
                        transfer.file,
                        transfer.files,
                        transfer.mbps()
                    )));
                }
                if !lan.message.is_empty() {
                    ui.label(RichText::new(&lan.message).small().color(MUTED));
                }
                drop(lan);
                ui.horizontal(|ui| {
                    let idle = busy.is_none() && online;
                    if ui
                        .add_enabled(idle, egui::Button::new("Test speed").small())
                        .on_hover_text("Sends 64 MB to measure real LAN throughput")
                        .clicked()
                    {
                        network::speed_test(link.host(), &link.lan, &self.ctx);
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Check media").small())
                        .clicked()
                    {
                        network::check_media(link.host(), &link.lan, &self.ctx);
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Send media").small())
                        .on_hover_text("Copies missing files; the Player verifies each checksum")
                        .clicked()
                    {
                        send_to.push((link.host(), link.lan.clone()));
                    }
                    if let Some(busy) = &busy {
                        ui.spinner();
                        ui.label(RichText::new(busy).small());
                    }
                });
            });
        }
        if !send_to.is_empty() {
            // The Player only accepts files that belong to the show it has.
            self.sync_now();
            for (host, lan) in send_to {
                network::send_media(host, assets.clone(), &lan, &self.ctx);
            }
        }
    }

    /// The Player that reports itself as master, if any.
    fn master_link(&self) -> Option<&PlayerLink> {
        self.links.iter().find(|l| {
            let status = l.status.lock().unwrap();
            status.state.as_ref().and_then(|s| s.role) == Some(PlayerRole::Master)
        })
    }

    /// Each Player's job, as chosen on that PC when it first started.
    fn show_pc_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Show PCs");
        ui.label(
            RichText::new(
                "Each Player asks once, on first start, whether it is the master or a sub. \
                 The master takes the iPad's commands and passes them to the subs; subs copy \
                 the show and media from the master. Every Player keeps the last show and \
                 opens it when it starts.",
            )
            .small()
            .color(MUTED),
        );
        let mut masters = 0;
        egui::Grid::new("show_pcs")
            .num_columns(3)
            .spacing([10.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for link in &self.links {
                    let status = link.status.lock().unwrap();
                    let names: Vec<&str> = self
                        .project
                        .outputs
                        .iter()
                        .filter(|o| o.player_address() == link.address)
                        .map(|o| o.name.as_str())
                        .collect();
                    ui.label(RichText::new(link.host()).monospace());
                    ui.label(RichText::new(names.join(", ")).small().color(MUTED));
                    let (text, color) = match status.state.as_ref().filter(|_| status.online) {
                        None => ("offline".to_owned(), DANGER),
                        Some(state) => match state.role {
                            Some(PlayerRole::Master) => {
                                masters += 1;
                                let start = if state.autoplay {
                                    "starts by itself"
                                } else {
                                    "waits for Play"
                                };
                                (format!("Master · {start}"), LIVE)
                            }
                            Some(PlayerRole::Sub) => ("Sub".to_owned(), ACCENT),
                            None => ("not set up: choose on that PC".to_owned(), WARN),
                        },
                    };
                    ui.label(RichText::new(text).color(color));
                    ui.end_row();
                }
            });
        if masters > 1 {
            ui.label(
                RichText::new("More than one PC is set as master. Choose Sub on the others.")
                    .color(WARN),
            );
        } else if masters == 0 && self.links.len() > 1 && self.online {
            ui.label(RichText::new("No PC is set as master yet.").color(WARN));
        }
        let show = &mut self.project.show;
        let mut master_only = !show.audio_everywhere;
        ui.checkbox(&mut master_only, "Play sound on the master PC only")
            .on_hover_text("Stops the same music playing from two PCs with an echo");
        show.audio_everywhere = !master_only;
        if ui
            .button("Send show to all Players")
            .on_hover_text("Each Player saves it and opens it next time it starts")
            .clicked()
        {
            self.sync_now();
            self.status = format!("Show sent to {} Player(s)", self.links.len());
        }
    }

    fn controller_ui(&mut self, ui: &mut egui::Ui) {
        self.show_pc_ui(ui);
        section(ui, "iPad / phone page");
        let player = self
            .master_link()
            .or(self.links.first())
            .map_or("127.0.0.1:4777".to_string(), |l| l.address.clone());
        let url = controller_url(&player);
        ui.horizontal(|ui| {
            ui.label("Open on a phone or tablet:");
            ui.label(RichText::new(&url).monospace().color(ACCENT));
        });
        ui.horizontal(|ui| {
            if ui.button("Open in browser").clicked() {
                open_in_browser(&url);
            }
            if ui.button("Copy link").clicked() {
                ui.ctx().copy_text(url.clone());
                self.status = "Controller link copied".into();
            }
        });
        if !self.online {
            ui.label(
                RichText::new("The Player is offline, so the page won't load yet.").color(WARN),
            );
        }

        section(ui, "Page");
        let settings = &mut self.project.controller;
        egui::Grid::new("controller_page")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Title");
                ui.add(egui::TextEdit::singleline(&mut settings.title).desired_width(260.0));
                ui.end_row();
                ui.label("Note");
                ui.add(
                    egui::TextEdit::multiline(&mut settings.note)
                        .desired_rows(2)
                        .desired_width(260.0)
                        .hint_text("Instructions for the operator (optional)"),
                );
                ui.end_row();
                ui.label("Accent colour");
                ui.color_edit_button_srgb(&mut settings.accent);
                ui.end_row();
                ui.label("Button columns");
                ui.add(egui::Slider::new(&mut settings.columns, 1..=4));
                ui.end_row();
            });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut settings.show_transport, "Play / pause / stop");
            ui.checkbox(&mut settings.show_scenes, "Scene buttons");
            ui.checkbox(&mut settings.show_blackout, "Blackout");
            ui.checkbox(&mut settings.show_volume, "Volume");
        });

        section(ui, "Scene buttons");
        ui.label(
            RichText::new("Tapping a scene button goes live with it, like pressing its F-key.")
                .small()
                .color(MUTED),
        );
        egui::Grid::new("controller_scenes")
            .num_columns(4)
            .spacing([10.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Show").small().color(MUTED));
                ui.label(RichText::new("Scene").small().color(MUTED));
                ui.label(RichText::new("Button text").small().color(MUTED));
                ui.label(RichText::new("Colour").small().color(MUTED));
                ui.end_row();
                for (index, scene) in self.project.scenes.iter_mut().enumerate() {
                    let mut visible = !scene.button.hidden;
                    if ui.checkbox(&mut visible, "").changed() {
                        scene.button.hidden = !visible;
                    }
                    let key = scene_hotkey(index, scene);
                    ui.label(format!("{key} {}", scene.name));
                    ui.add(
                        egui::TextEdit::singleline(&mut scene.button.label)
                            .desired_width(140.0)
                            .hint_text(&scene.name),
                    );
                    color_choice(ui, &mut scene.button.color);
                    ui.end_row();
                    for region in &mut scene.loops {
                        let mut visible = !region.button.hidden;
                        if ui.checkbox(&mut visible, "").changed() {
                            region.button.hidden = !visible;
                        }
                        ui.label(
                            RichText::new(format!("   ⟲ {} {}", region.hotkey, region.name))
                                .color(LOOP_COLOR),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut region.button.label)
                                .desired_width(140.0)
                                .hint_text(&region.name),
                        );
                        color_choice(ui, &mut region.button.color);
                        ui.end_row();
                    }
                    for cue in &mut scene.cues {
                        let mut visible = !cue.button.hidden;
                        if ui.checkbox(&mut visible, "").changed() {
                            cue.button.hidden = !visible;
                        }
                        ui.label(
                            RichText::new(format!("   ◆ {} {}", cue.hotkey, cue.name)).color(WARN),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut cue.button.label)
                                .desired_width(140.0)
                                .hint_text(&cue.name),
                        );
                        color_choice(ui, &mut cue.button.color);
                        ui.end_row();
                    }
                }
            });
        if !self.live_sync {
            ui.label(
                RichText::new("Live sync is off: click “Send changes” to update the page.")
                    .small()
                    .color(WARN),
            );
        }
    }

    // --- Preview -------------------------------------------------------------

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, EditMode::Layers, "🖼 Edit layers");
            ui.selectable_value(&mut self.mode, EditMode::Projectors, "📽 Edit projectors");
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "{} · {:.0} × {:.0} · {} projector(s)",
                    self.scene().name,
                    self.project.stage.width,
                    self.project.stage.height,
                    self.project.outputs.len()
                ))
                .color(MUTED),
            );
            if self.project.test_pattern != TestPattern::Off {
                ui.label(
                    RichText::new(format!(
                        "Test pattern: {}",
                        pattern_name(self.project.test_pattern)
                    ))
                    .color(WARN),
                );
            }
        });
        ui.label(
            RichText::new(match self.mode {
                EditMode::Layers => {
                    "Drag to move · drag the corner to resize (Shift = free aspect) · Delete removes"
                }
                EditMode::Projectors => {
                    "Drag projectors to position them (they keep their native size) · overlaps are \
                     blended automatically"
                }
            })
            .small()
            .color(MUTED),
        );

        let stage_w = self.project.stage.width;
        let stage_h = self.project.stage.height;
        let available = ui.available_size().max(egui::vec2(120.0, 80.0));
        let (outer, background) = ui.allocate_exact_size(available, egui::Sense::click());
        let margin = 16.0;
        let scale = ((outer.width() - margin * 2.0) / stage_w)
            .min((outer.height() - margin * 2.0) / stage_h)
            .max(0.001);
        let rect = egui::Rect::from_center_size(
            outer.center(),
            egui::vec2(stage_w * scale, stage_h * scale),
        );
        if background.clicked() {
            match self.mode {
                EditMode::Layers => self.selected_layer = None,
                EditMode::Projectors => self.selected_output = None,
            }
        }
        let painter = ui.painter_at(outer);
        painter.rect_filled(outer, 0.0, CANVAS);
        painter.rect_filled(rect, 0.0, STAGE);
        let stage_painter = ui.painter_at(rect);
        let to_screen =
            |x: f32, y: f32| egui::pos2(rect.left() + x * scale, rect.top() + y * scale);
        let shift = ui.input(|i| i.modifiers.shift);
        let snap_distance = 8.0 / scale;
        let layers_mode = self.mode == EditMode::Layers;

        // Media dragged from the library lands where it is dropped.
        if let Some(payload) = background.dnd_release_payload::<MediaDrag>() {
            let start = self.playhead_seconds;
            self.add_layer_at(payload.0, start, None);
            if let (Some(pointer), Some(layer)) =
                (background.interact_pointer_pos(), self.selected_layer_mut())
            {
                layer.x = (pointer.x - rect.left()) / scale - layer.width / 2.0;
                layer.y = (pointer.y - rect.top()) / scale - layer.height / 2.0;
            }
        }
        let audio_assets: HashSet<Uuid> = self
            .project
            .assets
            .iter()
            .filter(|a| a.kind == AssetKind::Audio)
            .map(|a| a.id)
            .collect();
        let playhead = self.playhead_seconds;

        // Layers on screen at the playhead, bottom first. A selected layer
        // outside its time is shown faintly so it can still be edited.
        let scene = &mut self.project.scenes[self.scene];
        for (index, layer) in scene.layers.iter_mut().enumerate() {
            if audio_assets.contains(&layer.asset_id) {
                continue;
            }
            let on_screen = layer.active_at(playhead);
            let selected = self.selected_layer == Some(layer.id);
            if !on_screen && !selected {
                continue;
            }
            let layer_rect = egui::Rect::from_min_size(
                to_screen(layer.x, layer.y),
                egui::vec2(layer.width * scale, layer.height * scale),
            );
            let tint =
                Color32::WHITE.gamma_multiply(layer.opacity * if on_screen { 1.0 } else { 0.25 });
            match self.thumbs.textures.get(&layer.asset_id) {
                Some(tex) => {
                    stage_painter.image(
                        tex.id(),
                        layer_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        tint,
                    );
                }
                None => {
                    stage_painter.rect_filled(
                        layer_rect,
                        3.0,
                        OUTPUT_COLORS[index % 3].gamma_multiply(0.5 * layer.opacity),
                    );
                    stage_painter.text(
                        layer_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &layer.name,
                        egui::FontId::proportional(13.0),
                        Color32::WHITE,
                    );
                }
            }
            if !layers_mode {
                continue;
            }
            let body = ui
                .interact(
                    layer_rect.intersect(outer),
                    egui::Id::new(("layer", layer.id)),
                    egui::Sense::click_and_drag(),
                )
                .on_hover_cursor(egui::CursorIcon::Grab);
            if body.clicked() || body.drag_started() {
                self.selected_layer = Some(layer.id);
            }
            if body.dragged() && body.drag_delta() != egui::Vec2::ZERO {
                let delta = body.drag_delta() / scale;
                layer.x += delta.x;
                layer.y += delta.y;
                layer.x = snap(layer.x, layer.width, stage_w, snap_distance);
                layer.y = snap(layer.y, layer.height, stage_h, snap_distance);
            }
            if self.selected_layer == Some(layer.id) {
                painter.rect_stroke(
                    layer_rect,
                    0.0,
                    egui::Stroke::new(2.0_f32, Color32::WHITE),
                    egui::StrokeKind::Outside,
                );
                if let Some(pointer) =
                    resize_handle(ui, &painter, layer_rect, ("layer_size", layer.id))
                        .filter(|p| p.distance(layer_rect.right_bottom()) > 0.5)
                {
                    let aspect = layer.height / layer.width.max(1.0);
                    layer.width = ((pointer.x - layer_rect.left()) / scale).max(8.0);
                    layer.height = if shift {
                        ((pointer.y - layer_rect.top()) / scale).max(8.0)
                    } else {
                        layer.width * aspect
                    };
                }
            }
        }

        // Overlap zones between projectors.
        let outputs = &self.project.outputs;
        for i in 0..outputs.len() {
            for j in i + 1..outputs.len() {
                let (a, b) = (&outputs[i], &outputs[j]);
                let overlap = output_rect(a).intersect(output_rect(b));
                if !overlap.is_positive() {
                    continue;
                }
                let blended = overlap_is_blended(a, b, overlap);
                let zone = egui::Rect::from_min_max(
                    to_screen(overlap.left(), overlap.top()),
                    to_screen(overlap.right(), overlap.bottom()),
                );
                let (fill, label) = if blended {
                    (
                        Color32::from_rgba_unmultiplied(255, 200, 0, 28),
                        format!("blend {:.0} px", overlap.width().min(overlap.height())),
                    )
                } else {
                    (
                        Color32::from_rgba_unmultiplied(255, 60, 60, 70),
                        "NOT BLENDED · 2× bright".to_string(),
                    )
                };
                stage_painter.rect_filled(zone, 0.0, fill);
                stage_painter.text(
                    zone.center_bottom() - egui::vec2(0.0, 8.0),
                    egui::Align2::CENTER_BOTTOM,
                    label,
                    egui::FontId::proportional(11.0),
                    if blended { WARN } else { Color32::WHITE },
                );
            }
        }

        // Projector frames.
        let mut geometry_changed = false;
        for (index, output) in self.project.outputs.iter_mut().enumerate() {
            let color = OUTPUT_COLORS[index % OUTPUT_COLORS.len()];
            let frame = egui::Rect::from_min_size(
                to_screen(output.stage_x, output.stage_y),
                egui::vec2(output.stage_width * scale, output.stage_height * scale),
            );
            let selected = !layers_mode && self.selected_output == Some(output.id);
            if !layers_mode {
                painter.rect_filled(
                    frame,
                    0.0,
                    color.gamma_multiply(if selected { 0.18 } else { 0.08 }),
                );
                let body = ui
                    .interact(
                        frame.intersect(outer),
                        egui::Id::new(("output", output.id)),
                        egui::Sense::click_and_drag(),
                    )
                    .on_hover_cursor(egui::CursorIcon::Grab);
                if body.clicked() || body.drag_started() {
                    self.selected_output = Some(output.id);
                }
                if body.dragged() && body.drag_delta() != egui::Vec2::ZERO {
                    let delta = body.drag_delta() / scale;
                    output.stage_x = snap(
                        output.stage_x + delta.x,
                        output.stage_width,
                        stage_w,
                        snap_distance,
                    );
                    output.stage_y = snap(
                        output.stage_y + delta.y,
                        output.stage_height,
                        stage_h,
                        snap_distance,
                    );
                    geometry_changed = true;
                }
            }
            painter.rect_stroke(
                frame,
                0.0,
                egui::Stroke::new(if selected { 3.0_f32 } else { 1.5_f32 }, color),
                egui::StrokeKind::Inside,
            );
            painter.text(
                frame.left_top() + egui::vec2(8.0, 6.0),
                egui::Align2::LEFT_TOP,
                format!("{} · {}", index + 1, output.name),
                egui::FontId::proportional(12.0),
                color,
            );
        }
        if geometry_changed && self.auto_blend {
            self.project.auto_blend();
        }
    }
}

impl eframe::App for ProducerApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let now = Instant::now();
        let delta = now.duration_since(self.last_frame).as_secs_f64();
        self.last_frame = now;

        self.refresh_links();
        self.thumbs.poll(ctx);
        for asset in &self.project.assets {
            self.thumbs.request(asset, ctx);
        }
        self.handle_input(ctx);
        self.scene = self.scene.min(self.project.scenes.len() - 1);
        self.update_playhead(delta);
        if self.player_transport() == Some(Transport::Playing) {
            ctx.request_repaint_after(Duration::from_millis(33));
        }

        let title = format!(
            "MapForge Producer {} — {}{}",
            mapforge_core::update::current_version(),
            self.project.name,
            if self.dirty() { " •" } else { "" }
        );
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }

        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| self.top_bar(ui));
        egui::TopBottomPanel::bottom("status")
            .exact_height(26.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(
                                "F1–F12 scenes · Space play/pause · Esc stop · Enter exit loop · M cue · L loop · B blackout · ⌘Z undo",
                            )
                            .small()
                            .color(MUTED),
                        );
                    });
                });
            });
        egui::TopBottomPanel::bottom("timeline")
            .resizable(true)
            .default_height(190.0)
            .min_height(120.0)
            .show(ctx, |ui| self.timeline_ui(ui));
        egui::SidePanel::left("library")
            .resizable(true)
            .default_width(270.0)
            .show(ctx, |ui| self.left_panel(ui));
        egui::SidePanel::right("properties")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| self.right_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.preview_ui(ui));

        let blocked = self
            .dirty()
            .then_some("Save your show first (Ctrl+S), then update.");
        if mapforge_core::update::bubble::show(ctx, &mut self.updater, blocked) {
            // The installer is open and replaces the programs once we close.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            let screen = ctx.screen_rect();
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                format!(
                    "Drop images or videos to add them to “{}”",
                    self.scene().name
                ),
                egui::FontId::proportional(24.0),
                Color32::WHITE,
            );
        }

        if self.show_controller {
            let mut open = true;
            egui::Window::new("📱 Web controller")
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| self.controller_ui(ui));
            self.show_controller = open;
        }

        if let Some(mut spec) = self.new_projector.take() {
            let mut open = true;
            let mut add = false;
            egui::Window::new("Add projector")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::Grid::new("new_projector")
                        .num_columns(2)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Name");
                            ui.text_edit_singleline(&mut spec.name);
                            ui.end_row();
                            ui.label("Resolution");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut spec.width).range(320..=8192));
                                ui.label("×");
                                ui.add(egui::DragValue::new(&mut spec.height).range(200..=8192));
                                egui::ComboBox::from_id_salt("new_projector_res")
                                    .selected_text("")
                                    .width(20.0)
                                    .show_ui(ui, |ui| {
                                        for (w, h) in RESOLUTION_PRESETS {
                                            if ui
                                                .selectable_label(false, format!("{w} × {h}"))
                                                .clicked()
                                            {
                                                spec.width = w;
                                                spec.height = h;
                                            }
                                        }
                                    });
                            });
                            ui.end_row();
                            ui.label("Player PC IP");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut spec.player)
                                        .desired_width(120.0)
                                        .hint_text("192.168.1.50"),
                                )
                                .on_hover_text("The PC on the LAN this projector is connected to");
                                this_pc_button(ui, &mut spec.player);
                            });
                            ui.end_row();
                            ui.label("Overlap with previous");
                            ui.add(
                                egui::Slider::new(&mut spec.overlap_percent, 0.0..=40.0)
                                    .suffix("%")
                                    .fixed_decimals(1),
                            );
                            ui.end_row();
                        });
                    ui.checkbox(&mut spec.expand_stage, "Grow the canvas width to fit it");
                    let right = self.project.outputs.last().map_or(0.0, |o| {
                        o.stage_x + o.stage_width - spec.width as f32 * spec.overlap_percent / 100.0
                    }) + spec.width as f32;
                    if !spec.expand_stage && right > self.project.stage.width {
                        ui.label(
                            RichText::new(format!(
                                "It will reach {right:.0} px, past the {:.0} px canvas.",
                                self.project.stage.width
                            ))
                            .small()
                            .color(WARN),
                        );
                    }
                    ui.label(
                        RichText::new(
                            "It is placed to the right of the last projector at its native \
                             size. Existing projectors are not moved or resized.",
                        )
                        .small()
                        .color(MUTED),
                    );
                    ui.horizontal(|ui| {
                        add = ui
                            .add(
                                egui::Button::new(RichText::new("Add projector").strong())
                                    .fill(ACCENT),
                            )
                            .clicked();
                    });
                });
            if add {
                self.add_output(&spec);
            } else if open {
                self.new_projector = Some(spec);
            }
        }

        if self.confirm_new {
            egui::Window::new("Discard unsaved changes?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("The current show has changes that are not saved.");
                    ui.horizontal(|ui| {
                        if ui.button("Save first").clicked() {
                            self.save(false);
                            if !self.dirty() {
                                self.new_project();
                            }
                            self.confirm_new = false;
                        }
                        if ui
                            .add(egui::Button::new("Discard").fill(DANGER.gamma_multiply(0.6)))
                            .clicked()
                        {
                            self.new_project();
                            self.confirm_new = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_new = false;
                        }
                    });
                });
        }

        self.project.lock_output_sizes();
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        self.history.track(&self.project, pointer_down);
        self.auto_sync();
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn apply_theme(ctx: &egui::Context) {
    // A dark control room UI regardless of the system light/dark setting.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = CANVAS;
    style.visuals.faint_bg_color = Color32::from_rgb(26, 30, 40);
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.7);
    style.visuals.hyperlink_color = ACCENT;
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(5);
    }
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(38, 43, 56);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(50, 57, 74);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(9.0, 4.0);
    style.spacing.slider_width = 150.0;
    ctx.set_style(style);
}

fn color_choice(ui: &mut egui::Ui, color: &mut Option<[u8; 3]>) {
    ui.horizontal(|ui| {
        let mut custom = color.is_some();
        if ui.checkbox(&mut custom, "").changed() {
            *color = custom.then_some([28, 36, 54]);
        }
        if let Some(color) = color {
            ui.color_edit_button_srgb(color);
        }
    });
}

fn end_action_name(action: EndAction) -> &'static str {
    match action {
        EndAction::Loop => "Loop the scene",
        EndAction::Hold => "Hold the last frame",
        EndAction::Stop => "Stop (black)",
        EndAction::Next => "Go to the next scene",
    }
}

fn kind_icon(kind: &AssetKind) -> &'static str {
    match kind {
        AssetKind::Image => "🖼",
        AssetKind::Video => "🎬",
        AssetKind::Audio => "🎵",
        AssetKind::Unknown => "?",
    }
}

fn format_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1_000_000_000.0;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1_000_000.0)
    }
}

fn format_duration_precise(seconds: f64) -> String {
    let total = seconds.max(0.0);
    format!("{}:{:04.1}", (total / 60.0) as u64, total % 60.0)
}

/// Payload when a media thumbnail is dragged onto the timeline or preview.
#[derive(Clone, Copy)]
struct MediaDrag(Uuid);

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(title.to_uppercase())
            .small()
            .strong()
            .color(MUTED),
    );
    ui.separator();
}

fn pattern_name(pattern: TestPattern) -> &'static str {
    match pattern {
        TestPattern::Off => "Off (show content)",
        TestPattern::Grid => "Alignment grid",
        TestPattern::White => "White",
        TestPattern::Gray => "50% gray",
        TestPattern::Identify => "Identify (numbers)",
    }
}

/// Edits a stage-pixel feather width as a percentage of `total`.
fn percent_slider(ui: &mut egui::Ui, value: &mut f32, total: f32, label: &str) {
    let mut percent = *value / total * 100.0;
    let response = ui.add(
        egui::Slider::new(&mut percent, 0.0..=50.0)
            .text(label)
            .suffix("%")
            .fixed_decimals(1),
    );
    if response.changed() {
        *value = (percent / 100.0 * total).clamp(0.0, total);
    }
    response.on_hover_text(format!("{:.0} stage px", *value));
}

/// Plots this projector fading out, its neighbour fading in, and their sum.
fn blend_curve_preview(ui: &mut egui::Ui, blend: &EdgeBlend) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 90.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, CANVAS);
    let plot = rect.shrink2(egui::vec2(8.0, 12.0));
    let y_for = |light: f32| plot.bottom() - light / 1.2 * plot.height();
    painter.line_segment(
        [
            egui::pos2(plot.left(), y_for(1.0)),
            egui::pos2(plot.right(), y_for(1.0)),
        ],
        egui::Stroke::new(1.0_f32, Color32::from_gray(60)),
    );
    let steps = 48;
    let mut fade_out = Vec::new();
    let mut fade_in = Vec::new();
    let mut total = Vec::new();
    let (mut min_total, mut max_total) = (f32::MAX, f32::MIN);
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let x = plot.left() + t * plot.width();
        let a = blend.light(1.0 - t);
        let b = blend.light(t);
        min_total = min_total.min(a + b);
        max_total = max_total.max(a + b);
        fade_out.push(egui::pos2(x, y_for(a)));
        fade_in.push(egui::pos2(x, y_for(b)));
        total.push(egui::pos2(x, y_for(a + b)));
    }
    painter.add(egui::Shape::line(
        fade_out,
        egui::Stroke::new(2.0_f32, OUTPUT_COLORS[0]),
    ));
    painter.add(egui::Shape::line(
        fade_in,
        egui::Stroke::new(2.0_f32, OUTPUT_COLORS[1]),
    ));
    painter.add(egui::Shape::line(
        total,
        egui::Stroke::new(2.0_f32, Color32::WHITE),
    ));
    let flat = (max_total - 1.0).abs() < 0.03 && (min_total - 1.0).abs() < 0.03;
    painter.text(
        rect.left_top() + egui::vec2(8.0, 2.0),
        egui::Align2::LEFT_TOP,
        if flat {
            "Overlap brightness: 100% ✔".to_string()
        } else {
            format!(
                "Overlap brightness: {:.0}–{:.0}%",
                min_total * 100.0,
                max_total * 100.0
            )
        },
        egui::FontId::proportional(11.0),
        if flat { LIVE } else { WARN },
    );
}

/// Draws a corner handle and returns the pointer position while it is dragged.
fn resize_handle(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    id: impl std::hash::Hash,
) -> Option<egui::Pos2> {
    let handle = egui::Rect::from_center_size(rect.right_bottom(), egui::vec2(14.0, 14.0));
    let response = ui
        .interact(handle, egui::Id::new(id), egui::Sense::drag())
        .on_hover_cursor(egui::CursorIcon::ResizeNwSe);
    painter.rect_filled(handle, 2.0, Color32::WHITE);
    painter.rect_stroke(
        handle,
        2.0,
        egui::Stroke::new(1.5_f32, ACCENT),
        egui::StrokeKind::Inside,
    );
    if response.dragged() {
        response.interact_pointer_pos()
    } else {
        None
    }
}

/// Snaps a position to the stage edges or centre when within `distance`.
fn snap(position: f32, size: f32, total: f32, distance: f32) -> f32 {
    for target in [0.0, total - size, (total - size) / 2.0] {
        if (position - target).abs() < distance {
            return target;
        }
    }
    position
}

fn output_rect(output: &ProjectorOutput) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(output.stage_x, output.stage_y),
        egui::vec2(output.stage_width, output.stage_height),
    )
}

/// True when both projectors fade across at least half of their overlap.
fn overlap_is_blended(a: &ProjectorOutput, b: &ProjectorOutput, overlap: egui::Rect) -> bool {
    let (first, second) = if a.stage_x <= b.stage_x {
        (a, b)
    } else {
        (b, a)
    };
    if overlap.height() >= overlap.width() {
        let need = overlap.width() * 0.5;
        first.blend.right >= need && second.blend.left >= need
    } else {
        let (upper, lower) = if a.stage_y <= b.stage_y {
            (a, b)
        } else {
            (b, a)
        };
        let need = overlap.height() * 0.5;
        upper.blend.bottom >= need && lower.blend.top >= need
    }
}

/// Sets a projector to the Player on this computer: a one-PC setup needs no
/// network. Shows "this PC" when it already is.
fn this_pc_button(ui: &mut egui::Ui, player: &mut String) {
    let host = mapforge_core::player_host(&mapforge_core::normalize_player_address(player));
    if host == "127.0.0.1" || host == "localhost" {
        ui.label(RichText::new("this PC").small().color(LIVE))
            .on_hover_text("The Player on this computer. No LAN needed.");
    } else if ui
        .small_button("This PC")
        .on_hover_text("One PC only: use the Player on this computer, no LAN needed")
        .clicked()
    {
        *player = "127.0.0.1".into();
    }
}

/// The controller runs on the Player, so use the Player's host, or this
/// computer's LAN address when the Player is local.
fn controller_url(player_address: &str) -> String {
    let host = player_address
        .rsplit_once(':')
        .map_or(player_address, |(host, _)| host);
    let host = if host == "127.0.0.1" || host == "localhost" || host.is_empty() {
        local_ip().unwrap_or_else(|| "127.0.0.1".into())
    } else {
        host.to_string()
    };
    format!("http://{host}:8080")
}

fn local_ip() -> Option<String> {
    // Connecting a UDP socket sends nothing; it only picks the outgoing interface.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let result = ProcessCommand::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let result = ProcessCommand::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let result = ProcessCommand::new("xdg-open").arg(url).spawn();
    let _ = result;
}

fn asset_size(asset: &Asset) -> (f32, f32) {
    (
        asset.width.unwrap_or(1600).max(1) as f32,
        asset.height.unwrap_or(900).max(1) as f32,
    )
}

fn asset_details(asset: &Asset) -> String {
    let size = match (asset.width, asset.height) {
        (Some(w), Some(h)) => format!(" · {w}×{h}"),
        _ => String::new(),
    };
    match asset.kind {
        AssetKind::Video => format!(
            "Video{size}{}",
            asset
                .duration_seconds
                .map(|d| format!(" · {}", format_duration(d)))
                .unwrap_or_default()
        ),
        AssetKind::Image => format!("Image{size}"),
        AssetKind::Audio => format!(
            "Sound{}",
            asset
                .duration_seconds
                .map(|d| format!(" · {}", format_duration(d)))
                .unwrap_or_default()
        ),
        AssetKind::Unknown => "Unknown".into(),
    }
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.round() as u64;
    if total >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            total / 3600,
            (total / 60) % 60,
            total % 60
        )
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

fn format_timecode(seconds: f64) -> String {
    let total_frames = (seconds.max(0.0) * 30.0).round() as u64;
    let frames = total_frames % 30;
    let total_seconds = total_frames / 30;
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        total_seconds / 3600,
        (total_seconds / 60) % 60,
        total_seconds % 60,
        frames
    )
}

fn probe_media(path: &Path) -> (Option<u32>, Option<u32>, Option<f64>) {
    let output = tool_command("ffprobe")
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
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([960.0, 600.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "MapForge Producer",
        options,
        Box::new(|cc| {
            let mut app = ProducerApp::new(cc);
            // A show file passed on the command line (or by double-click) opens directly.
            if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) {
                app.open_path(path);
            }
            Ok(Box::new(app))
        }),
    )
}
