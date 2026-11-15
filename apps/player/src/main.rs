// Release builds on Windows open no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod displays;
mod relay;
mod setup;

use displays::DisplayMonitor;
use eframe::egui;
use mapforge_core::{
    net::unix_time_ms, player_host, scene_hotkey, tool_command, Asset, AssetKind, AssetStatus,
    Command, DisplayInfo, EndAction, Envelope, Layer, LoopRegion, PlayerRole, PlayerState,
    ProjectorOutput, Scene, ShowProject, TestPattern, Transport, WarpMode, CONTROLLER_PORT,
    DEFAULT_PLAYER_PORT, PROTOCOL_VERSION,
};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use relay::Relay;
use serde::Deserialize;
use setup::PlayerSettings;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const CONTROLLER_HTML: &str = include_str!("controller.html");
const VIDEO_FPS: f64 = 30.0;
/// Largest texture side the GPU accepts, read from OpenGL at start-up. Media
/// is decoded at its own resolution so panoramas spanning several projectors
/// stay sharp; only media wider or taller than this is scaled down, because
/// uploading a larger texture would crash the renderer.
static TEXTURE_LIMIT: AtomicU32 = AtomicU32::new(8192);
const AUDIO_RATE: u32 = 48_000;

fn texture_limit() -> u32 {
    TEXTURE_LIMIT.load(Ordering::Relaxed)
}

/// Decode size for media of `width` × `height`: unchanged when it fits the
/// GPU, otherwise shrunk to fit while keeping its shape. Both sides are even,
/// as video codecs and FFmpeg's scaler prefer.
fn fit_within(width: u32, height: u32, limit: u32) -> (usize, usize) {
    let (width, height) = (width.max(2) as f64, height.max(2) as f64);
    let scale = (limit as f64 / width).min(limit as f64 / height).min(1.0);
    let even = |v: f64| ((v * scale / 2.0).round() as usize * 2).max(2);
    (even(width), even(height))
}
/// How long before a scene ends its next pass is started in the background.
const PREROLL_SECONDS: f64 = 2.0;

/// The show clock: a position that advances in real time while running.
#[derive(Default)]
struct Clock {
    base: f64,
    since: Option<Instant>,
}

impl Clock {
    fn position(&self) -> f64 {
        self.base + self.since.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }

    fn run(&mut self) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }

    fn hold(&mut self) {
        self.base = self.position();
        self.since = None;
    }

    fn set(&mut self, seconds: f64) {
        self.base = seconds;
        if self.since.is_some() {
            self.since = Some(Instant::now());
        }
    }
}

#[derive(Default)]
struct Runtime {
    state: PlayerState,
    project: Option<ShowProject>,
    seen: HashSet<Uuid>,
    order: VecDeque<Uuid>,
    clock: Clock,
    /// Times each loop has jumped back since the scene was prepared.
    loop_plays: HashMap<Uuid, u32>,
    /// Loops let go by the operator; re-armed when the clock goes back before them.
    released: HashSet<Uuid>,
    /// Clock position at the previous scheduler tick, to detect crossing a loop end.
    prev_position: f64,
    /// Outputs assigned to this PC; all outputs when `None`.
    outputs: Option<Vec<Uuid>>,
    /// This PC's address in the show, as Producer knows it.
    address: Option<String>,
    /// Master or sub, chosen on first start.
    settings: PlayerSettings,
    /// The TCP port this Player listens on.
    port: u16,
    /// This PC's LAN addresses, to find its own projectors in a show.
    local_ips: Vec<String>,
    /// A controller edit to hand back to Producer on its next state poll.
    project_update_pending: bool,
    /// Sub: how the link to the master is doing.
    link_note: String,
    scheduled: Option<ScheduledStart>,
}

#[derive(Clone, Copy)]
struct ScheduledStart {
    unix_ms: u64,
    scene_id: Uuid,
    seconds: f64,
    /// Cued while the show was playing: the scene on air keeps playing
    /// until the start time, then the cued media (pre-rolled in
    /// `MediaPool::cue`) cuts in.
    cut: bool,
    /// An exit-loop arrived while this cut was waiting. It is meant for the
    /// cued position, so it is applied when the cut happens; applying it at
    /// once would be undone when the cut re-arms the loops.
    release: bool,
}

fn is_loopback(address: &str) -> bool {
    let host = player_host(address);
    host.starts_with("127.") || host == "localhost"
}

impl Runtime {
    fn role(&self) -> Option<PlayerRole> {
        self.settings.role
    }

    /// The master runs the show for the iPad and the other Players.
    fn is_master(&self) -> bool {
        self.role() == Some(PlayerRole::Master)
    }

    /// Whether a Player address in the show is this PC.
    fn is_me(&self, address: &str) -> bool {
        let port = address
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok());
        // A loopback address is "this PC" as Producer sees it, which in a
        // multi-PC show is the master. A sub taking it would draw the
        // master's projectors instead of its own.
        let loopback_is_me = is_loopback(address) && self.role() != Some(PlayerRole::Sub);
        port == Some(self.port)
            && (loopback_is_me || self.local_ips.contains(&player_host(address)))
    }

    /// This PC's address in the show: as Producer sent it, or found from
    /// this PC's IP when the show came from the master.
    fn own_address(&self) -> Option<String> {
        if self.address.is_some() {
            return self.address.clone();
        }
        let project = self.project.as_ref()?;
        project.players().into_iter().find(|a| self.is_me(a))
    }

    /// Players the master passes commands on to. A loopback address only
    /// means "this PC" to Producer, so it can't be reached from a remote master.
    fn followers(&self) -> Vec<String> {
        let Some(project) = self.project.as_ref().filter(|_| self.is_master()) else {
            return Vec::new();
        };
        let own = self.own_address();
        let own_loopback = own.as_deref().is_some_and(is_loopback);
        project
            .players()
            .into_iter()
            .filter(|p| {
                Some(p) != own.as_ref() && !self.is_me(p) && (own_loopback || !is_loopback(p))
            })
            .collect()
    }

    /// Sound plays on the master only unless the show says otherwise.
    fn plays_audio(&self) -> bool {
        match self.role() {
            Some(PlayerRole::Sub) => self
                .project
                .as_ref()
                .is_some_and(|p| p.show.audio_everywhere),
            _ => true,
        }
    }

    fn snapshot(&mut self) -> PlayerState {
        let position = self.display_position();
        self.state.position_seconds = position;
        self.state.loop_name = self
            .scene()
            .and_then(|s| {
                s.loops
                    .iter()
                    .find(|l| self.armed(l) && l.contains(position))
            })
            .map(|l| l.name.clone());
        self.state.server_time_unix_ms = unix_time_ms();
        self.state.role = self.settings.role;
        self.state.autoplay = self.is_master() && self.settings.autoplay;
        self.state.scheduled_start_unix_ms = self.scheduled.map(|start| start.unix_ms);
        self.state.clone()
    }

    fn armed(&self, region: &LoopRegion) -> bool {
        !self.released.contains(&region.id)
            && (region.count == 0
                || self.loop_plays.get(&region.id).copied().unwrap_or(0) + 1 < region.count)
    }

    fn reset_loops(&mut self, position: f64) {
        self.loop_plays.clear();
        self.released.clear();
        self.prev_position = position;
    }

    /// The loop whose end the clock has just passed, if it should repeat.
    fn crossed_loop(&self, position: f64) -> Option<LoopRegion> {
        let prev = self.prev_position;
        self.scene()?
            .loops
            .iter()
            .filter(|l| self.armed(l) && l.contains(prev) && position >= l.end)
            .min_by(|a, b| a.end.total_cmp(&b.end))
            .cloned()
    }

    /// Jumps back to the start of a loop once the clock passes its end.
    fn handle_loops(&mut self, media: &MediaPool) {
        let position = self.clock.position();
        if self.state.transport != Transport::Playing {
            self.prev_position = position;
            return;
        }
        if let Some(region) = self.crossed_loop(position) {
            *self.loop_plays.entry(region.id).or_default() += 1;
            if let (Some(project), Some(scene_id)) = (&self.project, self.state.scene_id) {
                media.sync(project, scene_id, true, region.start);
            }
            let overflow = (position - region.end).clamp(0.0, 0.05);
            self.clock = Clock {
                base: region.start + overflow,
                since: Some(Instant::now()),
            };
            self.state.message = format!("Looping {}", region.name);
            self.prev_position = region.start + overflow;
            return;
        }
        // Released loops re-arm once playback is back before them.
        if let Some(scene) = self.scene() {
            let before: Vec<Uuid> = scene
                .loops
                .iter()
                .filter(|l| position < l.start)
                .map(|l| l.id)
                .collect();
            self.released.retain(|id| !before.contains(id));
        }
        self.prev_position = position;
    }

    /// Lets every loop around the current position play through.
    fn release_loops(&mut self) {
        if let Some(start) = self.scheduled.as_mut().filter(|s| s.cut) {
            start.release = true;
            self.state.message = "Leaving loop at the cue".into();
            return;
        }
        self.release_loops_at(self.clock.position());
    }

    fn release_loops_at(&mut self, position: f64) {
        let ids: Vec<(Uuid, String)> = self
            .scene()
            .map(|s| {
                s.loops
                    .iter()
                    .filter(|l| l.contains(position))
                    .map(|l| (l.id, l.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.state.message = match ids.first() {
            Some((_, name)) => format!("Leaving loop {name}"),
            None => "Not in a loop".into(),
        };
        self.released.extend(ids.into_iter().map(|(id, _)| id));
    }

    /// Where the clock will jump next and when: the end of the current loop,
    /// or the scene's end when it loops or moves to the next scene.
    fn next_jump(&self) -> Option<(Uuid, f64, f64)> {
        let position = self.clock.position();
        let scene = self.scene()?;
        if let Some(region) = scene
            .loops
            .iter()
            .filter(|l| self.armed(l) && l.contains(position))
            .min_by(|a, b| a.end.total_cmp(&b.end))
        {
            return Some((scene.id, region.start, region.end));
        }
        let end = scene.duration();
        match scene.end_action {
            EndAction::Loop => Some((scene.id, self.visual_loop_start(scene), end)),
            EndAction::Next => {
                let project = self.project.as_ref()?;
                let index = project.scenes.iter().position(|s| s.id == scene.id)?;
                Some((project.scenes.get(index + 1)?.id, 0.0, end))
            }
            EndAction::Hold | EndAction::Stop => None,
        }
    }

    /// The clock as the outputs should show it. Between passing the scene
    /// end and the scheduler acting on it, this already wraps (loop) or
    /// clamps (hold), so no frame is ever drawn past the last clip.
    fn display_position(&self) -> f64 {
        let position = self.clock.position();
        if self.state.transport == Transport::Playing {
            if let Some(region) = self.crossed_loop(position) {
                return region.start + (position - region.end);
            }
        }
        let Some(scene) = self.scene() else {
            return position;
        };
        let end = scene.duration();
        if end <= 0.0 || position < end {
            return position;
        }
        match scene.end_action {
            EndAction::Loop => {
                let start = self.visual_loop_start(scene);
                let span = (end - start).max(0.001);
                start + (position - end).rem_euclid(span)
            }
            EndAction::Hold => end - 0.001,
            EndAction::Stop | EndAction::Next => position,
        }
    }

    /// Whole-scene loops skip a leading interval with no picture and no
    /// sound on repeats; a music intro before the first picture is content
    /// and plays on every pass. An explicitly drawn timeline loop still
    /// uses its exact authored start.
    fn visual_loop_start(&self, scene: &Scene) -> f64 {
        let Some(project) = &self.project else {
            return 0.0;
        };
        scene
            .layers
            .iter()
            .filter(|layer| {
                project
                    .assets
                    .iter()
                    .find(|asset| asset.id == layer.asset_id)
                    .is_some_and(|asset| {
                        matches!(
                            asset.kind,
                            AssetKind::Image | AssetKind::Video | AssetKind::Audio
                        )
                    })
            })
            .map(|layer| layer.timeline_start.max(0.0))
            .min_by(f64::total_cmp)
            .unwrap_or(0.0)
    }

    fn scene(&self) -> Option<&Scene> {
        let id = self.state.scene_id?;
        self.project.as_ref()?.scenes.iter().find(|s| s.id == id)
    }

    /// Loads a scene paused at `seconds`, with media pre-rolled.
    fn prepare(&mut self, media: &MediaPool, scene_id: Uuid, seconds: f64) -> bool {
        let Some(project) = &self.project else {
            self.state.message = "No project loaded".into();
            return false;
        };
        let Some(scene) = project.scenes.iter().find(|s| s.id == scene_id) else {
            self.state.message = "Unknown scene".into();
            return false;
        };
        self.state.message = format!("{} ready", scene.name);
        media.sync(project, scene_id, true, seconds);
        self.state.scene_id = Some(scene_id);
        self.state.transport = Transport::Ready;
        self.clock = Clock {
            base: seconds,
            since: None,
        };
        self.reset_loops(seconds);
        true
    }

    /// Forgets a scheduled start and stops any media pre-rolled for it.
    fn cancel_scheduled(&mut self, media: &MediaPool) {
        self.scheduled = None;
        media.drop_cue();
    }

    fn play(&mut self, media: &MediaPool) {
        self.cancel_scheduled(media);
        if self.state.scene_id.is_none() {
            let Some(first) = self.project.as_ref().and_then(|p| p.scenes.first()) else {
                self.state.message = "No project loaded".into();
                return;
            };
            let id = first.id;
            if !self.prepare(media, id, 0.0) {
                return;
            }
        }
        self.clock.run();
        self.state.transport = Transport::Playing;
        self.state.message = "Playing".into();
    }

    fn stop(&mut self, media: &MediaPool) {
        self.cancel_scheduled(media);
        if let (Some(project), Some(scene_id)) = (&self.project, self.state.scene_id) {
            media.sync(project, scene_id, true, 0.0);
        }
        self.clock = Clock::default();
        self.reset_loops(0.0);
        self.state.transport = Transport::Stopped;
        self.state.message = "Stopped".into();
    }

    fn schedule(&mut self, media: &MediaPool, scene_id: Uuid, seconds: f64, unix_ms: u64) {
        if self.state.transport == Transport::Playing {
            // Preparing now would stop the scene on air and leave the
            // outputs black and silent for the whole lead time. Instead the
            // cued media is pre-rolled beside it and cuts in at the start.
            let Some(project) = &self.project else {
                self.state.message = "No project loaded".into();
                return;
            };
            if !project.scenes.iter().any(|s| s.id == scene_id) {
                self.state.message = "Unknown scene".into();
                return;
            }
            media.cue(project, scene_id, seconds);
            self.scheduled = Some(ScheduledStart {
                unix_ms,
                scene_id,
                seconds,
                cut: true,
                release: false,
            });
            self.state.start_error_ms = None;
            self.state.message = format!("Cued; starts at {unix_ms}");
            return;
        }
        if self.prepare(media, scene_id, seconds) {
            media.drop_cue();
            self.scheduled = Some(ScheduledStart {
                unix_ms,
                scene_id,
                seconds,
                cut: false,
                release: false,
            });
            self.state.start_error_ms = None;
            self.state.message = format!("Prepared; starts at {unix_ms}");
        }
    }

    fn handle_scheduled_start(&mut self, media: &MediaPool) {
        let Some(start) = self.scheduled else {
            return;
        };
        let now = unix_time_ms();
        if now < start.unix_ms {
            return;
        }
        self.scheduled = None;
        let late_ms = now.saturating_sub(start.unix_ms) as f64;
        let position = start.seconds + late_ms / 1000.0;
        // The media was started at `start.seconds`. When the start is late,
        // the decoders and sound skip ahead to the show clock by themselves,
        // so nothing is restarted for it.
        if start.cut {
            let Some(project) = &self.project else {
                return;
            };
            if !project.scenes.iter().any(|s| s.id == start.scene_id) {
                self.state.message = "Unknown scene".into();
                return;
            }
            media.cut_to_cue(project, start.scene_id, start.seconds);
            self.state.scene_id = Some(start.scene_id);
            self.reset_loops(position);
            if start.release {
                self.release_loops_at(position);
            }
        } else if self.state.scene_id != Some(start.scene_id) {
            if !self.prepare(media, start.scene_id, start.seconds) {
                return;
            }
        } else if let Some(project) = &self.project {
            media.sync(project, start.scene_id, false, start.seconds);
        }
        self.clock = Clock {
            base: position,
            since: Some(Instant::now()),
        };
        self.state.transport = Transport::Playing;
        self.state.start_error_ms = Some(late_ms);
        self.state.message = format!("Playing · scheduled start {late_ms:.1} ms late");
    }

    /// Applies the scene's end action once the clock passes its last clip.
    fn handle_scene_end(&mut self, media: &MediaPool) {
        // A cue is about to cut in: the outputs already wrap or hold this
        // scene's end, and Stop or Next here would cancel the cue.
        if self.state.transport != Transport::Playing
            || self.scheduled.is_some_and(|start| start.cut)
        {
            return;
        }
        let Some(scene) = self.scene() else {
            return;
        };
        let end = scene.duration();
        if end <= 0.0 || self.clock.position() < end {
            return;
        }
        let (scene_id, action, loop_start) =
            (scene.id, scene.end_action, self.visual_loop_start(scene));
        match action {
            EndAction::Loop => {
                if let Some(project) = &self.project {
                    media.sync(project, scene_id, true, loop_start);
                }
                // Keep the few milliseconds past the end so timing never drifts.
                let overflow = (self.clock.position() - end).clamp(0.0, 0.05);
                self.clock = Clock {
                    base: loop_start + overflow,
                    since: Some(Instant::now()),
                };
                self.reset_loops(loop_start + overflow);
            }
            EndAction::Hold => {
                self.clock = Clock {
                    base: (end - 0.001).max(0.0),
                    since: None,
                };
                self.state.transport = Transport::Paused;
                self.state.message = "Holding last frame".into();
            }
            EndAction::Stop => self.stop(media),
            EndAction::Next => {
                let next = self.project.as_ref().and_then(|p| {
                    let index = p.scenes.iter().position(|s| s.id == scene_id)?;
                    p.scenes.get(index + 1).map(|s| s.id)
                });
                match next {
                    Some(id) => {
                        if self.prepare(media, id, 0.0) {
                            self.play(media);
                        }
                    }
                    None => self.stop(media),
                }
            }
        }
    }
}

/// One picture ready for the GPU. It is already in egui's pixel layout, so
/// the UI thread hands it to the texture without converting or copying it.
#[derive(Clone)]
struct DecodedFrame {
    image: Arc<egui::ColorImage>,
    sequence: u64,
}

/// What this PC shows: the projectors it draws (all when `None`) and the
/// quality chosen in the Player window. Together with the GPU's limit this
/// decides how much of each layer is decoded, and how large.
#[derive(Clone, Default)]
struct DecodeView {
    outputs: Option<Vec<Uuid>>,
    max_side: u32,
}

/// Everything a layer's decoders were started with; a change restarts them.
#[derive(Clone, PartialEq)]
struct LayerSpec {
    asset: Asset,
    looping: bool,
    source_offset: f64,
    timeline_start: f64,
    audio: bool,
    /// The part of the media this PC's projectors show, in media pixels
    /// (x, y, width, height). `None` decodes the whole picture. A panorama
    /// spanning several PCs is decoded by each PC only where it is shown, so
    /// it plays at full resolution without decoding the parts other PCs show.
    crop: Option<[u32; 4]>,
    /// The crop as texture coordinates of the whole media.
    uv: [f32; 4],
    /// No projector on this PC shows this layer, so no picture is decoded.
    hidden: bool,
    /// Longest side after decoding: the GPU's limit and the quality setting.
    max_side: u32,
    /// How many projector pixels the decoded part (`crop`, or the whole
    /// picture) covers on this PC's outputs. Media larger than that is
    /// shrunk while decoding, with a better filter than the GPU's.
    shown: Option<[u32; 2]>,
}

impl LayerSpec {
    fn new(layer: &Layer, asset: &Asset, outputs: &[&ProjectorOutput], max_side: u32) -> Self {
        let mut asset = asset.clone();
        asset.path = resolve_media(&asset).to_string_lossy().to_string();
        let visual = matches!(asset.kind, AssetKind::Image | AssetKind::Video);
        let shown = visible_part(layer, outputs);
        let crop = match (shown, asset.width, asset.height) {
            (Some(part), Some(w), Some(h)) if visual => media_crop(layer, part, w, h),
            _ => None,
        };
        let shown_px =
            shown.and_then(|_| shown_size(layer, outputs, crop, asset.width, asset.height));
        let uv = match (crop, asset.width, asset.height) {
            (Some([x, y, w, h]), Some(mw), Some(mh)) => [
                x as f32 / mw as f32,
                y as f32 / mh as f32,
                (x + w) as f32 / mw as f32,
                (y + h) as f32 / mh as f32,
            ],
            _ => [0.0, 0.0, 1.0, 1.0],
        };
        Self {
            asset,
            looping: layer.looping,
            source_offset: layer.source_offset,
            timeline_start: layer.timeline_start,
            audio: layer.audio,
            crop,
            uv,
            hidden: visual && shown.is_none(),
            max_side: max_side.min(texture_limit()),
            shown: shown_px.filter(|_| visual),
        }
    }

    /// Time into the media at show time `seconds`, counting on through
    /// loops: the clock the decoders pace their frames against.
    fn media_clock(&self, seconds: f64) -> f64 {
        self.source_offset + (seconds - self.timeline_start).max(0.0)
    }

    /// Where in the file `media_clock` time `t` is: wrapped for looping
    /// clips, and no further than the end for the rest, where a video
    /// holds its last frame and sound has finished.
    fn file_time(&self, t: f64) -> f64 {
        match self.asset.duration_seconds {
            Some(d) if self.looping && d > 0.0 => t % d,
            Some(d) if d > 0.0 => t.min(d),
            _ => t,
        }
    }

    /// Media position for show time `seconds`, as a place in the file.
    #[cfg(test)]
    fn media_time(&self, seconds: f64) -> f64 {
        self.file_time(self.media_clock(seconds))
    }
}

/// How many projector pixels the decoded part of a layer covers: its share
/// of the layer's stage size, times the projector pixels per stage unit of
/// the sharpest output on this PC that shows the layer.
fn shown_size(
    layer: &Layer,
    outputs: &[&ProjectorOutput],
    crop: Option<[u32; 4]>,
    width: Option<u32>,
    height: Option<u32>,
) -> Option<[u32; 2]> {
    let (fx, fy) = match (crop, width, height) {
        (Some([_, _, w, h]), Some(mw), Some(mh)) if mw > 0 && mh > 0 => {
            (w as f32 / mw as f32, h as f32 / mh as f32)
        }
        _ => (1.0, 1.0),
    };
    let (mut px, mut py) = (0.0_f32, 0.0_f32);
    for output in outputs.iter().filter(|o| layer.output_ids.contains(&o.id)) {
        if output.stage_width > 0.0 && output.stage_height > 0.0 {
            px = px.max(output.resolution[0] as f32 / output.stage_width);
            py = py.max(output.resolution[1] as f32 / output.stage_height);
        }
    }
    if px <= 0.0 || py <= 0.0 {
        return None;
    }
    let even_up = |v: f32| ((v.ceil() as u32).div_ceil(2) * 2).max(2);
    Some([
        even_up(fx * layer.width * px),
        even_up(fy * layer.height * py),
    ])
}

/// Decode size for a `part` of the media shown on `shown` projector pixels:
/// never larger than the media itself (FFmpeg never enlarges), no larger
/// than it appears on screen, and within the quality and GPU limit.
fn decode_size(part: (u32, u32), shown: Option<[u32; 2]>, max_side: u32) -> (usize, usize) {
    let (w, h) = match shown {
        Some([sw, sh]) => (part.0.min(sw), part.1.min(sh)),
        None => part,
    };
    fit_within(w, h, max_side)
}

/// The crop `[x, y, w, h]`, given in pixels of media measured as `asset`,
/// in pixels of the picture FFmpeg actually delivers (`frame`). The two
/// differ for phone videos stored sideways with a rotation flag, which
/// FFmpeg turns upright before cropping, or for shows probed before the
/// flag was read. Edges stay even; `None` when the crop is unusable.
fn crop_in_frame(crop: [u32; 4], asset: (u32, u32), frame: (u32, u32)) -> Option<[u32; 4]> {
    let [x, y, w, h] = crop;
    let (aw, ah) = (asset.0.max(1) as u64, asset.1.max(1) as u64);
    let (fw, fh) = (frame.0 as u64, frame.1 as u64);
    let lo = |v: u32, from: u64, to: u64| (v as u64 * to / from) as u32 / 2 * 2;
    let hi = |v: u32, from: u64, to: u64| ((v as u64 * to).div_ceil(from) as u32).div_ceil(2) * 2;
    let (x0, x1) = (lo(x, aw, fw), hi(x + w, aw, fw).min(frame.0));
    let (y0, y1) = (lo(y, ah, fh), hi(y + h, ah, fh).min(frame.1));
    if x1 < x0 + 2 || y1 < y0 + 2 {
        return None;
    }
    if x0 == 0 && y0 == 0 && x1 >= frame.0 && y1 >= frame.1 {
        return None;
    }
    Some([x0, y0, x1 - x0, y1 - y0])
}

/// The stage rectangle (left, top, right, bottom) where `outputs` show the
/// layer: the layer's rectangle cut to the box around the projectors routed
/// to it. `None` when none of them shows it.
fn visible_part(layer: &Layer, outputs: &[&ProjectorOutput]) -> Option<[f32; 4]> {
    let mut shown: Option<[f32; 4]> = None;
    for output in outputs
        .iter()
        .filter(|o| layer.output_ids.contains(&o.id))
    {
        let rect = [
            output.stage_x,
            output.stage_y,
            output.stage_x + output.stage_width,
            output.stage_y + output.stage_height,
        ];
        shown = Some(match shown {
            None => rect,
            Some(s) => [
                s[0].min(rect[0]),
                s[1].min(rect[1]),
                s[2].max(rect[2]),
                s[3].max(rect[3]),
            ],
        });
    }
    let s = shown?;
    let part = [
        s[0].max(layer.x),
        s[1].max(layer.y),
        s[2].min(layer.x + layer.width),
        s[3].min(layer.y + layer.height),
    ];
    (part[2] > part[0] && part[3] > part[1]).then_some(part)
}

/// The media pixels (x, y, width, height) behind the stage rectangle `part`
/// of the layer, with a small margin, on even pixel boundaries. `None` when
/// that is the whole picture anyway.
fn media_crop(layer: &Layer, part: [f32; 4], width: u32, height: u32) -> Option<[u32; 4]> {
    const MARGIN: f32 = 4.0;
    if layer.width <= 0.0 || layer.height <= 0.0 || width < 2 || height < 2 {
        return None;
    }
    let (w, h) = (width as f32, height as f32);
    let span = |from: f32, to: f32, origin: f32, size: f32, pixels: f32| {
        let lo = ((from - origin) / size * pixels - MARGIN).floor().max(0.0);
        let hi = ((to - origin) / size * pixels + MARGIN).ceil().min(pixels);
        // Even edges, as video codecs and FFmpeg's scaler prefer.
        let lo = (lo as u32) / 2 * 2;
        let hi = ((hi as u32).div_ceil(2) * 2).min(pixels as u32).max(lo + 2);
        (lo, hi)
    };
    let (x0, x1) = span(part[0], part[2], layer.x, layer.width, w);
    let (y0, y1) = span(part[1], part[3], layer.y, layer.height, h);
    if x0 == 0 && y0 == 0 && x1 >= width && y1 >= height {
        return None;
    }
    Some([x0, y0, (x1 - x0).min(width - x0), (y1 - y0).min(height - y0)])
}

/// How far behind the show a decoder may fall before it starts FFmpeg again
/// further on, rather than catching up by skipping frames.
const RESYNC_SECONDS: f64 = 1.0;
/// While a decoder catches up by skipping frames it still shows one at
/// least this often, so the picture keeps moving.
const MAX_HOLD: Duration = Duration::from_millis(100);

/// What to do with a decoded frame, given when it is due and where the
/// show is, both as media clock (`LayerSpec::media_clock`).
#[derive(Debug, PartialEq)]
enum Pace {
    /// Not due yet; due in this many seconds.
    Wait(f64),
    Show,
    /// The next frame is already due, so this one is skipped.
    Drop,
    /// Too far behind to catch up by skipping frames.
    Resync,
}

fn pace(due: f64, clock: f64, held_long: bool) -> Pace {
    let late = clock - due;
    if late > RESYNC_SECONDS {
        Pace::Resync
    } else if late >= 1.0 / VIDEO_FPS && !held_long {
        Pace::Drop
    } else if late >= 0.0 {
        Pace::Show
    } else {
        Pace::Wait(-late)
    }
}

/// How one FFmpeg run of a video decoder ended.
enum Run {
    Cancelled,
    Ended,
    Resync,
}

#[derive(Default)]
struct Decoder {
    latest: Mutex<Option<DecodedFrame>>,
    error: Mutex<Option<String>>,
    /// What is being decoded and at what size, for the Player window.
    info: Mutex<String>,
    cancelled: AtomicBool,
    /// True while the show clock is inside this clip and playing.
    active: AtomicBool,
    /// The show's place in the media (`LayerSpec::media_clock`) and when it
    /// was read, set by the scheduler while the layer plays. Frames are
    /// shown when the show reaches them, so the picture stays with the
    /// clock, the sound and the other PCs however long FFmpeg takes to
    /// start or seek, and however fast this PC decodes.
    clock: Mutex<Option<(f64, Instant)>>,
}

impl Decoder {
    /// Every frame gets a number no other frame ever had, so a restarted
    /// decoder's first frame is never mistaken for the old one.
    fn publish(&self, image: egui::ColorImage) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        *self.latest.lock().unwrap() = Some(DecodedFrame {
            image: Arc::new(image),
            sequence: NEXT.fetch_add(1, Ordering::Relaxed),
        });
    }

    fn fail(&self, message: String) {
        *self.error.lock().unwrap() = Some(message);
    }

    fn set_clock(&self, media: f64) {
        *self.clock.lock().unwrap() = Some((media, Instant::now()));
    }

    /// The show's place in the media now, carried on between the
    /// scheduler's updates.
    fn clock(&self) -> Option<f64> {
        self.clock
            .lock()
            .unwrap()
            .map(|(media, at)| media + at.elapsed().as_secs_f64())
    }

    /// Describes the decode for the Player window: the source size, the part
    /// of it this PC shows, and whether it was scaled down.
    fn describe(
        &self,
        spec: &LayerSpec,
        source: (u32, u32),
        part: Option<[u32; 4]>,
        decoded: (usize, usize),
    ) {
        let (sw, sh) = source;
        let (cw, ch) = part.map_or((sw, sh), |[_, _, w, h]| (w, h));
        let part = match part {
            Some(_) => format!("{cw}×{ch} part of {sw}×{sh}"),
            None => format!("{sw}×{sh}"),
        };
        let to_display = spec.shown.is_some_and(|[w, h]| w < cw || h < ch)
            && decoded == decode_size((cw, ch), spec.shown, u32::MAX);
        let size = if decoded == (cw as usize, ch as usize) {
            "full resolution".to_owned()
        } else if to_display {
            format!("scaled to display size {}×{}", decoded.0, decoded.1)
        } else {
            format!("scaled down to {}×{}", decoded.0, decoded.1)
        };
        *self.info.lock().unwrap() = format!("{}: {part}, {size}", spec.asset.name);
    }

    fn decode_image(&self, spec: &LayerSpec) {
        let asset = &spec.asset;
        match image::open(&asset.path) {
            Ok(image) => {
                let source = (image.width(), image.height());
                let part = spec
                    .crop
                    .filter(|[x, y, w, h]| x + w <= image.width() && y + h <= image.height());
                let image = match part {
                    Some([x, y, w, h]) => image.crop_imm(x, y, w, h),
                    None => image,
                };
                let (w, h) =
                    decode_size((image.width(), image.height()), spec.shown, spec.max_side);
                // Never enlarged: rounding to even sizes may add a pixel.
                let (w, h) = (
                    (w as u32).min(image.width()),
                    (h as u32).min(image.height()),
                );
                let image = if (w, h) != (image.width(), image.height()) {
                    // Lanczos keeps fine detail and text crisp when
                    // shrinking, where the GPU's bilinear sampling aliases.
                    image.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
                } else {
                    image
                };
                self.describe(
                    spec,
                    source,
                    part,
                    (image.width() as usize, image.height() as usize),
                );
                let rgba = image.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                // Pictures may be transparent, so their alpha is honoured.
                self.publish(egui::ColorImage::from_rgba_unmultiplied(size, &rgba));
            }
            Err(error) => self.fail(format!("{}: image decode failed: {error}", asset.name)),
        }
    }

    /// Waits until the show reaches a frame due at media clock `due`, then
    /// says whether to show it, skip it, or start FFmpeg again further on.
    /// While paused or not yet playing the frame is held. `None` when the
    /// decoder was cancelled meanwhile.
    fn wait_until_due(&self, due: f64, last_shown: Instant) -> Option<Pace> {
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                return None;
            }
            let clock = self.clock().filter(|_| self.active.load(Ordering::SeqCst));
            let Some(clock) = clock else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            match pace(due, clock, last_shown.elapsed() >= MAX_HOLD) {
                Pace::Wait(seconds) => thread::sleep(Duration::from_secs_f64(seconds.min(0.01))),
                decision => return Some(decision),
            }
        }
    }

    /// Streams RGBA frames from FFmpeg and shows each one when the show
    /// clock reaches it: early frames wait, late ones are skipped, and a
    /// decoder that falls far behind (a slow seek, a stall, a PC too slow
    /// for the file) starts FFmpeg again where the show will be. While
    /// inactive it stops reading the pipe, which blocks FFmpeg, so playback
    /// resumes on the same frame. `start` is the media clock of the first
    /// frame (`LayerSpec::media_clock`).
    fn decode_video(&self, spec: &LayerSpec, start: f64) {
        let asset = &spec.asset;
        let known = match (asset.width, asset.height) {
            (Some(w), Some(h)) if w >= 2 && h >= 2 => Some((w, h)),
            _ => None,
        };
        // FFmpeg turns a video stored sideways with a rotation flag (phone
        // clips) upright before any filter, so the crop and scale below
        // work on the upright picture, whose size the file tells.
        let source = probe_size(&asset.path).or(known).unwrap_or((1280, 720));
        let (source_width, source_height) = source;
        // Only the part this PC shows is decoded; cropping is free in FFmpeg.
        let part = spec
            .crop
            .and_then(|c| crop_in_frame(c, known.unwrap_or(source), source));
        let (crop, part_width, part_height) = match part {
            Some([x, y, w, h]) => (format!("crop={w}:{h}:{x}:{y},"), w, h),
            None => (String::new(), source_width, source_height),
        };
        let (width, height) = decode_size((part_width, part_height), spec.shown, spec.max_side);
        self.describe(spec, source, part, (width, height));
        // The scale filter also does the RGBA conversion, so at the same size
        // it costs nothing extra, and it pins the frame size read below even
        // for odd-sized files. Shrinking uses Lanczos with exact rounding and
        // full chroma, which keeps fine detail and text crisp where the GPU's
        // bilinear sampling of a larger texture would alias it.
        let scale = if (width, height) == (part_width as usize, part_height as usize) {
            format!("scale={width}:{height}")
        } else {
            format!("scale={width}:{height}:flags=lanczos+accurate_rnd+full_chroma_int")
        };
        let filter = format!("{crop}{scale}");
        let frame_time = 1.0 / VIDEO_FPS;
        // Media clock of this FFmpeg run's first frame.
        let mut run_start = start;
        // Reading only the end of the file, to hold its last frame.
        let mut tail = false;
        let mut published = false;
        let mut last_shown = Instant::now();
        // How long FFmpeg took to deliver its first frame: a restart aims
        // that far ahead of the show.
        let mut startup = 0.3_f64;
        while !self.cancelled.load(Ordering::SeqCst) {
            let mut command = if tail {
                ffmpeg_tail(&asset.path)
            } else {
                ffmpeg_input(&asset.path, spec.looping, spec.file_time(run_start))
            };
            command
                .args([
                    "-an",
                    "-vf",
                    &filter,
                    "-r",
                    &VIDEO_FPS.to_string(),
                    "-pix_fmt",
                    "rgba",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let spawned = Instant::now();
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    self.fail(format!("Could not start FFmpeg: {error}"));
                    return;
                }
            };
            let Some(mut stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return;
            };
            let mut frames = 0_u64;
            let mut last = None;
            let run = loop {
                if self.cancelled.load(Ordering::SeqCst) {
                    break Run::Cancelled;
                }
                // The first frame is decoded straight away so a clip
                // appears instantly when its time comes.
                if published && !tail && !self.active.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let mut rgba = vec![0_u8; width * height * 4];
                if stdout.read_exact(&mut rgba).is_err() {
                    break Run::Ended;
                }
                if frames == 0 {
                    startup = spawned.elapsed().as_secs_f64().clamp(0.05, 3.0);
                }
                let due = run_start + frames as f64 * frame_time;
                frames += 1;
                if tail {
                    last = Some(rgba);
                    continue;
                }
                if published {
                    match self.wait_until_due(due, last_shown) {
                        None => break Run::Cancelled,
                        Some(Pace::Drop) => continue,
                        Some(Pace::Resync) => break Run::Resync,
                        Some(_) => {}
                    }
                }
                // Video is opaque, so its bytes are already egui's premultiplied
                // layout: this is a straight copy rather than a per-pixel multiply.
                self.publish(egui::ColorImage::from_rgba_premultiplied(
                    [width, height],
                    &rgba,
                ));
                published = true;
                last_shown = Instant::now();
            };
            let _ = child.kill();
            let _ = child.wait();
            match run {
                Run::Cancelled => return,
                Run::Resync => run_start = self.clock().unwrap_or(run_start) + startup,
                Run::Ended if tail => {
                    match last {
                        Some(rgba) => self.publish(egui::ColorImage::from_rgba_premultiplied(
                            [width, height],
                            &rgba,
                        )),
                        None if !published => {
                            self.fail(format!("{}: FFmpeg produced no frames", asset.name))
                        }
                        None => {}
                    }
                    return;
                }
                // Started at or past the end of a clip that plays once, as a
                // seek or a cue into its held last frame does: show that
                // last frame, as playing through to here would.
                Run::Ended if frames == 0 && !spec.looping && spec.file_time(run_start) > 0.001 => {
                    tail = true;
                }
                Run::Ended => {
                    if frames == 0 && !published {
                        self.fail(format!("{}: FFmpeg produced no frames", asset.name));
                    }
                    // A non-looping clip holds its last frame.
                    return;
                }
            }
        }
    }
}

/// The upright picture size of a video file, from FFprobe. Each file is
/// probed once while it stays the same.
fn probe_size(path: &str) -> Option<(u32, u32)> {
    type Key = (String, u64, Option<std::time::SystemTime>);
    static PROBED: OnceLock<Mutex<HashMap<Key, Option<(u32, u32)>>>> = OnceLock::new();
    let meta = fs::metadata(path).ok();
    let key = (
        path.to_owned(),
        meta.as_ref().map_or(0, |m| m.len()),
        meta.and_then(|m| m.modified().ok()),
    );
    let probed = PROBED.get_or_init(Default::default);
    if let Some(size) = probed.lock().unwrap().get(&key) {
        return *size;
    }
    let output = tool_command("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height:stream_tags=rotate:stream_side_data=rotation",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    let size = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|value| upright_size(value.get("streams")?.get(0)?));
    probed.lock().unwrap().insert(key, size);
    size
}

/// A video stream's picture size as shown. Phones record portrait video as
/// a landscape picture plus a rotation (a display matrix, or an older
/// `rotate` tag), so a quarter turn swaps width and height.
fn upright_size(stream: &serde_json::Value) -> Option<(u32, u32)> {
    let side = |key: &str| stream.get(key)?.as_u64().map(|v| v as u32);
    let (width, height) = (side("width")?, side("height")?);
    let number = |v: &serde_json::Value| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok());
    let matrix = stream
        .get("side_data_list")
        .and_then(|list| list.as_array())
        .and_then(|list| list.iter().find_map(|data| number(data.get("rotation")?)));
    let tag = stream
        .get("tags")
        .and_then(|tags| number(tags.get("rotate")?));
    let quarters = (matrix.or(tag).unwrap_or(0.0) / 90.0).round() as i64;
    Some(if quarters.rem_euclid(2) == 1 {
        (height, width)
    } else {
        (width, height)
    })
}

fn ffmpeg_input(path: &str, looping: bool, start: f64) -> ProcessCommand {
    let mut command = tool_command("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error"]);
    if looping {
        command.args(["-stream_loop", "-1"]);
    }
    if start > 0.001 {
        command.args(["-ss", &format!("{start:.3}")]);
    }
    command.arg("-i").arg(path);
    command
}

/// FFmpeg reading only the last second of a file, for its last frame.
fn ffmpeg_tail(path: &str) -> ProcessCommand {
    let mut command = tool_command("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-sseof", "-1"]);
    command.arg("-i").arg(path);
    command
}

/// Sound for one layer: FFmpeg decodes to 48 kHz stereo PCM, which a rodio
/// sink plays. Pausing the sink stops pulling samples, so FFmpeg waits.
struct AudioTrack {
    sink: rodio::Sink,
    cancelled: Arc<AtomicBool>,
    /// Media clock (`LayerSpec::media_clock`) where the sound starts.
    start: f64,
    /// Samples the source skips to catch up with the show; see `PcmSource`.
    late: Arc<AtomicU64>,
    started: bool,
}

impl Drop for AudioTrack {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.sink.stop();
    }
}

impl AudioTrack {
    fn start(output: &rodio::OutputStreamHandle, spec: &LayerSpec, start: f64) -> Option<Self> {
        let sink = rodio::Sink::try_new(output).ok()?;
        sink.pause();
        let (tx, rx) = mpsc::sync_channel::<Vec<i16>>(24);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut command = ffmpeg_input(&spec.asset.path, spec.looping, spec.file_time(start));
        command
            .args([
                "-vn",
                "-f",
                "s16le",
                "-ac",
                "2",
                "-ar",
                &AUDIO_RATE.to_string(),
            ])
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let worker_cancelled = cancelled.clone();
        thread::spawn(move || pump_audio(command, tx, &worker_cancelled));
        let late = Arc::new(AtomicU64::new(0));
        sink.append(PcmSource {
            rx,
            chunk: Vec::new(),
            position: 0,
            owed: 0,
            late: late.clone(),
        });
        Some(Self {
            sink,
            cancelled,
            start,
            late,
            started: false,
        })
    }

    /// Plays with the show at media clock `clock`. The first time, sound
    /// that starts late (a late scheduled start, a seek during playback)
    /// skips ahead to the clock instead of trailing it for the whole clip.
    fn play(&mut self, clock: f64) {
        if !self.started {
            self.started = true;
            let frames = ((clock - self.start).max(0.0) * AUDIO_RATE as f64).round() as u64;
            self.late.store(frames * 2, Ordering::SeqCst);
        }
        self.sink.play();
    }
}

fn pump_audio(mut command: ProcessCommand, tx: SyncSender<Vec<i16>>, cancelled: &AtomicBool) {
    let Ok(mut child) = command.spawn() else {
        return;
    };
    let Some(mut stdout) = child.stdout.take() else {
        return;
    };
    let mut bytes = vec![0_u8; 8192];
    'read: while !cancelled.load(Ordering::SeqCst) {
        let mut filled = 0;
        while filled < bytes.len() {
            match stdout.read(&mut bytes[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        // Whole stereo frames only, so channels never swap.
        let usable = filled / 4 * 4;
        if usable == 0 {
            break;
        }
        let mut chunk: Vec<i16> = bytes[..usable]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        loop {
            match tx.try_send(chunk) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    if cancelled.load(Ordering::SeqCst) {
                        break 'read;
                    }
                    chunk = back;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => break 'read,
            }
        }
        if usable < bytes.len() {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Plays the PCM FFmpeg delivers and keeps it on the show clock. When
/// FFmpeg is behind (starting up, a disk stall) it plays silence and later
/// skips as much sound, so the sound never ends up late for good.
struct PcmSource {
    rx: Receiver<Vec<i16>>,
    chunk: Vec<i16>,
    position: usize,
    /// Samples still to skip to be back on the clock; always whole frames.
    owed: u64,
    /// Samples to skip, set by `AudioTrack::play` for a late start.
    late: Arc<AtomicU64>,
}

impl Iterator for PcmSource {
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        loop {
            if self.position < self.chunk.len() {
                let sample = self.chunk[self.position];
                self.position += 1;
                return Some(sample);
            }
            // Only at chunk ends, which are whole stereo frames, so the
            // channels never swap.
            self.owed += self.late.swap(0, Ordering::SeqCst) / 2 * 2;
            match self.rx.try_recv() {
                Ok(chunk) => {
                    let skip = (self.owed as usize).min(chunk.len()) / 2 * 2;
                    self.owed -= skip as u64;
                    self.chunk = chunk;
                    self.position = skip;
                }
                // Decoder is behind: play one silent stereo frame, owed back.
                Err(TryRecvError::Empty) => {
                    self.chunk.clear();
                    self.chunk.extend_from_slice(&[0, 0]);
                    self.position = 0;
                    self.owed += 2;
                }
                Err(TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl rodio::Source for PcmSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        AUDIO_RATE
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

struct Slot {
    spec: LayerSpec,
    video: Option<Arc<Decoder>>,
    audio: Option<AudioTrack>,
}

impl Slot {
    fn cancel(&self) {
        if let Some(video) = &self.video {
            video.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

/// Decoders for every layer of the prepared scene, keyed by layer id.
#[derive(Default)]
struct MediaPool {
    slots: Mutex<HashMap<Uuid, Slot>>,
    /// Media pre-rolled for the scene that plays next (or this one again).
    standby: Mutex<Option<(Uuid, f64, HashMap<Uuid, Slot>)>>,
    /// Media pre-rolled for a scene cued during playback, kept apart from
    /// `standby` so loops of the scene on air can't replace it.
    cued: Mutex<Option<(Uuid, f64, HashMap<Uuid, Slot>)>>,
    audio_output: Option<rodio::OutputStreamHandle>,
    view: Mutex<DecodeView>,
}

impl MediaPool {
    /// Which projectors this PC draws and the quality setting; the next
    /// `sync` restarts the layers whose decode changes because of it.
    fn set_view(&self, outputs: Option<Vec<Uuid>>, quality: setup::Quality) {
        *self.view.lock().unwrap() = DecodeView {
            outputs,
            max_side: quality.max_side(),
        };
    }

    fn set_quality(&self, quality: setup::Quality) {
        self.view.lock().unwrap().max_side = quality.max_side();
    }

    fn layer_specs(&self, project: &ShowProject, scene_id: Uuid) -> HashMap<Uuid, LayerSpec> {
        let view = self.view.lock().unwrap().clone();
        let outputs: Vec<&ProjectorOutput> = project
            .outputs
            .iter()
            .filter(|o| view.outputs.as_ref().is_none_or(|ids| ids.contains(&o.id)))
            .collect();
        let mut specs = HashMap::new();
        if let Some(scene) = project.scenes.iter().find(|s| s.id == scene_id) {
            for layer in &scene.layers {
                if let Some(asset) = project.assets.iter().find(|a| a.id == layer.asset_id) {
                    specs.insert(
                        layer.id,
                        LayerSpec::new(layer, asset, &outputs, view.max_side),
                    );
                }
            }
        }
        specs
    }

    /// Starts decoders for the scene's layers at show time `position` and
    /// stops the rest. Unchanged layers keep running unless `restart`.
    fn sync(&self, project: &ShowProject, scene_id: Uuid, restart: bool, position: f64) {
        let wanted = self.layer_specs(project, scene_id);
        // A scene restarting from the top can use media pre-rolled for it.
        let mut standby = match self.standby.lock().unwrap().take() {
            Some((id, at, slots)) if restart && id == scene_id && (at - position).abs() < 1e-6 => {
                slots
            }
            Some((_, _, slots)) => {
                slots.values().for_each(Slot::cancel);
                HashMap::new()
            }
            None => HashMap::new(),
        };
        let mut slots = self.slots.lock().unwrap();
        slots.retain(|id, slot| {
            let keep = !restart && wanted.get(id) == Some(&slot.spec);
            if !keep {
                slot.cancel();
            }
            keep
        });
        for (id, spec) in wanted {
            if slots.contains_key(&id) {
                continue;
            }
            let slot = match standby.remove(&id) {
                Some(ready) if ready.spec == spec => ready,
                other => {
                    if let Some(stale) = other {
                        stale.cancel();
                    }
                    self.start_slot(spec, position)
                }
            };
            slots.insert(id, slot);
        }
        standby.values().for_each(Slot::cancel);
    }

    /// Starts a scene's media paused on its first frame, ready to swap in
    /// at the next loop or scene change without a black gap.
    fn preroll(&self, project: &ShowProject, scene_id: Uuid, position: f64) {
        let mut standby = self.standby.lock().unwrap();
        if standby
            .as_ref()
            .is_some_and(|(id, at, _)| *id == scene_id && (at - position).abs() < 1e-6)
        {
            return;
        }
        if let Some((_, _, old)) = standby.take() {
            old.values().for_each(Slot::cancel);
        }
        let slots = self
            .layer_specs(project, scene_id)
            .into_iter()
            .map(|(id, spec)| (id, self.start_slot(spec, position)))
            .collect();
        *standby = Some((scene_id, position, slots));
    }

    /// Starts a cued scene's media on its first frame while the scene on
    /// air keeps playing; `cut_to_cue` swaps it in at the start time.
    fn cue(&self, project: &ShowProject, scene_id: Uuid, position: f64) {
        let slots = self
            .layer_specs(project, scene_id)
            .into_iter()
            .map(|(id, spec)| (id, self.start_slot(spec, position)))
            .collect();
        let old = self.cued.lock().unwrap().replace((scene_id, position, slots));
        if let Some((_, _, old)) = old {
            old.values().for_each(Slot::cancel);
        }
    }

    fn drop_cue(&self) {
        let old = self.cued.lock().unwrap().take();
        if let Some((_, _, old)) = old {
            old.values().for_each(Slot::cancel);
        }
    }

    /// Makes the cued media the scene's media, as `sync` with `restart`.
    fn cut_to_cue(&self, project: &ShowProject, scene_id: Uuid, position: f64) {
        let cued = self.cued.lock().unwrap().take();
        if let Some(cued) = cued {
            let old = self.standby.lock().unwrap().replace(cued);
            if let Some((_, _, old)) = old {
                old.values().for_each(Slot::cancel);
            }
        }
        self.sync(project, scene_id, true, position);
    }

    fn start_slot(&self, spec: LayerSpec, position: f64) -> Slot {
        let start = spec.media_clock(position);
        let visual = matches!(spec.asset.kind, AssetKind::Image | AssetKind::Video);
        let video = (visual && !spec.hidden).then(|| {
            let decoder = Arc::new(Decoder::default());
            let worker = decoder.clone();
            let worker_spec = spec.clone();
            thread::spawn(move || match worker_spec.asset.kind {
                AssetKind::Image => worker.decode_image(&worker_spec),
                _ => worker.decode_video(&worker_spec, start),
            });
            decoder
        });
        let wants_audio = match spec.asset.kind {
            AssetKind::Audio => true,
            AssetKind::Video => spec.audio,
            _ => false,
        };
        let audio = self
            .audio_output
            .as_ref()
            .filter(|_| wants_audio)
            .and_then(|output| AudioTrack::start(output, &spec, start));
        Slot { spec, video, audio }
    }

    fn clear(&self) {
        for (_, slot) in self.slots.lock().unwrap().drain() {
            slot.cancel();
        }
        if let Some((_, _, standby)) = self.standby.lock().unwrap().take() {
            standby.values().for_each(Slot::cancel);
        }
        self.drop_cue();
    }

    /// Runs or pauses each layer's decoders for the current show time.
    fn update(&self, scene: Option<&Scene>, position: f64, playing: bool, master: f32) {
        let mut slots = self.slots.lock().unwrap();
        for (id, slot) in slots.iter_mut() {
            let layer = scene.and_then(|s| s.layers.iter().find(|l| l.id == *id));
            let active = playing && layer.is_some_and(|l| l.active_at(position));
            let clock = slot.spec.media_clock(position);
            if let Some(video) = &slot.video {
                // The clock is set before the decoder may run, so it never
                // paces against an old one.
                if active {
                    video.set_clock(clock);
                }
                video.active.store(active, Ordering::SeqCst);
            }
            if let Some(audio) = &mut slot.audio {
                audio
                    .sink
                    .set_volume(master * layer.map_or(0.0, |l| l.volume));
                if active {
                    audio.play(clock);
                } else {
                    audio.sink.pause();
                }
            }
        }
    }

    /// Each picture layer's newest frame and which part of the media it is.
    fn frames(&self) -> Vec<(Uuid, Option<DecodedFrame>, [f32; 4])> {
        self.slots
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, slot)| {
                let video = slot.video.as_ref()?;
                Some((*id, video.latest.lock().unwrap().clone(), slot.spec.uv))
            })
            .collect()
    }

    /// Picture and sound layer counts, errors, and what each picture layer
    /// is decoding, for the Player window.
    fn summary(&self) -> (usize, usize, Vec<String>, Vec<String>) {
        let slots = self.slots.lock().unwrap();
        let videos = slots.values().filter(|s| s.video.is_some()).count();
        let sounds = slots.values().filter(|s| s.audio.is_some()).count();
        let errors = slots
            .values()
            .filter_map(|s| s.video.as_ref()?.error.lock().unwrap().clone())
            .collect();
        let mut infos: Vec<String> = slots
            .values()
            .filter_map(|s| s.video.as_ref()?.info.lock().unwrap().clone().into())
            .filter(|info| !info.is_empty())
            .collect();
        infos.sort();
        (videos, sounds, errors, infos)
    }
}

/// The audio device must stay open on its own thread; only the handle is shared.
fn open_audio_output() -> Option<rodio::OutputStreamHandle> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || match rodio::OutputStream::try_default() {
        Ok((stream, handle)) => {
            let _ = tx.send(Some(handle));
            let _keep_open = stream;
            loop {
                thread::park();
            }
        }
        Err(_) => {
            let _ = tx.send(None);
        }
    });
    rx.recv().ok().flatten()
}

/// Where media received over the LAN is stored on this PC. Override with the
/// MAPFORGE_MEDIA_DIR environment variable.
fn media_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MAPFORGE_MEDIA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join("MapForge Media")
}

fn cached_path(asset: &Asset) -> PathBuf {
    let ext = std::path::Path::new(&asset.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    media_cache_dir().join(format!("{}.{ext}", asset.id))
}

/// A file of its own for one transfer of `asset`, next to the cached copy.
/// Two transfers of the same file (Producer sending it while this sub copies
/// it from the master, or two Players sharing one media folder) never write
/// into each other's file.
fn partial_path(asset: &Asset) -> PathBuf {
    media_cache_dir().join(format!("{}.{}.part", asset.id, Uuid::new_v4().simple()))
}

/// Deletes partial files of `asset` left behind by a transfer that was cut
/// off when the Player closed. A running transfer writes to its file all the
/// time, so one untouched for ten minutes is abandoned.
fn remove_stale_partials(asset: &Asset) {
    let prefix = format!("{}.", asset.id);
    let Ok(entries) = fs::read_dir(media_cache_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(600));
        if name.starts_with(&prefix) && name.ends_with(".part") && stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Assets being received right now, from Producer or from the master.
fn transfers() -> &'static Mutex<HashSet<Uuid>> {
    static TRANSFERS: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();
    TRANSFERS.get_or_init(Default::default)
}

/// Holds one asset as "being received" until dropped, so this Player only
/// writes one copy of a file at a time.
struct Transfer(Uuid);

impl Transfer {
    /// `None` when the asset is already being received.
    fn begin(asset: &Asset) -> Option<Self> {
        let started = transfers().lock().ok()?.insert(asset.id);
        if started {
            remove_stale_partials(asset);
        }
        started.then(|| Transfer(asset.id))
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Ok(mut transfers) = transfers().lock() {
            transfers.remove(&self.0);
        }
    }
}

/// File types Producer imports for each kind of media.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];
const VIDEO_EXTENSIONS: [&str; 6] = ["mp4", "mov", "mkv", "avi", "webm", "m4v"];
const AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "wav", "aac", "m4a", "flac", "ogg", "aiff"];

/// Whether `path` has a file type Producer imports as the asset's kind.
fn is_media_file(asset: &Asset, path: &std::path::Path) -> bool {
    let allowed: &[&str] = match asset.kind {
        AssetKind::Image => &IMAGE_EXTENSIONS,
        AssetKind::Video => &VIDEO_EXTENSIONS,
        AssetKind::Audio => &AUDIO_EXTENSIONS,
        AssetKind::Unknown => return false,
    };
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| allowed.contains(&e.to_ascii_lowercase().as_str()))
}

/// SHA-256 of a file, remembered while its size and modified time stay the
/// same, so a large original is read once however many subs fetch it.
fn verified_checksum(path: &std::path::Path) -> Option<String> {
    type Known = HashMap<PathBuf, (u64, std::time::SystemTime, String)>;
    static KNOWN: OnceLock<Mutex<Known>> = OnceLock::new();
    let metadata = fs::metadata(path).ok()?;
    let stamp = (metadata.len(), metadata.modified().ok()?);
    let known = KNOWN.get_or_init(Default::default);
    if let Some((len, modified, checksum)) = known.lock().ok()?.get(path) {
        if (*len, *modified) == stamp {
            return Some(checksum.clone());
        }
    }
    let checksum = mapforge_core::sha256_file(path).ok()?;
    known
        .lock()
        .ok()?
        .insert(path.to_path_buf(), (stamp.0, stamp.1, checksum.clone()));
    Some(checksum)
}

/// The file this Player may send to a sub for `asset`, or `None`. Shows
/// arrive over the LAN without a password, so a show could name any file on
/// this PC; only media of the asset's kind is sent. A copy in the media
/// cache was checked when it arrived. The original path, used when the show
/// was made on this PC, is sent only when its size and SHA-256 match the
/// show, which a show naming some other file can't arrange.
fn shareable_media(asset: &Asset) -> Option<PathBuf> {
    let cached = cached_path(asset);
    if cached.is_file() {
        return is_media_file(asset, &cached).then_some(cached);
    }
    let original = PathBuf::from(&asset.path);
    let metadata = fs::metadata(&original).ok()?;
    let size_ok = asset.size_bytes.is_none_or(|size| size == metadata.len());
    if !metadata.is_file() || !size_ok || !is_media_file(asset, &original) {
        return None;
    }
    verified_checksum(&original)
        .filter(|checksum| checksum.eq_ignore_ascii_case(&asset.checksum_sha256))
        .map(|_| original)
}

/// A received copy wins over the original path, which usually only exists
/// on the Producer PC.
fn resolve_media(asset: &Asset) -> PathBuf {
    let cached = cached_path(asset);
    if cached.exists() {
        cached
    } else {
        PathBuf::from(&asset.path)
    }
}

fn asset_status(asset: &Asset) -> AssetStatus {
    let path = resolve_media(asset);
    let size_ok = fs::metadata(&path).is_ok_and(|m| asset.size_bytes.is_none_or(|s| s == m.len()));
    AssetStatus {
        asset_id: asset.id,
        ready: size_ok,
        cached: path == cached_path(asset),
    }
}

fn scheduler(shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>) {
    loop {
        thread::sleep(Duration::from_millis(10));
        let mut runtime = shared.lock().unwrap();
        runtime.handle_scheduled_start(&media);
        runtime.handle_loops(&media);
        runtime.handle_scene_end(&media);
        let position = runtime.clock.position();
        let playing = runtime.state.transport == Transport::Playing;
        if let (true, Some((scene_id, start, at))) = (playing, runtime.next_jump()) {
            if at > 0.0 && position >= at - PREROLL_SECONDS {
                if let Some(project) = &runtime.project {
                    media.preroll(project, scene_id, start);
                }
            }
        }
        let master = if runtime.state.muted || !runtime.plays_audio() {
            0.0
        } else {
            runtime.state.volume
        };
        media.update(runtime.scene(), position, playing, master);
    }
}

fn apply(runtime: &mut Runtime, envelope: Envelope, media: &MediaPool) {
    if envelope.protocol_version != PROTOCOL_VERSION {
        runtime.state.message = "Protocol version mismatch".into();
        return;
    }
    if runtime.seen.contains(&envelope.command_id) {
        return;
    }
    runtime.seen.insert(envelope.command_id);
    runtime.order.push_back(envelope.command_id);
    if runtime.order.len() > 1024 {
        if let Some(id) = runtime.order.pop_front() {
            runtime.seen.remove(&id);
        }
    }
    let mut command = envelope.command;
    // Shows saved before positions snapped to whole pixels may hold
    // fractional ones, which blur media; every show is snapped on arrival.
    if let Command::LoadProject { project, .. } = &mut command {
        project.snap_to_pixels();
    }
    match command {
        Command::LoadProject {
            project,
            outputs,
            player,
        } => match project.validate() {
            Ok(()) => {
                media.set_view(outputs.clone(), runtime.settings.quality);
                let current = runtime
                    .state
                    .scene_id
                    .filter(|id| project.scenes.iter().any(|s| s.id == *id));
                match current {
                    Some(scene_id) => {
                        media.sync(&project, scene_id, false, runtime.clock.position())
                    }
                    None => {
                        runtime.state.scene_id = None;
                        media.clear();
                    }
                }
                save_show(&project, &outputs, &player);
                runtime.project = Some(project);
                runtime.outputs = outputs;
                runtime.address = player;
                runtime.state.message = "Project loaded".into();
            }
            Err(e) => runtime.state.message = format!("Rejected project: {e}"),
        },
        Command::Prepare { scene_id } => {
            runtime.prepare(media, scene_id, 0.0);
        }
        Command::Cue { scene_id, seconds } => {
            if runtime.prepare(media, scene_id, seconds.max(0.0)) {
                runtime.play(media);
            }
        }
        Command::CueAt {
            scene_id,
            seconds,
            start_time_unix_ms,
        } => runtime.schedule(media, scene_id, seconds.max(0.0), start_time_unix_ms),
        Command::Play => runtime.play(media),
        Command::PlayAt { start_time_unix_ms } => {
            if let Some(scene_id) = runtime.state.scene_id {
                runtime.schedule(
                    media,
                    scene_id,
                    runtime.clock.position(),
                    start_time_unix_ms,
                );
            } else {
                runtime.state.message = "No prepared scene for scheduled play".into();
            }
        }
        Command::ReleaseLoop => runtime.release_loops(),
        Command::Pause => {
            runtime.cancel_scheduled(media);
            runtime.clock.hold();
            runtime.state.transport = Transport::Paused;
            runtime.state.message = "Paused".into();
        }
        Command::Stop => runtime.stop(media),
        Command::Seek { seconds } => {
            runtime.cancel_scheduled(media);
            let seconds = seconds.max(0.0);
            if let (Some(project), Some(scene_id)) = (&runtime.project, runtime.state.scene_id) {
                media.sync(project, scene_id, true, seconds);
            }
            runtime.clock.set(seconds);
            runtime.reset_loops(seconds);
            runtime.state.message = format!("Position {seconds:.2}s");
        }
        Command::SetVolume { value } => runtime.state.volume = value.clamp(0.0, 1.0),
        Command::SetMute { value } => runtime.state.muted = value,
        Command::SetBlackout { value } => runtime.state.blackout = value,
        Command::GetState => {}
    }
}

fn saved_show_path() -> PathBuf {
    media_cache_dir().join("current-show.json")
}

/// Keeps the last show received so the Player can run it after a restart
/// without Producer. Writes happen on one thread, newest show wins.
fn save_show(project: &ShowProject, outputs: &Option<Vec<Uuid>>, player: &Option<String>) {
    static SAVER: OnceLock<Sender<String>> = OnceLock::new();
    let Ok(json) = serde_json::to_string(&serde_json::json!({
        "project": project,
        "outputs": outputs,
        "player": player,
    })) else {
        return;
    };
    let saver = SAVER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            while let Ok(mut json) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    json = newer;
                }
                let path = saved_show_path();
                let partial = path.with_extension("json.part");
                let _ = fs::create_dir_all(media_cache_dir());
                if fs::write(&partial, json).is_ok() {
                    let _ = fs::rename(&partial, &path);
                }
            }
        });
        tx
    });
    let _ = saver.send(json);
}

/// Opens the show this Player last received. Returns whether it should start
/// playing by itself.
fn open_saved_show(shared: &Mutex<Runtime>, media: &MediaPool) -> bool {
    #[derive(Deserialize)]
    struct SavedShow {
        project: ShowProject,
        outputs: Option<Vec<Uuid>>,
        player: Option<String>,
    }
    let Some(saved) = fs::read_to_string(saved_show_path())
        .ok()
        .and_then(|json| serde_json::from_str::<SavedShow>(&json).ok())
    else {
        return false;
    };
    apply_local(
        shared,
        media,
        Command::LoadProject {
            project: saved.project,
            outputs: saved.outputs,
            player: saved.player,
        },
    );
    let mut runtime = shared.lock().unwrap();
    let Some(name) = runtime.project.as_ref().map(|p| p.name.clone()) else {
        return false;
    };
    runtime.state.message = format!("Opened the last show: {name}");
    runtime.is_master() && runtime.settings.autoplay
}

fn apply_local(shared: &Mutex<Runtime>, media: &MediaPool, command: Command) {
    apply(
        &mut shared.lock().unwrap(),
        Envelope {
            protocol_version: PROTOCOL_VERSION,
            command_id: Uuid::new_v4(),
            command,
        },
        media,
    );
}

fn handle_protocol(mut stream: TcpStream, shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_ok() && line.len() <= 8 * 1024 * 1024 {
        if let Ok(envelope) = serde_json::from_str::<Envelope>(&line) {
            apply(&mut shared.lock().unwrap(), envelope, &media);
        }
    }
    let state = {
        let mut runtime = shared.lock().unwrap();
        let mut state = runtime.snapshot();
        if runtime.project_update_pending {
            state.project_update = runtime.project.clone().map(Box::new);
            runtime.project_update_pending = false;
        }
        state
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = writeln!(stream, "{json}");
    }
}

/// Ports can be moved, e.g. to run two Players on one test machine.
fn port_setting(variable: &str, default: u16) -> u16 {
    std::env::var(variable)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn protocol_server(shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>) {
    let port = port_setting("MAPFORGE_PORT", DEFAULT_PLAYER_PORT);
    let listener = TcpListener::bind(("0.0.0.0", port))
        .unwrap_or_else(|_| panic!("TCP port {port} is unavailable"));
    for stream in listener.incoming().flatten() {
        let state = shared.clone();
        let media = media.clone();
        thread::spawn(move || handle_protocol(stream, state, media));
    }
}

fn http_response(mut stream: TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = stream.write_all(response.as_bytes());
}

fn button_label(label: &str, fallback: &str) -> String {
    if label.trim().is_empty() {
        fallback.to_string()
    } else {
        label.to_string()
    }
}

fn controller_json(runtime: &Runtime) -> String {
    let settings = runtime
        .project
        .as_ref()
        .map(|p| p.controller.clone())
        .unwrap_or_default();
    let scenes: Vec<_> = runtime
        .project
        .iter()
        .flat_map(|p| p.scenes.iter().enumerate())
        .filter(|(_, s)| !s.button.hidden)
        .map(|(index, s)| {
            let cues: Vec<_> = s
                .cues
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.button.hidden)
                .map(|(cue_index, c)| {
                    serde_json::json!({
                        "index": cue_index,
                        "label": button_label(&c.button.label, &c.name),
                        "color": c.button.color,
                        "hotkey": c.hotkey,
                    })
                })
                .collect();
            let loops: Vec<_> = s
                .loops
                .iter()
                .enumerate()
                .filter(|(_, l)| !l.button.hidden)
                .map(|(loop_index, l)| {
                    serde_json::json!({
                        "index": loop_index,
                        "label": button_label(&l.button.label, &l.name),
                        "name": l.name,
                        "color": l.button.color,
                        "hotkey": l.hotkey,
                    })
                })
                .collect();
            serde_json::json!({
                "index": index,
                "id": s.id,
                "loops": loops,
                "label": button_label(&s.button.label, &s.name),
                "color": s.button.color,
                "hotkey": scene_hotkey(index, s),
                "cues": cues,
            })
        })
        .collect();
    let current_scene = runtime.scene().map(|scene| {
        serde_json::json!({
            "id": scene.id,
            "label": button_label(&scene.button.label, &scene.name),
            "duration_seconds": scene.duration(),
        })
    });
    serde_json::json!({ "settings": settings, "scenes": scenes, "current_scene": current_scene }).to_string()
}

fn http_server(shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>, relay: Arc<Relay>) {
    let port = port_setting("MAPFORGE_HTTP_PORT", CONTROLLER_PORT);
    // Advertise the controller through Bonjour/mDNS so the iOS app can find
    // Players on the venue LAN without asking the operator for an IP address.
    // Keep the daemon alive for as long as the HTTP listener is running.
    let _discovery = advertise_controller(port);
    let listener = TcpListener::bind(("0.0.0.0", port))
        .unwrap_or_else(|_| panic!("HTTP port {port} is unavailable"));
    // One thread per request, so a large upload never blocks the controller.
    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        let media = media.clone();
        let relay = relay.clone();
        thread::spawn(move || handle_http(stream, &shared, &media, &relay));
    }
}

fn advertise_controller(port: u16) -> Option<ServiceDaemon> {
    let computer = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "player".to_owned());
    let host = computer
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .take(50)
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_lowercase();
    let host = if host.is_empty() { "player" } else { &host };
    let instance = format!("MapForge {host}");
    let hostname = format!("{host}.local.");
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("Could not start Bonjour discovery: {error}");
            return None;
        }
    };
    let info = match ServiceInfo::new(
        "_mapforge._tcp.local.",
        &instance,
        &hostname,
        "",
        port,
        &[("app", "mapforge")][..],
    ) {
        Ok(info) => info.enable_addr_auto(),
        Err(error) => {
            eprintln!("Could not create Bonjour service: {error}");
            let _ = daemon.shutdown();
            return None;
        }
    };
    if let Err(error) = daemon.register(info) {
        eprintln!("Could not advertise the controller on the LAN: {error}");
        let _ = daemon.shutdown();
        None
    } else {
        Some(daemon)
    }
}

struct HttpRequest {
    method: String,
    path: String,
    content_length: u64,
    /// Body bytes that arrived together with the headers.
    body_start: Vec<u8>,
}

#[derive(Deserialize)]
struct ProjectorOrderRequest {
    order: Vec<Uuid>,
}

fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    Some(HttpRequest {
        method,
        path,
        content_length,
        body_start: buffer[header_end..].to_vec(),
    })
}

/// Streams the request body into `sink`, returning the byte count.
fn read_body(
    stream: &mut TcpStream,
    request: &HttpRequest,
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<u64> {
    let mut received = request.body_start.len() as u64;
    sink(&request.body_start)?;
    let mut chunk = vec![0_u8; 1 << 20];
    while received < request.content_length {
        let want = ((request.content_length - received) as usize).min(chunk.len());
        let n = stream.read(&mut chunk[..want])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        sink(&chunk[..n])?;
        received += n as u64;
    }
    Ok(received)
}

fn handle_http(mut stream: TcpStream, shared: &Mutex<Runtime>, media: &MediaPool, relay: &Relay) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Some(request) = read_request(&mut stream) else {
        http_response(stream, "400 Bad Request", "text/plain", "Bad request");
        return;
    };
    let (method, path) = (request.method.as_str(), request.path.as_str());
    match (method, path) {
        ("GET", "/") => http_response(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            CONTROLLER_HTML,
        ),
        ("GET", "/api/state") => {
            let state = shared.lock().unwrap().snapshot();
            let json = serde_json::to_string(&state).unwrap();
            http_response(stream, "200 OK", "application/json", &json);
        }
        ("GET", "/api/controller") => {
            let json = controller_json(&shared.lock().unwrap());
            http_response(stream, "200 OK", "application/json", &json);
        }
        ("GET", "/api/projectors") => {
            let json = projector_positions_json(&shared.lock().unwrap());
            http_response(stream, "200 OK", "application/json", &json);
        }
        ("POST", "/api/calibration") => {
            // Read the master's address on its own line: a lock taken inside
            // `if let` would stay held during the forward, freezing this PC's
            // projectors and scheduler for the whole network round trip.
            let master = shared.lock().unwrap().settings.master_http();
            if let Some(master) = master {
                match setup::forward_to_master(&master, "/api/calibration") {
                    Ok(()) => http_response(stream, "204 No Content", "text/plain", ""),
                    Err(e) => http_response(stream, "502 Bad Gateway", "text/plain", &e),
                }
                return;
            }
            let command = {
                let mut runtime = shared.lock().unwrap();
                let Some(project) = runtime.project.as_mut() else {
                    http_response(stream, "409 Conflict", "text/plain", "No show is loaded");
                    return;
                };
                project.test_pattern = if project.test_pattern == TestPattern::Identify {
                    TestPattern::Off
                } else {
                    TestPattern::Identify
                };
                let updated = project.clone();
                runtime.project_update_pending = true;
                let outputs = runtime.outputs.clone();
                let player = runtime.address.clone();
                save_show(&updated, &outputs, &player);
                Command::LoadProject { project: updated, outputs, player }
            };
            relay::control(shared, media, relay, command);
            http_response(stream, "204 No Content", "text/plain", "");
        }
        ("POST", "/api/projector-order") => {
            let mut body = Vec::new();
            if let Err(e) = read_body(&mut stream, &request, |bytes| {
                body.extend_from_slice(bytes);
                Ok(())
            }) {
                http_response(stream, "400 Bad Request", "text/plain", &e.to_string());
                return;
            }
            let request_order = match serde_json::from_slice::<ProjectorOrderRequest>(&body) {
                Ok(order) => order.order,
                Err(e) => {
                    http_response(stream, "400 Bad Request", "text/plain", &e.to_string());
                    return;
                }
            };
            let master = shared.lock().unwrap().settings.master_http();
            if let Some(master) = master {
                match setup::forward_to_master_json(&master, "/api/projector-order", &body) {
                    Ok(()) => http_response(stream, "204 No Content", "text/plain", ""),
                    Err(e) => http_response(stream, "502 Bad Gateway", "text/plain", &e),
                }
                return;
            }
            let result = {
                let mut runtime = shared.lock().unwrap();
                let outputs = runtime.outputs.clone();
                let player = runtime.address.clone();
                let Some(project) = runtime.project.as_mut() else {
                    http_response(stream, "409 Conflict", "text/plain", "No show is loaded");
                    return;
                };
                let ids: HashSet<Uuid> = request_order.iter().copied().collect();
                if request_order.len() != project.outputs.len()
                    || ids.len() != request_order.len()
                    || project.outputs.iter().any(|output| !ids.contains(&output.id))
                {
                    Err("Order must contain every projector exactly once.".to_owned())
                } else {
                    let mut slots: Vec<(f32, f32)> = project.outputs.iter()
                        .map(|output| (output.stage_x, output.stage_y)).collect();
                    slots.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.total_cmp(&b.0)));
                    for (id, (x, y)) in request_order.iter().zip(slots) {
                        if let Some(output) = project.outputs.iter_mut().find(|output| output.id == *id) {
                            output.stage_x = x;
                            output.stage_y = y;
                        }
                    }
                    project.auto_blend();
                    let updated = project.clone();
                    runtime.project_update_pending = true;
                    save_show(&updated, &outputs, &player);
                    Ok(Command::LoadProject { project: updated, outputs, player })
                }
            };
            match result {
                Ok(command) => {
                    relay::control(shared, media, relay, command);
                    http_response(stream, "204 No Content", "text/plain", "");
                }
                Err(message) => http_response(stream, "400 Bad Request", "text/plain", &message),
            }
        }
        ("POST", "/api/swap-positions") => {
            let master = shared.lock().unwrap().settings.master_http();
            if let Some(master) = master {
                match setup::forward_to_master(&master, "/api/swap-positions") {
                    Ok(()) => http_response(stream, "204 No Content", "text/plain", ""),
                    Err(e) => http_response(stream, "502 Bad Gateway", "text/plain", &e),
                }
                return;
            }
            let result = {
                let mut runtime = shared.lock().unwrap();
                let outputs = runtime.outputs.clone();
                let player = runtime.address.clone();
                let Some(project) = runtime.project.as_mut() else {
                    http_response(stream, "409 Conflict", "text/plain", "No show is loaded");
                    return;
                };
                if !swap_first_two_positions(project) {
                    Err((
                        "409 Conflict",
                        "This Player needs at least two projectors to swap positions.".to_owned(),
                    ))
                } else {
                    project.auto_blend();
                    let updated = project.clone();
                    runtime.project_update_pending = true;
                    save_show(&updated, &outputs, &player);
                    Ok(Command::LoadProject {
                        project: updated,
                        outputs,
                        player,
                    })
                }
            };
            match result {
                Ok(command) => {
                    relay::control(shared, media, relay, command);
                    http_response(stream, "204 No Content", "text/plain", "");
                }
                Err((status, message)) => http_response(stream, status, "text/plain", &message),
            }
        }
        ("GET", "/api/assets") => {
            let assets: Vec<Asset> = shared
                .lock()
                .unwrap()
                .project
                .as_ref()
                .map(|p| p.assets.clone())
                .unwrap_or_default();
            let statuses: Vec<AssetStatus> = assets.iter().map(asset_status).collect();
            let json = serde_json::to_string(&statuses).unwrap();
            http_response(stream, "200 OK", "application/json", &json);
        }
        ("POST", "/api/bandwidth") => {
            // LAN speed test: receive and discard the body.
            let started = Instant::now();
            match read_body(&mut stream, &request, |_| Ok(())) {
                Ok(bytes) => {
                    let json = serde_json::json!({
                        "bytes": bytes,
                        "seconds": started.elapsed().as_secs_f64(),
                    })
                    .to_string();
                    http_response(stream, "200 OK", "application/json", &json);
                }
                Err(e) => http_response(stream, "400 Bad Request", "text/plain", &e.to_string()),
            }
        }
        ("PUT", p) if p.starts_with("/api/media/") => {
            receive_media(stream, &request, shared, media)
        }
        ("GET", "/api/show") => {
            let project = shared.lock().unwrap().project.clone();
            match project {
                Some(project) => http_response(
                    stream,
                    "200 OK",
                    "application/json",
                    &serde_json::to_string(&project).unwrap(),
                ),
                None => http_response(stream, "404 Not Found", "text/plain", "No show yet"),
            }
        }
        ("GET", p) if p.starts_with("/api/media/") => send_media(stream, p, shared),
        ("POST", p)
            if p.starts_with("/api/")
                && shared.lock().unwrap().settings.master_http().is_some() =>
        {
            let master = shared.lock().unwrap().settings.master_http().unwrap();
            match setup::forward_to_master(&master, p) {
                Ok(()) => http_response(stream, "204 No Content", "text/plain", ""),
                Err(e) => http_response(stream, "502 Bad Gateway", "text/plain", &e),
            }
        }
        ("POST", "/api/release") => {
            relay::control(shared, media, relay, Command::ReleaseLoop);
            http_response(stream, "204 No Content", "text/plain", "");
        }
        ("POST", p)
            if p.starts_with("/api/scene/")
                || p.starts_with("/api/cue/")
                || p.starts_with("/api/loop/") =>
        {
            // /api/scene/{scene}, /api/cue/{scene}/{cue} or /api/loop/{scene}/{loop}
            let is_loop = p.starts_with("/api/loop/");
            let numbers: Vec<usize> = p
                .trim_start_matches("/api/scene/")
                .trim_start_matches("/api/cue/")
                .trim_start_matches("/api/loop/")
                .split('/')
                .filter_map(|p| p.parse().ok())
                .collect();
            let target = {
                let runtime = shared.lock().unwrap();
                runtime.project.as_ref().and_then(|p| {
                    let scene = p.scenes.get(*numbers.first()?)?;
                    let seconds = match (numbers.get(1), is_loop) {
                        (Some(i), true) => scene.loops.get(*i)?.start,
                        (Some(i), false) => scene.cues.get(*i)?.time,
                        (None, _) => 0.0,
                    };
                    Some((scene.id, seconds))
                })
            };
            if let Some((scene_id, seconds)) = target {
                relay::control(shared, media, relay, Command::Cue { scene_id, seconds });
                http_response(stream, "204 No Content", "text/plain", "");
            } else {
                http_response(
                    stream,
                    "404 Not Found",
                    "text/plain",
                    "Unknown scene or cue",
                );
            }
        }
        ("POST", p) if p.starts_with("/api/") => {
            let command = match p {
                "/api/play" => Some(Command::Play),
                "/api/pause" => Some(Command::Pause),
                "/api/stop" => Some(Command::Stop),
                "/api/mute" => Some(Command::SetMute { value: true }),
                "/api/unmute" => Some(Command::SetMute { value: false }),
                "/api/blackout" => Some(Command::SetBlackout { value: true }),
                "/api/restore" => Some(Command::SetBlackout { value: false }),
                p if p.starts_with("/api/volume/") => p
                    .trim_start_matches("/api/volume/")
                    .parse::<f32>()
                    .ok()
                    .map(|value| Command::SetVolume { value }),
                _ => None,
            };
            if let Some(command) = command {
                relay::control(shared, media, relay, command);
                http_response(stream, "204 No Content", "text/plain", "");
            } else {
                http_response(stream, "404 Not Found", "text/plain", "Unknown control");
            }
        }
        _ => http_response(stream, "404 Not Found", "text/plain", "Not found"),
    }
}

/// Swaps the stage positions of the first two projectors in reading order
/// (top to bottom, left to right). Returns false when there are fewer than
/// two. Reading order is not list order, so the positions are copied out
/// and written back by index rather than splitting the list between them.
fn swap_first_two_positions(project: &mut ShowProject) -> bool {
    let mut order: Vec<usize> = (0..project.outputs.len()).collect();
    order.sort_by(|a, b| {
        let (a, b) = (&project.outputs[*a], &project.outputs[*b]);
        a.stage_y
            .total_cmp(&b.stage_y)
            .then_with(|| a.stage_x.total_cmp(&b.stage_x))
    });
    let [first, second, ..] = order[..] else {
        return false;
    };
    let place = |o: &ProjectorOutput| (o.stage_x, o.stage_y);
    let first_place = place(&project.outputs[first]);
    let second_place = place(&project.outputs[second]);
    for (index, (x, y)) in [(first, second_place), (second, first_place)] {
        project.outputs[index].stage_x = x;
        project.outputs[index].stage_y = y;
    }
    true
}

fn projector_positions_json(runtime: &Runtime) -> String {
    let Some(project) = &runtime.project else {
        return serde_json::json!({ "outputs": [], "identifying": false }).to_string();
    };
    let mut ordered: Vec<_> = project.outputs.iter().enumerate().collect();
    ordered.sort_by(|(ai, a), (bi, b)| {
        a.stage_y.total_cmp(&b.stage_y)
            .then_with(|| a.stage_x.total_cmp(&b.stage_x))
            .then_with(|| ai.cmp(bi))
    });
    let outputs: Vec<_> = ordered.iter().enumerate().map(|(position, (index, output))| {
        serde_json::json!({
            "id": output.id,
            "name": output.name,
            "number": index + 1,
            "position": position,
            "x": output.stage_x,
            "y": output.stage_y
        })
    }).collect();
    serde_json::json!({
        "outputs": outputs,
        "identifying": project.test_pattern == TestPattern::Identify
    }).to_string()
}

/// Sends one of the show's media files to a sub that is missing it.
fn send_media(mut stream: TcpStream, path: &str, shared: &Mutex<Runtime>) {
    let asset = Uuid::parse_str(path.trim_start_matches("/api/media/"))
        .ok()
        .and_then(|id| {
            let runtime = shared.lock().unwrap();
            let project = runtime.project.as_ref()?;
            project.assets.iter().find(|a| a.id == id).cloned()
        });
    let file = asset
        .as_ref()
        .and_then(shareable_media)
        .and_then(|path| fs::File::open(path).ok());
    let Some(file) = file else {
        http_response(stream, "404 Not Found", "text/plain", "No such media here");
        return;
    };
    let length = file.metadata().map_or(0, |m| m.len());
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_ok() {
        let _ = std::io::copy(&mut std::io::BufReader::new(file), &mut stream);
    }
}

/// Stores an uploaded asset in the media cache after checking its SHA-256
/// against the project, then restarts any layer that uses it.
fn receive_media(
    mut stream: TcpStream,
    request: &HttpRequest,
    shared: &Mutex<Runtime>,
    media: &MediaPool,
) {
    let asset = Uuid::parse_str(request.path.trim_start_matches("/api/media/"))
        .ok()
        .and_then(|id| {
            let runtime = shared.lock().unwrap();
            runtime
                .project
                .as_ref()?
                .assets
                .iter()
                .find(|a| a.id == id)
                .cloned()
        });
    let Some(asset) = asset else {
        http_response(
            stream,
            "404 Not Found",
            "text/plain",
            "Asset is not in the loaded show",
        );
        return;
    };
    let Some(_transfer) = Transfer::begin(&asset) else {
        http_response(
            stream,
            "409 Conflict",
            "text/plain",
            "This file is already being received",
        );
        return;
    };
    let target = cached_path(&asset);
    let partial = partial_path(&asset);
    let result = (|| -> std::io::Result<String> {
        fs::create_dir_all(media_cache_dir())?;
        let mut file = std::io::BufWriter::new(fs::File::create(&partial)?);
        let mut hash = Sha256::new();
        read_body(&mut stream, request, |bytes| {
            hash.update(bytes);
            file.write_all(bytes)
        })?;
        file.flush()?;
        Ok(format!("{:x}", hash.finalize()))
    })();
    match result {
        Ok(checksum) if checksum == asset.checksum_sha256 => {
            if fs::rename(&partial, &target).is_err() {
                let _ = fs::remove_file(&partial);
                http_response(
                    stream,
                    "500 Internal Server Error",
                    "text/plain",
                    "Could not store file",
                );
                return;
            }
            let runtime = shared.lock().unwrap();
            if let (Some(project), Some(scene_id)) = (&runtime.project, runtime.state.scene_id) {
                media.sync(project, scene_id, false, runtime.clock.position());
            }
            http_response(stream, "200 OK", "text/plain", "Stored");
        }
        Ok(_) => {
            let _ = fs::remove_file(&partial);
            http_response(
                stream,
                "422 Unprocessable Entity",
                "text/plain",
                "Checksum mismatch",
            );
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            http_response(stream, "400 Bad Request", "text/plain", &e.to_string());
        }
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;

    #[test]
    fn swapping_works_whatever_order_the_projectors_were_added_in() {
        let mut project = ShowProject::default();
        project.outputs[0].stage_x = 1920.0;
        project.outputs[1].stage_x = 0.0;
        for _ in 0..3 {
            let before: Vec<f32> = project.outputs.iter().map(|o| o.stage_x).collect();
            assert!(swap_first_two_positions(&mut project));
            assert_eq!(project.outputs[0].stage_x, before[1]);
            assert_eq!(project.outputs[1].stage_x, before[0]);
        }
        project.outputs.truncate(1);
        assert!(!swap_first_two_positions(&mut project));
    }

    #[test]
    fn each_transfer_writes_its_own_partial_file() {
        let asset = asset_at("clip.mp4", AssetKind::Video, b"");
        let (a, b) = (partial_path(&asset), partial_path(&asset));
        assert_ne!(a, b);
        assert!(a.to_string_lossy().ends_with(".part"));
        let first = Transfer::begin(&asset).unwrap();
        assert!(Transfer::begin(&asset).is_none());
        drop(first);
        assert!(Transfer::begin(&asset).is_some());
    }

    /// An asset whose original file holds `bytes`, or names `name` without
    /// creating it when `bytes` is empty.
    fn asset_at(name: &str, kind: AssetKind, bytes: &[u8]) -> Asset {
        let id = Uuid::new_v4();
        let path = std::env::temp_dir().join(format!("mapforge-test-{id}-{name}"));
        if !bytes.is_empty() {
            fs::write(&path, bytes).unwrap();
        }
        Asset {
            id,
            path: path.to_string_lossy().into_owned(),
            name: name.into(),
            kind,
            checksum_sha256: format!("{:x}", Sha256::digest(bytes)),
            width: None,
            height: None,
            duration_seconds: None,
            size_bytes: Some(bytes.len() as u64),
        }
    }

    #[test]
    fn only_real_show_media_is_sent_to_other_pcs() {
        let video = asset_at("clip.mp4", AssetKind::Video, b"video bytes");
        assert_eq!(shareable_media(&video), Some(PathBuf::from(&video.path)));
        // A show naming some other file: wrong type, kind or contents.
        let text = asset_at("passwords.txt", AssetKind::Video, b"secret");
        assert_eq!(shareable_media(&text), None);
        let unknown = asset_at("notes.mp4", AssetKind::Unknown, b"secret");
        assert_eq!(shareable_media(&unknown), None);
        let mut guessed = asset_at("photo.jpg", AssetKind::Image, b"private photo");
        guessed.checksum_sha256 = "0".repeat(64);
        assert_eq!(shareable_media(&guessed), None);
        for asset in [video, text, unknown, guessed] {
            let _ = fs::remove_file(&asset.path);
        }
    }
}

struct PlayerApp {
    shared: Arc<Mutex<Runtime>>,
    media: Arc<MediaPool>,
    textures: HashMap<Uuid, (egui::TextureHandle, u64, [f32; 4])>,
    show_outputs: bool,
    monitors: Vec<DisplayMonitor>,
    /// When the screens were last listed; projectors plugged in later appear.
    monitors_checked: Option<Instant>,
    /// The master-or-sub form, open on first start or when changing it.
    setup_draft: Option<PlayerSettings>,
    /// This PC's LAN address, shown to the operator.
    ip: String,
    updater: mapforge_core::update::Updater,
}

#[derive(Clone, Copy, PartialEq)]
enum ScreenSource {
    /// Double-clicked on this PC.
    ThisPc,
    /// Set in Producer's Edit projectors.
    Producer,
    /// The next free extra screen.
    Automatic,
}

struct Placement {
    /// The screen to fill; `None` keeps a window.
    display: Option<u32>,
    source: ScreenSource,
    /// A chosen screen that isn't connected, replaced automatically.
    missing: Option<u32>,
}

/// See [`PlayerApp::screen_plan`].
fn plan_screens(
    monitors: &[DisplayMonitor],
    project: &ShowProject,
    assigned: &Option<Vec<Uuid>>,
    chosen: &HashMap<Uuid, Option<u32>>,
) -> HashMap<Uuid, Placement> {
    let connected = |d: u32| monitors.iter().any(|m| m.index == d);
    let mine: Vec<&ProjectorOutput> = project
        .outputs
        .iter()
        .filter(|o| assigned.as_ref().is_none_or(|ids| ids.contains(&o.id)))
        .collect();
    let mut plan = HashMap::new();
    let mut taken = HashSet::new();
    let mut missing = HashMap::new();
    for output in &mine {
        let explicit = match chosen.get(&output.id) {
            Some(choice) => Some((*choice, ScreenSource::ThisPc)),
            None => output
                .display_index
                .map(|d| (Some(d), ScreenSource::Producer)),
        };
        match explicit {
            Some((Some(d), _)) if !connected(d) => {
                missing.insert(output.id, d);
            }
            Some((display, source)) => {
                taken.extend(display);
                plan.insert(
                    output.id,
                    Placement {
                        display,
                        source,
                        missing: None,
                    },
                );
            }
            None => {}
        }
    }
    let automatic: Vec<&ProjectorOutput> = mine
        .iter()
        .copied()
        .filter(|output| !plan.contains_key(&output.id))
        .collect();
    // Keep the primary screen free when there are enough secondary screens.
    // If the show has more outputs, include the primary so every projector can
    // fill a display. Screens are handed out left to right in projector-number
    // order, never by stage position: a projector stays on its screen when it
    // is moved on the canvas, so the picture on that screen changes, which is
    // the whole point of moving it (or of ordering projectors on the iPad).
    let extra_screens = monitors
        .iter()
        .filter(|m| !m.primary && !taken.contains(&m.index))
        .count();
    let include_primary = automatic.len() > extra_screens;
    let mut free: Vec<&DisplayMonitor> = monitors
        .iter()
        .filter(|m| !taken.contains(&m.index) && (include_primary || !m.primary))
        .collect();
    free.sort_by_key(|m| (m.x, m.y));
    for (output, display) in automatic.into_iter().zip(free) {
        plan.insert(
            output.id,
            Placement {
                display: Some(display.index),
                source: ScreenSource::Automatic,
                missing: missing.get(&output.id).copied(),
            },
        );
    }
    // If there are more projectors than displays, keep the remaining outputs
    // as movable preview windows instead of stacking fullscreen windows.
    for output in mine {
        plan.entry(output.id).or_insert_with(|| Placement {
            display: None,
            source: ScreenSource::Automatic,
            missing: missing.get(&output.id).copied(),
        });
    }
    plan
}

impl PlayerApp {
    /// Which screen each of this PC's projectors fills: the one chosen on this
    /// PC, else the one set in Producer, else a free screen matched to the
    /// output's left-to-right position. The primary is kept free when enough
    /// secondary screens exist. A chosen screen that isn't connected is
    /// replaced automatically.
    fn screen_plan(
        &self,
        project: &ShowProject,
        assigned: &Option<Vec<Uuid>>,
        chosen: &HashMap<Uuid, Option<u32>>,
    ) -> HashMap<Uuid, Placement> {
        plan_screens(&self.monitors, project, assigned, chosen)
    }

    /// Lists the screens again and tells Producer about them.
    fn refresh_monitors(&mut self) {
        self.monitors = displays::enumerate();
        self.monitors_checked = Some(Instant::now());
        self.shared.lock().unwrap().state.displays = self
            .monitors
            .iter()
            .map(|m| DisplayInfo {
                index: m.index,
                width: m.width,
                height: m.height,
                primary: m.primary,
            })
            .collect();
    }

    /// Remembers the screen chosen on this PC for a projector: `Some` to
    /// choose (a display, or `None` for a window), `None` to forget it.
    fn choose_display(&mut self, output: Uuid, choice: Option<Option<u32>>) {
        let mut runtime = self.shared.lock().unwrap();
        match choice {
            Some(choice) => runtime.settings.displays.insert(output, choice),
            None => runtime.settings.displays.remove(&output),
        };
        if let Err(e) = setup::save_settings(&runtime.settings) {
            runtime.state.message = format!("Could not save the screen choice: {e}");
        }
    }

    /// Saves the chosen quality and restarts the playing media at it.
    fn set_quality(&mut self, quality: setup::Quality) {
        let mut runtime = self.shared.lock().unwrap();
        runtime.settings.quality = quality;
        if let Err(e) = setup::save_settings(&runtime.settings) {
            runtime.state.message = format!("Could not save the quality setting: {e}");
        }
        self.media.set_quality(quality);
        if let (Some(project), Some(scene_id)) = (&runtime.project, runtime.state.scene_id) {
            self.media
                .sync(project, scene_id, false, runtime.clock.position());
        }
    }

    /// First-start question: is this PC the master or a sub? Returns whether
    /// the form is showing.
    fn setup_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(draft) = self.setup_draft.as_mut() else {
            return false;
        };
        let mut save = false;
        let mut cancel = false;
        ui.add_space(8.0);
        ui.heading("Set up this PC");
        ui.label(format!("This PC's IP: {}", self.ip));
        ui.add_space(8.0);
        ui.radio_value(&mut draft.role, Some(PlayerRole::Master), "Master PC");
        ui.indent("master_help", |ui| {
            ui.label(
                "The iPad connects here. It runs the show and tells the sub PCs what to play.",
            );
        });
        ui.radio_value(&mut draft.role, Some(PlayerRole::Sub), "Sub PC");
        ui.indent("sub_help", |ui| {
            ui.label("Shows its own projectors and follows the master. It copies the show and media from the master.");
        });
        ui.add_space(8.0);
        match draft.role {
            Some(PlayerRole::Master) => {
                ui.checkbox(
                    &mut draft.autoplay,
                    "Start the show automatically when this PC starts",
                );
                ui.label(format!(
                    "Sub PCs enter this IP: {}   ·   iPad: http://{}:{CONTROLLER_PORT}",
                    self.ip, self.ip
                ));
            }
            Some(PlayerRole::Sub) => {
                ui.horizontal(|ui| {
                    ui.label("Master PC IP");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.master)
                            .hint_text("e.g. 192.168.50.11")
                            .desired_width(180.0),
                    );
                });
            }
            None => {}
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(draft.complete(), egui::Button::new("Save"))
                .clicked()
            {
                save = true;
            }
            if self.shared.lock().unwrap().settings.complete() && ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
        if save {
            let mut settings = self.setup_draft.take().unwrap();
            settings.master = settings.master.trim().to_owned();
            let mut runtime = self.shared.lock().unwrap();
            // Screens chosen meanwhile are kept.
            settings.displays = runtime.settings.displays.clone();
            if let Err(e) = setup::save_settings(&settings) {
                runtime.state.message = format!("Could not save setup: {e}");
            }
            runtime.settings = settings;
            runtime.link_note.clear();
        } else if cancel {
            self.setup_draft = None;
        }
        true
    }

    fn update_textures(&mut self, ctx: &egui::Context) {
        let frames = self.media.frames();
        self.textures
            .retain(|id, _| frames.iter().any(|(frame_id, _, _)| frame_id == id));
        for (id, frame, uv) in frames {
            let Some(frame) = frame else {
                continue;
            };
            if self
                .textures
                .get(&id)
                .is_some_and(|(_, sequence, _)| *sequence == frame.sequence)
            {
                continue;
            }
            match self.textures.get_mut(&id) {
                Some((texture, sequence, part)) => {
                    texture.set(frame.image.clone(), egui::TextureOptions::LINEAR);
                    *sequence = frame.sequence;
                    *part = uv;
                }
                None => {
                    let texture = ctx.load_texture(
                        format!("media-{id}"),
                        frame.image.clone(),
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(id, (texture, frame.sequence, uv));
                }
            }
        }
    }
}

/// Maps virtual-stage coordinates into one output window.
struct OutputMapping {
    rect: egui::Rect,
    output_x: f32,
    output_y: f32,
    output_width: f32,
    output_height: f32,
    /// Unit-square to corrected output homography, row-major.
    homography: [f32; 9],
    mode: WarpMode,
    mesh: [[f32; 2]; 9],
}

impl OutputMapping {
    fn new(rect: egui::Rect, output: &ProjectorOutput) -> Self {
        let corners = output.warp_corners.map(|corner| {
            egui::pos2(
                rect.left() + corner[0] * rect.width(),
                rect.top() + corner[1] * rect.height(),
            )
        });
        Self {
            rect,
            output_x: output.stage_x,
            output_y: output.stage_y,
            output_width: output.stage_width,
            output_height: output.stage_height,
            homography: square_to_quad(corners),
            mode: output.warp_mode,
            mesh: output.warp_mesh,
        }
    }

    fn point(&self, x: f32, y: f32) -> egui::Pos2 {
        self.normalized(
            (x - self.output_x) / self.output_width,
            (y - self.output_y) / self.output_height,
        )
    }

    fn normalized(&self, u: f32, v: f32) -> egui::Pos2 {
        if self.mode == WarpMode::None {
            return egui::pos2(
                self.rect.left() + u * self.rect.width(),
                self.rect.top() + v * self.rect.height(),
            );
        }
        if matches!(self.mode, WarpMode::Horizontal | WarpMode::Vertical | WarpMode::Full) {
            let gx = (u.clamp(0.0, 1.0) * 2.0).min(1.999_999);
            let gy = (v.clamp(0.0, 1.0) * 2.0).min(1.999_999);
            let col = gx.floor() as usize;
            let row = gy.floor() as usize;
            let fx = gx - col as f32;
            let fy = gy - row as f32;
            let p00 = self.mesh[row * 3 + col];
            let p10 = self.mesh[row * 3 + col + 1];
            let p01 = self.mesh[(row + 1) * 3 + col];
            let p11 = self.mesh[(row + 1) * 3 + col + 1];
            let x0 = egui::lerp(p00[0]..=p10[0], fx);
            let x1 = egui::lerp(p01[0]..=p11[0], fx);
            let y0 = egui::lerp(p00[1]..=p10[1], fx);
            let y1 = egui::lerp(p01[1]..=p11[1], fx);
            return egui::pos2(
                self.rect.left() + egui::lerp(x0..=x1, fy) * self.rect.width(),
                self.rect.top() + egui::lerp(y0..=y1, fy) * self.rect.height(),
            );
        }
        let h = self.homography;
        let w = h[6] * u + h[7] * v + h[8];
        egui::pos2(
            (h[0] * u + h[1] * v + h[2]) / w,
            (h[3] * u + h[4] * v + h[5]) / w,
        )
    }
}

fn square_to_quad(p: [egui::Pos2; 4]) -> [f32; 9] {
    let (dx1, dx2) = (p[1].x - p[2].x, p[3].x - p[2].x);
    let (dy1, dy2) = (p[1].y - p[2].y, p[3].y - p[2].y);
    let (dx3, dy3) = (
        p[0].x - p[1].x + p[2].x - p[3].x,
        p[0].y - p[1].y + p[2].y - p[3].y,
    );
    let denominator = dx1 * dy2 - dx2 * dy1;
    let (g, h) = if denominator.abs() < 1e-6 {
        (0.0, 0.0)
    } else {
        (
            (dx3 * dy2 - dx2 * dy3) / denominator,
            (dx1 * dy3 - dx3 * dy1) / denominator,
        )
    };
    [
        p[1].x - p[0].x + g * p[1].x,
        p[3].x - p[0].x + h * p[3].x,
        p[0].x,
        p[1].y - p[0].y + g * p[1].y,
        p[3].y - p[0].y + h * p[3].y,
        p[0].y,
        g,
        h,
        1.0,
    ]
}

const IDENTIFY_COLORS: [egui::Color32; 6] = [
    egui::Color32::from_rgb(38, 132, 255),
    egui::Color32::from_rgb(187, 79, 255),
    egui::Color32::from_rgb(30, 190, 130),
    egui::Color32::from_rgb(255, 160, 40),
    egui::Color32::from_rgb(255, 90, 120),
    egui::Color32::from_rgb(80, 210, 230),
];

fn paint_output(
    ui: &egui::Ui,
    index: usize,
    project: &ShowProject,
    state: &PlayerState,
    output: &ProjectorOutput,
    textures: &HashMap<Uuid, (egui::TextureHandle, u64, [f32; 4])>,
) {
    let rect = ui.max_rect();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::BLACK);
    if state.blackout {
        return;
    }
    let map = OutputMapping::new(rect, output);
    match project.test_pattern {
        TestPattern::Off => {
            if state.transport != Transport::Stopped {
                paint_layers(&painter, &map, project, state, output, textures);
            }
        }
        TestPattern::White => {
            paint_warped_color(&painter, &map, egui::Color32::WHITE);
        }
        TestPattern::Gray => {
            paint_warped_color(&painter, &map, egui::Color32::from_gray(128));
        }
        TestPattern::Grid => paint_grid(&painter, &map, project, output),
        TestPattern::Identify => {
            // No blending, so the true projector edges are visible.
            paint_identify(&painter, rect, index, output);
            return;
        }
    }
    paint_correction(&painter, &map, output);
    paint_mask(&painter, rect, output);
}

fn paint_warped_color(painter: &egui::Painter, map: &OutputMapping, color: egui::Color32) {
    paint_warped_rect(painter, map, [0.0, 0.0, 1.0, 1.0], color);
}

/// Fills `part` (left, top, right, bottom as fractions of the output,
/// before warping) with `color`, warped like the picture.
fn paint_warped_rect(
    painter: &egui::Painter,
    map: &OutputMapping,
    part: [f32; 4],
    color: egui::Color32,
) {
    const SUBDIVISIONS: usize = 24;
    let mut mesh = egui::Mesh::default();
    for y in 0..=SUBDIVISIONS {
        let v = egui::lerp(part[1]..=part[3], y as f32 / SUBDIVISIONS as f32);
        for x in 0..=SUBDIVISIONS {
            let u = egui::lerp(part[0]..=part[2], x as f32 / SUBDIVISIONS as f32);
            mesh.colored_vertex(map.normalized(u, v), color);
        }
    }
    let row = (SUBDIVISIONS + 1) as u32;
    for y in 0..SUBDIVISIONS as u32 {
        for x in 0..SUBDIVISIONS as u32 {
            let a = y * row + x;
            mesh.indices.extend_from_slice(&[a, a + 1, a + row + 1, a, a + row + 1, a + row]);
        }
    }
    painter.add(mesh);
}

fn paint_layers(
    painter: &egui::Painter,
    map: &OutputMapping,
    project: &ShowProject,
    state: &PlayerState,
    output: &ProjectorOutput,
    textures: &HashMap<Uuid, (egui::TextureHandle, u64, [f32; 4])>,
) {
    let Some(scene) = state
        .scene_id
        .and_then(|id| project.scenes.iter().find(|s| s.id == id))
    else {
        return;
    };
    let position = state.position_seconds;
    for layer in scene
        .layers
        .iter()
        .filter(|layer| layer.output_ids.contains(&output.id) && layer.active_at(position))
    {
        let is_visual = project
            .assets
            .iter()
            .find(|a| a.id == layer.asset_id)
            .is_some_and(|a| matches!(a.kind, AssetKind::Image | AssetKind::Video));
        if !is_visual {
            continue;
        }
        let left = layer.x.max(output.stage_x);
        let top = layer.y.max(output.stage_y);
        let right = (layer.x + layer.width).min(output.stage_x + output.stage_width);
        let bottom = (layer.y + layer.height).min(output.stage_y + output.stage_height);
        if right <= left || bottom <= top {
            continue;
        }
        // Until the first frame arrives the area stays black.
        if let Some((texture, _, part)) = textures.get(&layer.id) {
            // The texture holds only the part of the media this PC shows
            // (`part`, in whole-media coordinates), so map through it.
            let within = |v: f32, lo: f32, hi: f32| (v - lo) / (hi - lo).max(1e-6);
            let uv = egui::Rect::from_min_max(
                egui::pos2(
                    within((left - layer.x) / layer.width, part[0], part[2]),
                    within((top - layer.y) / layer.height, part[1], part[3]),
                ),
                egui::pos2(
                    within((right - layer.x) / layer.width, part[0], part[2]),
                    within((bottom - layer.y) / layer.height, part[1], part[3]),
                ),
            );
            paint_warped_image(
                painter,
                map,
                texture.id(),
                [left, top, right, bottom],
                uv,
                egui::Color32::WHITE.gamma_multiply(layer.opacity),
            );
        }
    }
}

fn paint_warped_image(
    painter: &egui::Painter,
    map: &OutputMapping,
    texture_id: egui::TextureId,
    stage: [f32; 4],
    uv: egui::Rect,
    tint: egui::Color32,
) {
    const SUBDIVISIONS: usize = 16;
    let mut mesh = egui::Mesh::with_texture(texture_id);
    for y in 0..=SUBDIVISIONS {
        let fy = y as f32 / SUBDIVISIONS as f32;
        for x in 0..=SUBDIVISIONS {
            let fx = x as f32 / SUBDIVISIONS as f32;
            mesh.vertices.push(egui::epaint::Vertex {
                pos: map.point(
                    egui::lerp(stage[0]..=stage[2], fx),
                    egui::lerp(stage[1]..=stage[3], fy),
                ),
                uv: egui::pos2(egui::lerp(uv.x_range(), fx), egui::lerp(uv.y_range(), fy)),
                color: tint,
            });
        }
    }
    let row = (SUBDIVISIONS + 1) as u32;
    for y in 0..SUBDIVISIONS as u32 {
        for x in 0..SUBDIVISIONS as u32 {
            let a = y * row + x;
            mesh.indices
                .extend_from_slice(&[a, a + 1, a + row + 1, a, a + row + 1, a + row]);
        }
    }
    painter.add(mesh);
}

fn paint_mask(painter: &egui::Painter, rect: egui::Rect, output: &ProjectorOutput) {
    if !output.mask.enabled || output.mask.points.len() < 3 {
        return;
    }
    let mut coordinates = vec![
        rect.left() as f64,
        rect.top() as f64,
        rect.right() as f64,
        rect.top() as f64,
        rect.right() as f64,
        rect.bottom() as f64,
        rect.left() as f64,
        rect.bottom() as f64,
    ];
    for point in &output.mask.points {
        coordinates.push((rect.left() + point[0] * rect.width()) as f64);
        coordinates.push((rect.top() + point[1] * rect.height()) as f64);
    }
    let Ok(indices) = earcutr::earcut(&coordinates, &[4], 2) else {
        return;
    };
    let mut mesh = egui::Mesh::default();
    for point in coordinates.chunks_exact(2) {
        mesh.colored_vertex(
            egui::pos2(point[0] as f32, point[1] as f32),
            egui::Color32::BLACK,
        );
    }
    mesh.indices
        .extend(indices.into_iter().map(|index| index as u32));
    painter.add(mesh);
}

/// Number, name, Player address and resolution, with a border, corner
/// marks and a centre cross for physical alignment.
fn paint_identify(
    painter: &egui::Painter,
    rect: egui::Rect,
    index: usize,
    output: &ProjectorOutput,
) {
    let color = IDENTIFY_COLORS[index % IDENTIFY_COLORS.len()];
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(12));
    let border = (rect.height() * 0.015).max(3.0);
    painter.rect_stroke(
        rect.shrink(border / 2.0),
        0.0,
        egui::Stroke::new(border, color),
        egui::StrokeKind::Middle,
    );
    let mark = rect.height().min(rect.width()) * 0.12;
    let stroke = egui::Stroke::new(border, egui::Color32::WHITE);
    for (corner, dx, dy) in [
        (rect.left_top(), 1.0, 1.0),
        (rect.right_top(), -1.0, 1.0),
        (rect.left_bottom(), 1.0, -1.0),
        (rect.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + egui::vec2(mark * dx, 0.0)], stroke);
        painter.line_segment([corner, corner + egui::vec2(0.0, mark * dy)], stroke);
    }
    let center = rect.center();
    let thin = egui::Stroke::new(1.5_f32, egui::Color32::from_gray(150));
    painter.line_segment(
        [
            egui::pos2(rect.left(), center.y),
            egui::pos2(rect.right(), center.y),
        ],
        thin,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, rect.top()),
            egui::pos2(center.x, rect.bottom()),
        ],
        thin,
    );
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        (index + 1).to_string(),
        egui::FontId::proportional(rect.height() * 0.5),
        color,
    );
    let details = format!(
        "{}\n{}\n{} × {} px",
        output.name,
        output.player_address(),
        output.resolution[0],
        output.resolution[1]
    );
    painter.text(
        egui::pos2(center.x, rect.bottom() - rect.height() * 0.08),
        egui::Align2::CENTER_BOTTOM,
        details,
        egui::FontId::proportional((rect.height() * 0.05).clamp(12.0, 40.0)),
        egui::Color32::WHITE,
    );
}

/// A stage-space grid, so lines continue across projectors for alignment.
fn paint_grid(
    painter: &egui::Painter,
    map: &OutputMapping,
    project: &ShowProject,
    output: &ProjectorOutput,
) {
    let stage = &project.stage;
    let stroke = egui::Stroke::new(1.5_f32, egui::Color32::WHITE);
    let columns = 16;
    let rows = 9;
    // Everything is drawn only where it crosses this output. Curved warps
    // pin points outside the output to its border, which would draw false
    // grid lines all along the edges.
    let (x0, y0) = (output.stage_x, output.stage_y);
    let (x1, y1) = (x0 + output.stage_width, y0 + output.stage_height);
    let inside = |x: f32, y: f32| x >= x0 && x <= x1 && y >= y0 && y <= y1;
    let (top, bottom) = (y0.max(0.0), y1.min(stage.height));
    let (left, right) = (x0.max(0.0), x1.min(stage.width));
    for i in 0..=columns {
        let x = stage.width * i as f32 / columns as f32;
        if x < x0 || x > x1 || bottom <= top {
            continue;
        }
        let points: Vec<_> = (0..=48)
            .map(|step| map.point(x, egui::lerp(top..=bottom, step as f32 / 48.0)))
            .collect();
        painter.add(egui::Shape::line(points, stroke));
    }
    for i in 0..=rows {
        let y = stage.height * i as f32 / rows as f32;
        if y < y0 || y > y1 || right <= left {
            continue;
        }
        let points: Vec<_> = (0..=48)
            .map(|step| map.point(egui::lerp(left..=right, step as f32 / 48.0), y))
            .collect();
        painter.add(egui::Shape::line(points, stroke));
    }
    let circle_stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 200, 0));
    let mut run = Vec::new();
    for step in 0..=192 {
        let angle = std::f32::consts::TAU * step as f32 / 192.0;
        let x = stage.width / 2.0 + angle.cos() * stage.height * 0.4;
        let y = stage.height / 2.0 + angle.sin() * stage.height * 0.4;
        if inside(x, y) {
            run.push(map.point(x, y));
        } else if run.len() > 1 {
            painter.add(egui::Shape::line(std::mem::take(&mut run), circle_stroke));
        } else {
            run.clear();
        }
    }
    if run.len() > 1 {
        painter.add(egui::Shape::line(run, circle_stroke));
    }
    painter.text(
        map.rect.left_top() + egui::vec2(12.0, 12.0),
        egui::Align2::LEFT_TOP,
        &output.name,
        egui::FontId::proportional(20.0),
        egui::Color32::from_rgb(255, 200, 0),
    );
}

/// Black-level lift, edge-blend feathers and brightness, in that order.
/// Feather widths are stage pixels of content, so the lift and feathers
/// are laid out before warping and go through the same mapping as the
/// picture: on a keystoned or curved output they still cover exactly the
/// overlapping content.
fn paint_correction(painter: &egui::Painter, map: &OutputMapping, output: &ProjectorOutput) {
    let rect = map.rect;
    let blend = &output.blend;
    let share = |width: f32, of: f32| {
        if of > 0.0 {
            (width / of).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    let left = share(blend.left, output.stage_width);
    let right = share(blend.right, output.stage_width);
    let top = share(blend.top, output.stage_height);
    let bottom = share(blend.bottom, output.stage_height);

    if output.color.black_lift > 0.0 && left + right < 1.0 && top + bottom < 1.0 {
        let alpha = (output.color.black_lift * 255.0).round() as u8;
        paint_warped_rect(
            painter,
            map,
            [left, top, 1.0 - right, 1.0 - bottom],
            egui::Color32::from_white_alpha(alpha),
        );
    }

    for (edge, share, pixels) in [
        (0, left, left * rect.width()),
        (1, right, right * rect.width()),
        (2, top, top * rect.height()),
        (3, bottom, bottom * rect.height()),
    ] {
        if pixels > 0.5 {
            painter.add(feather_mesh(map, blend, edge, share));
        }
    }
    if output.color.brightness < 1.0 {
        let alpha = ((1.0 - output.color.brightness) * 255.0).round() as u8;
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(alpha));
    }
}

/// One edge's feather (0 left, 1 right, 2 top, 3 bottom) covering `share`
/// of the output, as one gradient mesh, since separate strips leave
/// visible seams. It is subdivided along the edge too, so curved warps are
/// followed.
fn feather_mesh(
    map: &OutputMapping,
    blend: &mapforge_core::EdgeBlend,
    edge: usize,
    share: f32,
) -> egui::Mesh {
    const STEPS: usize = 96;
    const ALONG: usize = 16;
    let row = (ALONG + 1) as u32;
    let mut mesh = egui::Mesh::default();
    for step in 0..=STEPS {
        let t = step as f32 / STEPS as f32;
        let alpha = ((1.0 - blend.pixel(t)) * 255.0).round() as u8;
        let color = egui::Color32::from_black_alpha(alpha);
        let across = t * share;
        for k in 0..=ALONG {
            let along = k as f32 / ALONG as f32;
            let (u, v) = match edge {
                0 => (across, along),
                1 => (1.0 - across, along),
                2 => (along, across),
                _ => (along, 1.0 - across),
            };
            mesh.colored_vertex(map.normalized(u, v), color);
        }
        if step > 0 {
            let (a, b) = ((step as u32 - 1) * row, step as u32 * row);
            for k in 0..ALONG as u32 {
                mesh.add_triangle(a + k, a + k + 1, b + k + 1);
                mesh.add_triangle(a + k, b + k + 1, b + k);
            }
        }
    }
    mesh
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.update_textures(ctx);
        ctx.request_repaint_after(Duration::from_millis(16));
        if mapforge_core::update::bubble::show(ctx, &mut self.updater, None) {
            // The installer is open and replaces the programs once we close.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self
            .monitors_checked
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(3))
        {
            self.refresh_monitors();
        }
        let (project, state, assigned, chosen) = {
            let mut runtime = self.shared.lock().unwrap();
            (
                runtime.project.clone(),
                runtime.snapshot(),
                runtime.outputs.clone(),
                runtime.settings.displays.clone(),
            )
        };
        let mut reset_choice = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!(
                    "MapForge Player {}",
                    mapforge_core::update::current_version()
                ));
                if ui.small_button("Check for updates").clicked() {
                    self.updater.check_now();
                }
            });
            ui.label("Producer: TCP 4777   Controller: HTTP 8080");
            if self.setup_ui(ui) {
                return;
            }
            let (settings, plays_audio, link_note) = {
                let runtime = self.shared.lock().unwrap();
                (
                    runtime.settings.clone(),
                    runtime.plays_audio(),
                    runtime.link_note.clone(),
                )
            };
            ui.horizontal_wrapped(|ui| {
                let role = match settings.role {
                    Some(PlayerRole::Master) => format!(
                        "MASTER PC · {} · iPad: http://{}:{CONTROLLER_PORT}",
                        if settings.autoplay {
                            "starts the show by itself"
                        } else {
                            "waits for Play"
                        },
                        self.ip
                    ),
                    Some(PlayerRole::Sub) => {
                        format!("SUB PC · follows the master at {}", settings.master)
                    }
                    None => "Not set up".to_owned(),
                };
                ui.strong(role);
                if ui.small_button("Change setup").clicked() {
                    self.setup_draft = Some(settings.clone());
                }
            });
            ui.label(format!("This PC's IP: {}", self.ip));
            if settings.role == Some(PlayerRole::Master) {
                let followers: Vec<String> = state
                    .followers
                    .iter()
                    .map(|f| {
                        format!(
                            "{} {}",
                            player_host(&f.address),
                            if f.online { "online" } else { "OFFLINE" }
                        )
                    })
                    .collect();
                if !followers.is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(31, 157, 98),
                        format!("Sub PCs: {}", followers.join(", ")),
                    );
                }
                if state.followers.iter().any(|f| !f.online) {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "A sub PC is offline; it joins the show when it answers.",
                    );
                }
            }
            if !link_note.is_empty() {
                ui.label(link_note);
            }
            if !plays_audio {
                ui.label("Sound: off on this PC (it plays on the master)");
            }
            ui.separator();
            let scene_name = project
                .as_ref()
                .zip(state.scene_id)
                .and_then(|(p, id)| p.scenes.iter().find(|s| s.id == id))
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "No scene".into());
            ui.label(format!(
                "{:?} · {} · {:.1}s — {}",
                state.transport, scene_name, state.position_seconds, state.message
            ));
            {
                let mut runtime = self.shared.lock().unwrap();
                ui.horizontal(|ui| {
                    ui.label("Volume");
                    ui.add(egui::Slider::new(&mut runtime.state.volume, 0.0..=1.0));
                    ui.checkbox(&mut runtime.state.muted, "Mute");
                    ui.checkbox(&mut runtime.state.blackout, "Blackout");
                });
            }
            let (videos, sounds, errors, decodes) = self.media.summary();
            for error in errors {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
            ui.label(format!(
                "{videos} picture layer(s) · {sounds} sound layer(s)"
            ));
            for decode in decodes {
                ui.small(decode);
            }
            ui.horizontal(|ui| {
                ui.label("Video quality");
                let mut quality = settings.quality;
                egui::ComboBox::from_id_salt("video-quality")
                    .selected_text(quality.label())
                    .show_ui(ui, |ui| {
                        for choice in setup::Quality::ALL {
                            ui.selectable_value(&mut quality, choice, choice.label());
                        }
                    });
                if quality != settings.quality {
                    self.set_quality(quality);
                }
                ui.label(format!(
                    "(this GPU shows pictures up to {} px)",
                    texture_limit()
                ))
                .on_hover_text(
                    "Each PC decodes only the part of the picture its projectors show, \
                     at the media's own resolution. Lower the quality only if playback \
                     stutters on this PC.",
                );
            });
            if self.media.audio_output.is_none() {
                ui.colored_label(egui::Color32::YELLOW, "No audio output device found");
            }
            if let Some(project) = &project {
                if project.test_pattern != TestPattern::Off {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!("Test pattern: {:?}", project.test_pattern),
                    );
                }
            }
            if let Some(project) = &project {
                let mine: Vec<_> = project
                    .outputs
                    .iter()
                    .filter(|o| assigned.as_ref().is_none_or(|ids| ids.contains(&o.id)))
                    .map(|o| o.name.as_str())
                    .collect();
                ui.label(format!("Projectors on this PC: {}", mine.join(", ")));
                // Where each projector's picture is, and why when it's a window.
                let plan = self.screen_plan(project, &assigned, &chosen);
                for output in project
                    .outputs
                    .iter()
                    .filter(|o| assigned.as_ref().is_none_or(|ids| ids.contains(&o.id)))
                {
                    let Some(placed) = plan.get(&output.id) else {
                        continue;
                    };
                    let screen = placed
                        .display
                        .and_then(|d| self.monitors.iter().find(|m| m.index == d));
                    let mut text = match (screen, placed.source) {
                        (Some(m), source) => format!(
                            "{}: fullscreen on Display {} ({}×{}){}",
                            output.name,
                            m.index + 1,
                            m.width,
                            m.height,
                            match source {
                                ScreenSource::ThisPc => ", chosen on this PC",
                                ScreenSource::Producer => "",
                                ScreenSource::Automatic => ", automatic",
                            }
                        ),
                        (None, ScreenSource::ThisPc) => {
                            format!("{}: window, chosen on this PC", output.name)
                        }
                        (None, _) if self.monitors.len() <= 1 => format!(
                            "{}: window. Only the main screen is connected; plug in the \
                             projector as an extended display.",
                            output.name
                        ),
                        (None, _) => format!(
                            "{}: window. No free screen; drag it onto a screen and \
                             double-click it to fill that screen.",
                            output.name
                        ),
                    };
                    if let Some(missing) = placed.missing {
                        text += &format!(" (Display {} is not connected)", missing + 1);
                    }
                    let color = if screen.is_some() {
                        egui::Color32::from_rgb(31, 157, 98)
                    } else {
                        egui::Color32::YELLOW
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(color, text);
                        if placed.source == ScreenSource::ThisPc
                            && ui
                                .small_button("Automatic")
                                .on_hover_text("Forget the screen chosen on this PC")
                                .clicked()
                        {
                            reset_choice = Some(output.id);
                        }
                    });
                }
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Screens on this PC: {}", self.monitors.len()));
                if ui.small_button("Refresh").clicked() {
                    self.refresh_monitors();
                }
            });
            for monitor in &self.monitors {
                ui.small(format!(
                    "Display {}: {}×{} at {},{}{}",
                    monitor.index + 1,
                    monitor.width,
                    monitor.height,
                    monitor.x,
                    monitor.y,
                    if monitor.primary {
                        " (main screen)"
                    } else {
                        ""
                    }
                ));
            }
            ui.label(format!("Media cache: {}", media_cache_dir().display()));
            ui.checkbox(&mut self.show_outputs, "Open projector outputs");
            ui.label("Closing Producer or the controller does not stop this Player.");
        });

        if let Some(id) = reset_choice {
            self.choose_display(id, None);
        }
        if !self.show_outputs {
            return;
        }
        let Some(project) = project else {
            return;
        };
        let mut clicked = Vec::new();
        let plan = self.screen_plan(&project, &assigned, &chosen);
        for (index, output) in project.outputs.iter().enumerate() {
            if assigned
                .as_ref()
                .is_some_and(|ids| !ids.contains(&output.id))
            {
                continue;
            }
            let monitor = plan
                .get(&output.id)
                .and_then(|placed| placed.display)
                .and_then(|display| self.monitors.iter().find(|m| m.index == display));
            let aspect = output.stage_height / output.stage_width;
            let preview_width = 640.0_f32;
            let preview_height = (preview_width * aspect).clamp(120.0, 900.0);
            let mut viewport = egui::ViewportBuilder::default().with_title(match monitor {
                Some(monitor) => format!(
                    "MapForge Output {} — {} — Display {}",
                    index + 1,
                    output.name,
                    monitor.index + 1
                ),
                None => format!("MapForge Preview {} — {}", index + 1, output.name),
            });
            // An assigned output opens small in the middle of its display and
            // then switches to real fullscreen there. Fullscreen always covers
            // the whole display at its native pixels, whatever Windows'
            // scaling of each screen; a window sized to the display would be
            // resized by Windows when it lands on a screen with other scaling.
            viewport = if let Some(monitor) = monitor {
                // New windows are placed in points of the primary display.
                let scale = self
                    .monitors
                    .iter()
                    .find(|m| m.primary)
                    .map_or(1.0, |m| m.scale)
                    * ctx.zoom_factor();
                let center_x = monitor.x as f32 + monitor.width as f32 / 2.0;
                let center_y = monitor.y as f32 + monitor.height as f32 / 2.0;
                viewport
                    .with_position([center_x / scale - 160.0, center_y / scale - 90.0])
                    .with_inner_size([320.0, 180.0])
                    .with_decorations(false)
                    .with_always_on_top()
            } else {
                viewport.with_inner_size([preview_width, preview_height])
            };
            let textures = &self.textures;
            let project = &project;
            let state = &state;
            let fullscreen = monitor.is_some();
            // A new display assignment opens a new window rather than moving
            // the old one, so it starts on the right display.
            let placement = monitor.map(|m| (m.index, m.x, m.y, m.width, m.height));
            let clicked_here = ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of(("output", output.id, placement)),
                viewport,
                move |ctx, _| {
                    if fullscreen && ctx.input(|i| i.viewport().fullscreen) != Some(true) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    }
                    // Double-click: fill the screen this window is on, or go
                    // back to a window. The window's centre, in desktop
                    // pixels, says which screen it is on.
                    let double = ctx.input(|i| {
                        i.pointer
                            .button_double_clicked(egui::PointerButton::Primary)
                            || (fullscreen && i.key_pressed(egui::Key::Escape))
                    });
                    let click = double.then(|| {
                        if fullscreen {
                            None
                        } else {
                            let ppp = ctx.pixels_per_point();
                            ctx.input(|i| i.viewport().outer_rect)
                                .map(|r| (r.center().x * ppp, r.center().y * ppp))
                        }
                    });
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                        .show(ctx, |ui| {
                            paint_output(ui, index, project, state, output, textures)
                        });
                    // A mouse left on a projector screen must not show up
                    // on the projection.
                    if fullscreen {
                        ctx.set_cursor_icon(egui::CursorIcon::None);
                    }
                    click
                },
            );
            if let Some(click) = clicked_here {
                clicked.push((output.id, click));
            }
        }
        for (id, click) in clicked {
            match click {
                None => self.choose_display(id, Some(None)),
                Some((x, y)) => {
                    let screen = self.monitors.iter().find(|m| {
                        x >= m.x as f32
                            && y >= m.y as f32
                            && x < (m.x + m.width as i32) as f32
                            && y < (m.y + m.height as i32) as f32
                    });
                    if let Some(screen) = screen {
                        self.choose_display(id, Some(Some(screen.index)));
                    }
                }
            }
        }
        wait_for_refresh();
    }
}

/// Waits once per round of output windows for the desktop to show its next
/// refresh, so frames are evenly paced without vsync on every window.
#[cfg(windows)]
fn wait_for_refresh() {
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmFlush() -> i32;
    }
    // SAFETY: DwmFlush takes no arguments and only blocks until the desktop
    // compositor's next frame; a failure just returns an error code.
    unsafe {
        DwmFlush();
    }
}

#[cfg(not(windows))]
fn wait_for_refresh() {}

/// Keeps Windows from turning the screens off, starting a screen saver or
/// going to sleep while the Player is open. A show PC gets no mouse or
/// keyboard input for hours, so without this the projectors go dark after
/// the PC's display timeout while the show keeps playing unseen. It lasts
/// as long as the calling thread, so it is called on the main thread.
#[cfg(windows)]
fn keep_screens_awake() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    // SAFETY: takes plain flags and only changes this thread's power request.
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
}

#[cfg(not(windows))]
fn keep_screens_awake() {}

fn main() -> eframe::Result<()> {
    keep_screens_awake();
    let shared = Arc::new(Mutex::new(Runtime {
        settings: setup::load_settings(),
        port: port_setting("MAPFORGE_PORT", DEFAULT_PLAYER_PORT),
        local_ips: setup::local_ip().into_iter().collect(),
        ..Runtime::default()
    }));
    let quality = shared.lock().unwrap().settings.quality;
    let media = Arc::new(MediaPool {
        slots: Mutex::new(HashMap::new()),
        standby: Mutex::new(None),
        cued: Mutex::new(None),
        audio_output: open_audio_output(),
        view: Mutex::new(DecodeView {
            outputs: None,
            max_side: quality.max_side(),
        }),
    });
    {
        let s = shared.clone();
        let m = media.clone();
        thread::spawn(move || protocol_server(s, m));
    }
    let relay = Arc::new(Relay::default());
    {
        let s = shared.clone();
        let m = media.clone();
        let r = relay.clone();
        thread::spawn(move || http_server(s, m, r));
    }
    {
        let s = shared.clone();
        let r = relay.clone();
        thread::spawn(move || r.run(s));
    }
    if open_saved_show(&shared, &media) {
        let s = shared.clone();
        let m = media.clone();
        let r = relay.clone();
        thread::spawn(move || relay::autoplay(s, m, r));
    }
    {
        let s = shared.clone();
        let m = media.clone();
        thread::spawn(move || scheduler(s, m));
    }
    {
        let s = shared.clone();
        let m = media.clone();
        thread::spawn(move || setup::follow_master(s, m));
    }
    let setup_draft = {
        let settings = shared.lock().unwrap().settings.clone();
        (!settings.complete()).then(|| PlayerSettings {
            autoplay: true,
            ..settings
        })
    };
    eframe::run_native(
        "MapForge Player",
        eframe::NativeOptions {
            // Every projector output is its own window, and with vsync each
            // window's buffer swap would wait for a refresh in turn, so two
            // outputs would run at a third of the refresh rate. Frames are
            // paced once per round instead (`wait_for_refresh`).
            vsync: false,
            ..Default::default()
        },
        Box::new(|cc| {
            // Media is decoded at the largest size this GPU can show.
            if let Some(gl) = &cc.gl {
                use eframe::glow::HasContext;
                let side = unsafe { gl.get_parameter_i32(eframe::glow::MAX_TEXTURE_SIZE) };
                if side >= 2048 {
                    TEXTURE_LIMIT.store(side as u32, Ordering::Relaxed);
                }
            }
            let ctx = cc.egui_ctx.clone();
            let updater = mapforge_core::update::Updater::start(move || ctx.request_repaint());
            Ok(Box::new(PlayerApp {
                updater,
                shared,
                media,
                textures: HashMap::new(),
                show_outputs: true,
                monitors: displays::enumerate(),
                monitors_checked: None,
                setup_draft,
                ip: setup::local_ip().unwrap_or_else(|| "unknown".into()),
            }))
        }),
    )
}

#[cfg(test)]
mod decode_size_tests {
    use super::fit_within;

    #[test]
    fn media_that_fits_keeps_its_own_size() {
        assert_eq!(fit_within(3840, 2160, 8192), (3840, 2160));
        assert_eq!(fit_within(7680, 1080, 8192), (7680, 1080));
        assert_eq!(fit_within(1080, 1920, 8192), (1080, 1920));
    }

    #[test]
    fn oversized_media_shrinks_to_the_limit_keeping_its_shape() {
        assert_eq!(fit_within(11520, 1080, 8192), (8192, 768));
        assert_eq!(fit_within(1080, 11520, 8192), (768, 8192));
        assert_eq!(fit_within(16000, 16000, 4096), (4096, 4096));
    }

    #[test]
    fn sizes_are_even_and_never_zero() {
        assert_eq!(fit_within(1919, 1079, 8192), (1920, 1080));
        assert_eq!(fit_within(1, 1, 8192), (2, 2));
    }
}

#[cfg(test)]
mod crop_tests {
    use super::*;

    /// A 10600×1080 panorama laid over a 10400 px stage, like a travelling
    /// show, with two 1920 px projectors at the left end.
    fn panorama() -> (ShowProject, Layer, Asset) {
        let mut project = ShowProject::default();
        project.outputs[0].stage_x = 0.0;
        project.outputs[1].stage_x = 1766.0;
        project.lock_output_sizes();
        let asset = Asset {
            id: Uuid::new_v4(),
            path: "pan.mov".into(),
            name: "pan.mov".into(),
            kind: AssetKind::Video,
            checksum_sha256: String::new(),
            width: Some(10600),
            height: Some(1080),
            duration_seconds: Some(60.0),
            size_bytes: None,
        };
        let layer = Layer {
            id: Uuid::new_v4(),
            asset_id: asset.id,
            name: "pan".into(),
            x: -100.0,
            y: 0.0,
            width: 10600.0,
            height: 1080.0,
            opacity: 1.0,
            output_ids: project.outputs.iter().map(|o| o.id).collect(),
            timeline_start: 0.0,
            timeline_duration: 60.0,
            source_offset: 0.0,
            looping: true,
            volume: 1.0,
            audio: true,
        };
        (project, layer, asset)
    }

    #[test]
    fn only_the_shown_part_of_a_panorama_is_decoded() {
        let (project, layer, asset) = panorama();
        let outputs: Vec<&ProjectorOutput> = project.outputs.iter().collect();
        let spec = LayerSpec::new(&layer, &asset, &outputs, u32::MAX);
        let [x, y, w, h] = spec.crop.expect("a slice of the panorama");
        // Stage 0..3686 is media pixels 100..3786, plus a small margin.
        assert_eq!((y, h), (0, 1080));
        assert!(x <= 100 && x >= 90, "x = {x}");
        assert!(x + w >= 3786 && x + w <= 3800, "right = {}", x + w);
        assert_eq!((x % 2, w % 2), (0, 0));
        assert!(!spec.hidden);
        assert!((spec.uv[0] - x as f32 / 10600.0).abs() < 1e-6);
        assert!((spec.uv[2] - (x + w) as f32 / 10600.0).abs() < 1e-6);
    }

    #[test]
    fn a_pc_whose_projectors_do_not_show_a_layer_skips_its_picture() {
        let (project, mut layer, asset) = panorama();
        layer.output_ids.clear();
        let outputs: Vec<&ProjectorOutput> = project.outputs.iter().collect();
        let spec = LayerSpec::new(&layer, &asset, &outputs, u32::MAX);
        assert!(spec.hidden);
        assert_eq!(spec.crop, None);
    }

    #[test]
    fn a_layer_inside_the_projectors_is_decoded_whole() {
        let (project, mut layer, asset) = panorama();
        layer.x = 200.0;
        layer.width = 1000.0;
        layer.height = 500.0;
        let outputs: Vec<&ProjectorOutput> = project.outputs.iter().collect();
        let spec = LayerSpec::new(&layer, &asset, &outputs, u32::MAX);
        assert_eq!(spec.crop, None);
        assert_eq!(spec.uv, [0.0, 0.0, 1.0, 1.0]);
        assert!(!spec.hidden);
    }

    #[test]
    fn the_quality_setting_caps_the_decode_size() {
        let (project, layer, asset) = panorama();
        let outputs: Vec<&ProjectorOutput> = project.outputs.iter().collect();
        let spec = LayerSpec::new(&layer, &asset, &outputs, setup::Quality::Hd.max_side());
        assert_eq!(spec.max_side, 1920);
        let [_, _, w, h] = spec.crop.unwrap();
        let (dw, dh) = fit_within(w, h, spec.max_side);
        assert_eq!(dw, 1920);
        assert!(dh < 1080 && dh % 2 == 0);
    }
}

#[cfg(test)]
mod output_mapping_tests {
    use super::*;

    #[test]
    fn homography_hits_all_four_corners() {
        let target = [
            egui::pos2(20.0, 10.0),
            egui::pos2(180.0, 30.0),
            egui::pos2(160.0, 110.0),
            egui::pos2(5.0, 90.0),
        ];
        let map = OutputMapping {
            rect: egui::Rect::NOTHING,
            output_x: 0.0,
            output_y: 0.0,
            output_width: 1.0,
            output_height: 1.0,
            homography: square_to_quad(target),
            mode: WarpMode::Perspective,
            mesh: [
                [0.0, 0.0], [0.5, 0.0], [1.0, 0.0],
                [0.0, 0.5], [0.5, 0.5], [1.0, 0.5],
                [0.0, 1.0], [0.5, 1.0], [1.0, 1.0],
            ],
        };
        for ((u, v), expected) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]
            .into_iter()
            .zip(target)
        {
            let actual = map.normalized(u, v);
            assert!((actual.x - expected.x).abs() < 0.001);
            assert!((actual.y - expected.y).abs() < 0.001);
        }
    }

    #[test]
    fn horizontal_mesh_moves_middle_row_and_keeps_edges() {
        let mut output = ShowProject::default().outputs.remove(0);
        output.warp_mode = WarpMode::Horizontal;
        output.warp_mesh[4][1] = 0.6;
        let map = OutputMapping::new(
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0)),
            &output,
        );
        let center = map.normalized(0.5, 0.5);
        assert!((center.x - 50.0).abs() < 0.001);
        assert!((center.y - 60.0).abs() < 0.001);
        let tl = map.normalized(0.0, 0.0);
        let br = map.normalized(1.0, 1.0);
        assert!(tl.distance(egui::pos2(0.0, 0.0)) < 0.001);
        assert!(br.distance(egui::pos2(100.0, 100.0)) < 0.001);
    }

    #[test]
    fn vertical_mesh_moves_middle_column_and_keeps_edges() {
        let mut output = ShowProject::default().outputs.remove(0);
        output.warp_mode = WarpMode::Vertical;
        output.warp_mesh[4][0] = 0.6;
        let map = OutputMapping::new(
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0)),
            &output,
        );
        let center = map.normalized(0.5, 0.5);
        assert!((center.x - 60.0).abs() < 0.001);
        assert!((center.y - 50.0).abs() < 0.001);
        let tl = map.normalized(0.0, 0.0);
        let br = map.normalized(1.0, 1.0);
        assert!(tl.distance(egui::pos2(0.0, 0.0)) < 0.001);
        assert!(br.distance(egui::pos2(100.0, 100.0)) < 0.001);
    }

    #[test]
    fn full_mesh_applies_an_interior_point_and_none_bypasses_warp() {
        let mut output = ShowProject::default().outputs.remove(0);
        output.warp_mode = WarpMode::Full;
        output.warp_mesh[4] = [0.6, 0.6];
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let full = OutputMapping::new(rect, &output);
        assert!(full.normalized(0.5, 0.5).distance(egui::pos2(60.0, 60.0)) < 0.001);

        output.warp_mode = WarpMode::None;
        output.warp_corners = [[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]];
        let none = OutputMapping::new(rect, &output);
        assert!(none.normalized(0.0, 0.0).distance(egui::pos2(0.0, 0.0)) < 0.001);
        assert!(none.normalized(1.0, 1.0).distance(egui::pos2(100.0, 100.0)) < 0.001);
    }

    #[test]
    fn scheduled_start_compensates_for_lateness() {
        let project = ShowProject::default();
        let scene_id = project.scenes[0].id;
        let media = MediaPool::default();
        let mut runtime = Runtime {
            project: Some(project),
            ..Default::default()
        };
        runtime.schedule(&media, scene_id, 2.0, unix_time_ms().saturating_sub(25));
        runtime.handle_scheduled_start(&media);
        assert_eq!(runtime.state.transport, Transport::Playing);
        assert!(runtime.clock.position() >= 2.02);
        assert!(runtime
            .state
            .start_error_ms
            .is_some_and(|error| error >= 20.0));
        assert!(runtime.scheduled.is_none());
    }
}

#[cfg(test)]
mod screen_plan_tests {
    use super::*;

    fn screen(index: u32, x: i32, primary: bool) -> DisplayMonitor {
        DisplayMonitor {
            index,
            x,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
            scale: 1.0,
        }
    }

    #[test]
    fn projectors_fill_extra_screens_left_to_right() {
        let project = ShowProject::default(); // two projectors
        let [a, b] = [project.outputs[0].id, project.outputs[1].id];
        // Windows numbers don't follow the desk order: 3 is left of 2.
        let monitors = [
            screen(0, 0, true),
            screen(2, 3840, false),
            screen(1, 1920, false),
        ];
        let plan = plan_screens(&monitors, &project, &None, &HashMap::new());
        assert_eq!(plan[&a].display, Some(1));
        assert_eq!(plan[&b].display, Some(2));
        assert!(plan[&a].source == ScreenSource::Automatic);

        // With no extra screen, the first output fills the primary and the
        // second remains a movable preview window.
        let plan = plan_screens(&monitors[..1], &project, &None, &HashMap::new());
        assert_eq!(plan[&a].display, Some(0));
        assert_eq!(plan[&b].display, None);
    }

    #[test]
    fn moving_a_projector_on_the_canvas_keeps_it_on_its_screen() {
        let mut project = ShowProject::default(); // two projectors, side by side
        let [a, b] = [project.outputs[0].id, project.outputs[1].id];
        let monitors = [
            screen(0, 0, true),
            screen(1, 1920, false),
            screen(2, 3840, false),
        ];
        let before = plan_screens(&monitors, &project, &None, &HashMap::new());
        // Swap the two projectors' places on the stage, as the iPad's projector
        // order or a drag in Producer does.
        let (x0, x1) = (project.outputs[0].stage_x, project.outputs[1].stage_x);
        project.outputs[0].stage_x = x1;
        project.outputs[1].stage_x = x0;
        let after = plan_screens(&monitors, &project, &None, &HashMap::new());
        // Same screens, so each screen now shows the other part of the canvas.
        assert_eq!(after[&a].display, before[&a].display);
        assert_eq!(after[&b].display, before[&b].display);
    }

    #[test]
    fn chosen_screens_win_and_missing_ones_fall_back() {
        let mut project = ShowProject::default();
        let [a, b] = [project.outputs[0].id, project.outputs[1].id];
        let monitors = [
            screen(0, 0, true),
            screen(1, 1920, false),
            screen(2, 3840, false),
        ];
        // Producer put projector 1 on Display 3; projector 2 gets the free one.
        project.outputs[0].display_index = Some(2);
        let plan = plan_screens(&monitors, &project, &None, &HashMap::new());
        assert_eq!(plan[&a].display, Some(2));
        assert_eq!(plan[&b].display, Some(1));

        // A choice on this PC wins; a screen that isn't there is replaced.
        let chosen = HashMap::from([(a, Some(1)), (b, Some(7))]);
        let plan = plan_screens(&monitors, &project, &None, &chosen);
        assert_eq!(plan[&a].display, Some(1));
        assert!(plan[&a].source == ScreenSource::ThisPc);
        assert_eq!(plan[&b].display, Some(2));
        assert_eq!(plan[&b].missing, Some(7));

        // "Window" chosen on this PC stays a window.
        let chosen = HashMap::from([(a, None)]);
        let plan = plan_screens(&monitors, &project, &None, &chosen);
        assert_eq!(plan[&a].display, None);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn asset(kind: AssetKind, width: u32, height: u32) -> Asset {
        Asset {
            id: Uuid::new_v4(),
            path: "clip.mov".into(),
            name: "clip.mov".into(),
            kind,
            checksum_sha256: String::new(),
            width: Some(width),
            height: Some(height),
            duration_seconds: Some(10.0),
            size_bytes: None,
        }
    }

    fn layer(asset: &Asset, output: &ProjectorOutput, width: f32, height: f32) -> Layer {
        Layer {
            id: Uuid::new_v4(),
            asset_id: asset.id,
            name: "clip".into(),
            x: output.stage_x,
            y: output.stage_y,
            width,
            height,
            opacity: 1.0,
            output_ids: vec![output.id],
            timeline_start: 0.0,
            timeline_duration: 15.0,
            source_offset: 0.0,
            looping: false,
            volume: 1.0,
            audio: true,
        }
    }

    #[test]
    fn media_larger_than_its_place_on_screen_is_decoded_at_display_size() {
        let mut project = ShowProject::default();
        project.lock_output_sizes();
        let output = &project.outputs[0];
        let uhd = asset(AssetKind::Video, 3840, 2160);
        let layer = layer(&uhd, output, 1920.0, 1080.0);
        let spec = LayerSpec::new(&layer, &uhd, &[output], u32::MAX);
        assert_eq!(spec.crop, None);
        assert_eq!(spec.shown, Some([1920, 1080]));
        assert_eq!(
            decode_size((3840, 2160), spec.shown, spec.max_side),
            (1920, 1080)
        );
        // The quality cap still applies on top.
        assert_eq!(decode_size((3840, 2160), spec.shown, 1280), (1280, 720));
        // Never enlarged: small media keeps its own size.
        assert_eq!(decode_size((1280, 720), spec.shown, u32::MAX), (1280, 720));
    }

    #[test]
    fn display_size_counts_projector_pixels_not_stage_units() {
        // The default show maps 1020 stage units onto 1920 projector pixels.
        let project = ShowProject::default();
        let output = &project.outputs[0];
        let uhd = asset(AssetKind::Image, 3840, 2160);
        let layer = layer(&uhd, output, 1020.0, 1080.0);
        let spec = LayerSpec::new(&layer, &uhd, &[output], u32::MAX);
        assert_eq!(spec.shown, Some([1920, 1080]));
    }

    #[test]
    fn a_non_looping_clip_is_never_started_past_its_end() {
        let project = ShowProject::default();
        let output = &project.outputs[0];
        let clip = asset(AssetKind::Video, 1920, 1080);
        let mut layer = layer(&clip, output, 1920.0, 1080.0);
        let spec = LayerSpec::new(&layer, &clip, &[output], u32::MAX);
        assert_eq!(spec.media_time(12.0), 10.0);
        assert_eq!(spec.media_clock(12.0), 12.0);
        layer.looping = true;
        let spec = LayerSpec::new(&layer, &clip, &[output], u32::MAX);
        assert!((spec.media_time(12.0) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn frames_are_shown_when_the_show_reaches_them() {
        let frame = 1.0 / VIDEO_FPS;
        assert!(matches!(pace(5.0, 4.99, false), Pace::Wait(w) if (w - 0.01).abs() < 1e-9));
        assert_eq!(pace(5.0, 5.0 + frame / 2.0, false), Pace::Show);
        // The next frame is due already: skip this one to catch up...
        assert_eq!(pace(5.0, 5.0 + frame * 1.5, false), Pace::Drop);
        // ...unless the picture has not moved for a while.
        assert_eq!(pace(5.0, 5.0 + frame * 1.5, true), Pace::Show);
        // Far behind: start again further on.
        assert_eq!(pace(5.0, 5.0 + RESYNC_SECONDS + 0.1, true), Pace::Resync);
    }

    #[test]
    fn rotated_videos_report_their_upright_size() {
        let matrix = serde_json::json!({
            "width": 1920, "height": 1080,
            "side_data_list": [{ "rotation": -90 }]
        });
        assert_eq!(upright_size(&matrix), Some((1080, 1920)));
        let tag = serde_json::json!({ "width": 1920, "height": 1080, "tags": { "rotate": "270" } });
        assert_eq!(upright_size(&tag), Some((1080, 1920)));
        let flipped =
            serde_json::json!({ "width": 1920, "height": 1080, "tags": { "rotate": "180" } });
        assert_eq!(upright_size(&flipped), Some((1920, 1080)));
    }

    #[test]
    fn a_crop_measured_on_the_stored_picture_lands_inside_the_upright_one() {
        // Measured on a 1920×1080 sideways file, delivered as 1080×1920.
        let crop = crop_in_frame([700, 0, 1200, 1080], (1920, 1080), (1080, 1920)).unwrap();
        let [x, y, w, h] = crop;
        assert!(x + w <= 1080 && y + h <= 1920, "{crop:?}");
        assert_eq!((x % 2, w % 2), (0, 0));
        assert!(x <= 394 && x + w >= 1069);
        // Same sizes: the crop is kept as it is.
        assert_eq!(
            crop_in_frame([100, 0, 1000, 1080], (1920, 1080), (1920, 1080)),
            Some([100, 0, 1000, 1080])
        );
    }

    #[test]
    fn late_or_starved_sound_catches_up_with_the_clock() {
        let (tx, rx) = mpsc::sync_channel(4);
        let late = Arc::new(AtomicU64::new(0));
        let mut source = PcmSource {
            rx,
            chunk: Vec::new(),
            position: 0,
            owed: 0,
            late: late.clone(),
        };
        // Nothing decoded yet: one silent stereo frame, owed back later.
        assert_eq!((source.next(), source.next()), (Some(0), Some(0)));
        tx.send(vec![1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(source.next(), Some(3));
        assert_eq!(source.next(), Some(4));
        // A late start skips that much sound.
        late.store(2, Ordering::SeqCst);
        source.next();
        source.next();
        tx.send(vec![7, 8, 9, 10]).unwrap();
        assert_eq!(source.next(), Some(9));
    }

    #[test]
    fn feathers_follow_the_warp() {
        let mut output = ShowProject::default().outputs.remove(0);
        output.blend.left = 255.0;
        let share = output.blend.left / output.stage_width;
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.0, 1080.0));
        // Unwarped: the inner edge sits where it always did.
        let mesh = feather_mesh(&OutputMapping::new(rect, &output), &output.blend, 0, share);
        let inner = mesh.vertices.last().unwrap().pos;
        assert!((inner.x - share * 1920.0).abs() < 0.01);
        assert!((inner.y - 1080.0).abs() < 0.01);
        // Keystoned: the inner edge is where that content is drawn.
        output.warp_mode = WarpMode::Perspective;
        output.warp_corners = [[0.0, 0.0], [0.92, 0.05], [1.0, 1.0], [0.05, 0.95]];
        let map = OutputMapping::new(rect, &output);
        let mesh = feather_mesh(&map, &output.blend, 0, share);
        for (k, vertex) in mesh.vertices.iter().rev().take(17).enumerate() {
            let y = output.stage_y + output.stage_height * (16 - k) as f32 / 16.0;
            let expected = map.point(output.stage_x + output.blend.left, y);
            assert!(vertex.pos.distance(expected) < 0.01);
        }
    }

    #[test]
    fn a_cue_during_playback_keeps_the_scene_on_air_until_its_start() {
        let mut project = ShowProject::default();
        project.scenes.push(Scene::new("Scene 2"));
        let (first, second) = (project.scenes[0].id, project.scenes[1].id);
        let media = MediaPool::default();
        let mut runtime = Runtime {
            project: Some(project),
            ..Default::default()
        };
        runtime.play(&media);
        assert_eq!(runtime.state.scene_id, Some(first));
        runtime.schedule(&media, second, 0.0, unix_time_ms() + 60_000);
        assert_eq!(runtime.state.scene_id, Some(first));
        assert_eq!(runtime.state.transport, Transport::Playing);
        assert!(media.cued.lock().unwrap().is_some());
        runtime.scheduled.as_mut().unwrap().unix_ms = unix_time_ms().saturating_sub(5);
        runtime.handle_scheduled_start(&media);
        assert_eq!(runtime.state.scene_id, Some(second));
        assert_eq!(runtime.state.transport, Transport::Playing);
        assert!(runtime.clock.position() >= 0.004);
        assert!(media.cued.lock().unwrap().is_none());
    }

    /// A two-PC show where Producer named the master's projector
    /// 127.0.0.1: the sub must find itself by its own address, not take
    /// the loopback projector (it drew the master's half instead of its own).
    #[test]
    fn a_sub_never_takes_the_loopback_projector() {
        let mut project = ShowProject::default();
        project.outputs[0].player = "127.0.0.1:4777".into();
        project.outputs[1].player = "10.10.10.2:4777".into();
        let mut runtime = Runtime {
            project: Some(project),
            port: DEFAULT_PLAYER_PORT,
            local_ips: vec!["192.168.0.145".into(), "10.10.10.2".into()],
            ..Default::default()
        };
        runtime.settings.role = Some(PlayerRole::Sub);
        assert_eq!(runtime.own_address().as_deref(), Some("10.10.10.2:4777"));
        assert!(!runtime.is_me("127.0.0.1:4777"));
        runtime.settings.role = Some(PlayerRole::Master);
        assert!(runtime.is_me("127.0.0.1:4777"), "on the master it still means this PC");
    }

    /// The master re-cues a follower and then releases its loop: the release
    /// belongs to the cued position, not to where the follower still is.
    #[test]
    fn an_exit_loop_during_a_pending_cue_applies_at_the_cut() {
        let mut project = ShowProject::default();
        let region = LoopRegion {
            id: Uuid::new_v4(),
            name: "Hold".into(),
            start: 10.0,
            end: 20.0,
            count: 0,
            hotkey: String::new(),
            exit_hotkey: String::new(),
            button: Default::default(),
        };
        project.scenes[0].loops.push(region.clone());
        let scene = project.scenes[0].id;
        let media = MediaPool::default();
        let mut runtime = Runtime {
            project: Some(project),
            ..Default::default()
        };
        runtime.play(&media);
        runtime.schedule(&media, scene, 12.0, unix_time_ms() + 60_000);
        runtime.release_loops();
        assert!(runtime.armed(&region), "nothing is released before the cut");
        runtime.scheduled.as_mut().unwrap().unix_ms = unix_time_ms().saturating_sub(5);
        runtime.handle_scheduled_start(&media);
        assert!(!runtime.armed(&region), "the loop is released once the cue lands");
    }

    #[test]
    fn scene_loops_keep_a_sound_only_intro() {
        let mut project = ShowProject::default();
        let output = project.outputs[0].clone();
        let music = asset(AssetKind::Audio, 0, 0);
        let video = asset(AssetKind::Video, 1920, 1080);
        let bed = layer(&music, &output, 1.0, 1.0);
        let mut picture = layer(&video, &output, 1920.0, 1080.0);
        picture.timeline_start = 3.0;
        project.assets = vec![music, video];
        project.scenes[0].layers = vec![bed, picture];
        let runtime = Runtime {
            project: Some(project),
            ..Default::default()
        };
        let scene = &runtime.project.as_ref().unwrap().scenes[0];
        assert_eq!(runtime.visual_loop_start(scene), 0.0);
    }
}
