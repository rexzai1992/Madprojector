// Release builds on Windows open no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod displays;
mod relay;
mod setup;

use displays::DisplayMonitor;
use eframe::egui;
use mapforge_core::{
    net::unix_time_ms, player_host, scene_hotkey, tool_command, Asset, AssetKind, AssetStatus,
    Command, EndAction, Envelope, Layer, LoopRegion, PlayerRole, PlayerState, ProjectorOutput,
    Scene, ShowProject, TestPattern, Transport, CONTROLLER_PORT, DEFAULT_PLAYER_PORT,
    PROTOCOL_VERSION,
};
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
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const CONTROLLER_HTML: &str = include_str!("controller.html");
const VIDEO_FPS: f64 = 30.0;
const MAX_VIDEO_DECODE_WIDTH: u32 = 1920;
const MAX_IMAGE_SIZE: u32 = 4096;
const AUDIO_RATE: u32 = 48_000;
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
    /// Sub: how the link to the master is doing.
    link_note: String,
    scheduled: Option<ScheduledStart>,
}

#[derive(Clone, Copy)]
struct ScheduledStart {
    unix_ms: u64,
    scene_id: Uuid,
    seconds: f64,
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
        port == Some(self.port)
            && (is_loopback(address) || self.local_ips.contains(&player_host(address)))
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
        let position = self.clock.position();
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
        Some((self.upcoming_scene()?, 0.0, scene.duration()))
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
            EndAction::Loop => position % end,
            EndAction::Hold => end - 0.001,
            EndAction::Stop | EndAction::Next => position,
        }
    }

    /// The scene to pre-roll as the current one nears its end.
    fn upcoming_scene(&self) -> Option<Uuid> {
        let scene = self.scene()?;
        match scene.end_action {
            EndAction::Loop => Some(scene.id),
            EndAction::Next => {
                let scenes = &self.project.as_ref()?.scenes;
                let index = scenes.iter().position(|s| s.id == scene.id)?;
                scenes.get(index + 1).map(|s| s.id)
            }
            EndAction::Hold | EndAction::Stop => None,
        }
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

    fn play(&mut self, media: &MediaPool) {
        self.scheduled = None;
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
        self.scheduled = None;
        if let (Some(project), Some(scene_id)) = (&self.project, self.state.scene_id) {
            media.sync(project, scene_id, true, 0.0);
        }
        self.clock = Clock::default();
        self.reset_loops(0.0);
        self.state.transport = Transport::Stopped;
        self.state.message = "Stopped".into();
    }

    fn schedule(&mut self, media: &MediaPool, scene_id: Uuid, seconds: f64, unix_ms: u64) {
        if self.prepare(media, scene_id, seconds) {
            self.scheduled = Some(ScheduledStart {
                unix_ms,
                scene_id,
                seconds,
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
        if self.state.scene_id != Some(start.scene_id) {
            if !self.prepare(media, start.scene_id, position) {
                return;
            }
        } else if let Some(project) = &self.project {
            media.sync(project, start.scene_id, late_ms > 40.0, position);
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
        if self.state.transport != Transport::Playing {
            return;
        }
        let Some(scene) = self.scene() else {
            return;
        };
        let end = scene.duration();
        if end <= 0.0 || self.clock.position() < end {
            return;
        }
        let (scene_id, action) = (scene.id, scene.end_action);
        match action {
            EndAction::Loop => {
                if let Some(project) = &self.project {
                    media.sync(project, scene_id, true, 0.0);
                }
                // Keep the few milliseconds past the end so timing never drifts.
                let overflow = (self.clock.position() - end).clamp(0.0, 0.05);
                self.clock = Clock {
                    base: overflow,
                    since: Some(Instant::now()),
                };
                self.reset_loops(overflow);
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

#[derive(Clone)]
struct DecodedFrame {
    width: usize,
    height: usize,
    rgba: Arc<Vec<u8>>,
    sequence: u64,
}

/// Everything a layer's decoders were started with; a change restarts them.
#[derive(Clone, PartialEq)]
struct LayerSpec {
    asset: Asset,
    looping: bool,
    source_offset: f64,
    timeline_start: f64,
    audio: bool,
}

impl LayerSpec {
    fn new(layer: &Layer, asset: &Asset) -> Self {
        let mut asset = asset.clone();
        asset.path = resolve_media(&asset).to_string_lossy().to_string();
        Self {
            asset,
            looping: layer.looping,
            source_offset: layer.source_offset,
            timeline_start: layer.timeline_start,
            audio: layer.audio,
        }
    }

    /// Media position for show time `seconds`, wrapped for looping clips.
    fn media_time(&self, seconds: f64) -> f64 {
        let t = self.source_offset + (seconds - self.timeline_start).max(0.0);
        match self.asset.duration_seconds {
            Some(d) if self.looping && d > 0.0 => t % d,
            _ => t,
        }
    }
}

#[derive(Default)]
struct Decoder {
    latest: Mutex<Option<DecodedFrame>>,
    error: Mutex<Option<String>>,
    cancelled: AtomicBool,
    /// True while the show clock is inside this clip and playing.
    active: AtomicBool,
}

impl Decoder {
    fn publish(&self, width: usize, height: usize, rgba: Vec<u8>, sequence: u64) {
        *self.latest.lock().unwrap() = Some(DecodedFrame {
            width,
            height,
            rgba: Arc::new(rgba),
            sequence,
        });
    }

    fn fail(&self, message: String) {
        *self.error.lock().unwrap() = Some(message);
    }

    fn decode_image(&self, asset: &Asset) {
        match image::open(&asset.path) {
            Ok(image) => {
                let image = if image.width() > MAX_IMAGE_SIZE || image.height() > MAX_IMAGE_SIZE {
                    image.thumbnail(MAX_IMAGE_SIZE, MAX_IMAGE_SIZE)
                } else {
                    image
                };
                let rgba = image.to_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                self.publish(w, h, rgba.into_raw(), 1);
            }
            Err(error) => self.fail(format!("{}: image decode failed: {error}", asset.name)),
        }
    }

    /// Streams RGBA frames from FFmpeg at a fixed rate. While inactive it
    /// stops reading the pipe, which blocks FFmpeg, so playback resumes on
    /// the same frame.
    fn decode_video(&self, spec: &LayerSpec, start: f64) {
        let asset = &spec.asset;
        let source_width = asset.width.unwrap_or(1280).max(2);
        let source_height = asset.height.unwrap_or(720).max(2);
        let width = (source_width.min(MAX_VIDEO_DECODE_WIDTH) / 2 * 2).max(2) as usize;
        let height = ((width as f64 * source_height as f64 / source_width as f64 / 2.0).round()
            as usize
            * 2)
        .max(2);
        let mut command = ffmpeg_input(&asset.path, spec.looping, start);
        command
            .args([
                "-an",
                "-vf",
                &format!("scale={width}:{height}"),
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
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.fail(format!("Could not start FFmpeg: {error}"));
                return;
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            return;
        };
        let frame_time = Duration::from_secs_f64(1.0 / VIDEO_FPS);
        let mut sequence = 0_u64;
        let mut next_frame = Instant::now();
        while !self.cancelled.load(Ordering::SeqCst) {
            // The first frame is decoded straight away so a clip appears
            // instantly when its time comes.
            if sequence > 0 && !self.active.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
                next_frame = Instant::now();
                continue;
            }
            let mut rgba = vec![0_u8; width * height * 4];
            if stdout.read_exact(&mut rgba).is_err() {
                if sequence == 0 {
                    self.fail(format!("{}: FFmpeg produced no frames", asset.name));
                }
                // A non-looping clip holds its last frame.
                break;
            }
            sequence += 1;
            self.publish(width, height, rgba, sequence);
            next_frame += frame_time;
            let now = Instant::now();
            if next_frame > now {
                thread::sleep(next_frame - now);
            } else if now - next_frame > Duration::from_millis(250) {
                next_frame = now;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
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

/// Sound for one layer: FFmpeg decodes to 48 kHz stereo PCM, which a rodio
/// sink plays. Pausing the sink stops pulling samples, so FFmpeg waits.
struct AudioTrack {
    sink: rodio::Sink,
    cancelled: Arc<AtomicBool>,
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
        let mut command = ffmpeg_input(&spec.asset.path, spec.looping, start);
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
        sink.append(PcmSource {
            rx,
            chunk: Vec::new(),
            position: 0,
        });
        Some(Self { sink, cancelled })
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

struct PcmSource {
    rx: Receiver<Vec<i16>>,
    chunk: Vec<i16>,
    position: usize,
}

impl Iterator for PcmSource {
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        if self.position >= self.chunk.len() {
            match self.rx.try_recv() {
                Ok(chunk) => self.chunk = chunk,
                // Decoder is behind: play one silent stereo frame.
                Err(TryRecvError::Empty) => self.chunk = vec![0, 0],
                Err(TryRecvError::Disconnected) => return None,
            }
            self.position = 0;
        }
        let sample = self.chunk[self.position];
        self.position += 1;
        Some(sample)
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

fn layer_specs(project: &ShowProject, scene_id: Uuid) -> HashMap<Uuid, LayerSpec> {
    let mut specs = HashMap::new();
    if let Some(scene) = project.scenes.iter().find(|s| s.id == scene_id) {
        for layer in &scene.layers {
            if let Some(asset) = project.assets.iter().find(|a| a.id == layer.asset_id) {
                specs.insert(layer.id, LayerSpec::new(layer, asset));
            }
        }
    }
    specs
}

/// Decoders for every layer of the prepared scene, keyed by layer id.
#[derive(Default)]
struct MediaPool {
    slots: Mutex<HashMap<Uuid, Slot>>,
    /// Media pre-rolled for the scene that plays next (or this one again).
    standby: Mutex<Option<(Uuid, f64, HashMap<Uuid, Slot>)>>,
    audio_output: Option<rodio::OutputStreamHandle>,
}

impl MediaPool {
    /// Starts decoders for the scene's layers at show time `position` and
    /// stops the rest. Unchanged layers keep running unless `restart`.
    fn sync(&self, project: &ShowProject, scene_id: Uuid, restart: bool, position: f64) {
        let wanted = layer_specs(project, scene_id);
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
        let slots = layer_specs(project, scene_id)
            .into_iter()
            .map(|(id, spec)| (id, self.start_slot(spec, position)))
            .collect();
        *standby = Some((scene_id, position, slots));
    }

    fn start_slot(&self, spec: LayerSpec, position: f64) -> Slot {
        let start = spec.media_time(position);
        let video = matches!(spec.asset.kind, AssetKind::Image | AssetKind::Video).then(|| {
            let decoder = Arc::new(Decoder::default());
            let worker = decoder.clone();
            let worker_spec = spec.clone();
            thread::spawn(move || match worker_spec.asset.kind {
                AssetKind::Image => worker.decode_image(&worker_spec.asset),
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
    }

    /// Runs or pauses each layer's decoders for the current show time.
    fn update(&self, scene: Option<&Scene>, position: f64, playing: bool, master: f32) {
        let slots = self.slots.lock().unwrap();
        for (id, slot) in slots.iter() {
            let layer = scene.and_then(|s| s.layers.iter().find(|l| l.id == *id));
            let active = playing && layer.is_some_and(|l| l.active_at(position));
            if let Some(video) = &slot.video {
                video.active.store(active, Ordering::SeqCst);
            }
            if let Some(audio) = &slot.audio {
                audio
                    .sink
                    .set_volume(master * layer.map_or(0.0, |l| l.volume));
                if active {
                    audio.sink.play();
                } else {
                    audio.sink.pause();
                }
            }
        }
    }

    fn frames(&self) -> Vec<(Uuid, Option<DecodedFrame>)> {
        self.slots
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, slot)| {
                let video = slot.video.as_ref()?;
                Some((*id, video.latest.lock().unwrap().clone()))
            })
            .collect()
    }

    fn summary(&self) -> (usize, usize, Vec<String>) {
        let slots = self.slots.lock().unwrap();
        let videos = slots.values().filter(|s| s.video.is_some()).count();
        let sounds = slots.values().filter(|s| s.audio.is_some()).count();
        let errors = slots
            .values()
            .filter_map(|s| s.video.as_ref()?.error.lock().unwrap().clone())
            .collect();
        (videos, sounds, errors)
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
    match envelope.command {
        Command::LoadProject {
            project,
            outputs,
            player,
        } => match project.validate() {
            Ok(()) => {
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
            runtime.scheduled = None;
            runtime.clock.hold();
            runtime.state.transport = Transport::Paused;
            runtime.state.message = "Paused".into();
        }
        Command::Stop => runtime.stop(media),
        Command::Seek { seconds } => {
            runtime.scheduled = None;
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
    let state = shared.lock().unwrap().snapshot();
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
    serde_json::json!({ "settings": settings, "scenes": scenes }).to_string()
}

fn http_server(shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>, relay: Arc<Relay>) {
    let port = port_setting("MAPFORGE_HTTP_PORT", CONTROLLER_PORT);
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

struct HttpRequest {
    method: String,
    path: String,
    content_length: u64,
    /// Body bytes that arrived together with the headers.
    body_start: Vec<u8>,
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

/// Sends one of the show's media files to a sub that is missing it.
fn send_media(mut stream: TcpStream, path: &str, shared: &Mutex<Runtime>) {
    let asset = Uuid::parse_str(path.trim_start_matches("/api/media/"))
        .ok()
        .and_then(|id| {
            let runtime = shared.lock().unwrap();
            let project = runtime.project.as_ref()?;
            project.assets.iter().find(|a| a.id == id).cloned()
        });
    let Some(file) = asset.and_then(|a| fs::File::open(resolve_media(&a)).ok()) else {
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
    let target = cached_path(&asset);
    let partial = target.with_extension("part");
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

struct PlayerApp {
    shared: Arc<Mutex<Runtime>>,
    media: Arc<MediaPool>,
    textures: HashMap<Uuid, (egui::TextureHandle, u64)>,
    show_outputs: bool,
    monitors: Vec<DisplayMonitor>,
    /// The master-or-sub form, open on first start or when changing it.
    setup_draft: Option<PlayerSettings>,
    /// This PC's LAN address, shown to the operator.
    ip: String,
}

impl PlayerApp {
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
            if let Err(e) = setup::save_settings(&settings) {
                self.shared.lock().unwrap().state.message = format!("Could not save setup: {e}");
            }
            let mut runtime = self.shared.lock().unwrap();
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
            .retain(|id, _| frames.iter().any(|(frame_id, _)| frame_id == id));
        for (id, frame) in frames {
            let Some(frame) = frame else {
                continue;
            };
            if self
                .textures
                .get(&id)
                .is_some_and(|(_, sequence)| *sequence == frame.sequence)
            {
                continue;
            }
            let image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
            match self.textures.get_mut(&id) {
                Some((texture, sequence)) => {
                    texture.set(image, egui::TextureOptions::LINEAR);
                    *sequence = frame.sequence;
                }
                None => {
                    let texture = ctx.load_texture(
                        format!("media-{id}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(id, (texture, frame.sequence));
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
        }
    }

    fn point(&self, x: f32, y: f32) -> egui::Pos2 {
        self.normalized(
            (x - self.output_x) / self.output_width,
            (y - self.output_y) / self.output_height,
        )
    }

    fn normalized(&self, u: f32, v: f32) -> egui::Pos2 {
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
    textures: &HashMap<Uuid, (egui::TextureHandle, u64)>,
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
    paint_correction(&painter, rect, output);
    paint_mask(&painter, rect, output);
}

fn paint_warped_color(painter: &egui::Painter, map: &OutputMapping, color: egui::Color32) {
    let mut mesh = egui::Mesh::default();
    for point in [
        map.normalized(0.0, 0.0),
        map.normalized(1.0, 0.0),
        map.normalized(1.0, 1.0),
        map.normalized(0.0, 1.0),
    ] {
        mesh.colored_vertex(point, color);
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(mesh);
}

fn paint_layers(
    painter: &egui::Painter,
    map: &OutputMapping,
    project: &ShowProject,
    state: &PlayerState,
    output: &ProjectorOutput,
    textures: &HashMap<Uuid, (egui::TextureHandle, u64)>,
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
        if let Some((texture, _)) = textures.get(&layer.id) {
            let uv = egui::Rect::from_min_max(
                egui::pos2(
                    (left - layer.x) / layer.width,
                    (top - layer.y) / layer.height,
                ),
                egui::pos2(
                    (right - layer.x) / layer.width,
                    (bottom - layer.y) / layer.height,
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
    for i in 0..=columns {
        let x = stage.width * i as f32 / columns as f32;
        painter.line_segment([map.point(x, 0.0), map.point(x, stage.height)], stroke);
    }
    for i in 0..=rows {
        let y = stage.height * i as f32 / rows as f32;
        painter.line_segment([map.point(0.0, y), map.point(stage.width, y)], stroke);
    }
    let circle: Vec<_> = (0..=96)
        .map(|step| {
            let angle = std::f32::consts::TAU * step as f32 / 96.0;
            map.point(
                stage.width / 2.0 + angle.cos() * stage.height * 0.4,
                stage.height / 2.0 + angle.sin() * stage.height * 0.4,
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        circle,
        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 200, 0)),
    ));
    painter.text(
        map.rect.left_top() + egui::vec2(12.0, 12.0),
        egui::Align2::LEFT_TOP,
        &output.name,
        egui::FontId::proportional(20.0),
        egui::Color32::from_rgb(255, 200, 0),
    );
}

/// Black-level lift, edge-blend feathers and brightness, in that order.
fn paint_correction(painter: &egui::Painter, rect: egui::Rect, output: &ProjectorOutput) {
    let blend = &output.blend;
    let left = blend.left / output.stage_width * rect.width();
    let right = blend.right / output.stage_width * rect.width();
    let top = blend.top / output.stage_height * rect.height();
    let bottom = blend.bottom / output.stage_height * rect.height();

    if output.color.black_lift > 0.0 {
        let inner = egui::Rect::from_min_max(
            rect.min + egui::vec2(left, top),
            rect.max - egui::vec2(right, bottom),
        );
        if inner.is_positive() {
            let alpha = (output.color.black_lift * 255.0).round() as u8;
            painter.rect_filled(inner, 0.0, egui::Color32::from_white_alpha(alpha));
        }
    }

    // Each feather is one gradient mesh; separate strips leave visible seams.
    const STEPS: usize = 96;
    for (edge, width) in [(0, left), (1, right), (2, top), (3, bottom)] {
        if width <= 0.5 {
            continue;
        }
        let mut mesh = egui::Mesh::default();
        for step in 0..=STEPS {
            let t = step as f32 / STEPS as f32;
            let alpha = ((1.0 - blend.pixel(t)) * 255.0).round() as u8;
            let color = egui::Color32::from_black_alpha(alpha);
            let (a, b) = match edge {
                0 => {
                    let x = rect.left() + t * width;
                    (egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom()))
                }
                1 => {
                    let x = rect.right() - t * width;
                    (egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom()))
                }
                2 => {
                    let y = rect.top() + t * width;
                    (egui::pos2(rect.left(), y), egui::pos2(rect.right(), y))
                }
                _ => {
                    let y = rect.bottom() - t * width;
                    (egui::pos2(rect.left(), y), egui::pos2(rect.right(), y))
                }
            };
            mesh.colored_vertex(a, color);
            mesh.colored_vertex(b, color);
            if step > 0 {
                let i = (step as u32) * 2;
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i, i + 1);
            }
        }
        painter.add(mesh);
    }
    if output.color.brightness < 1.0 {
        let alpha = ((1.0 - output.color.brightness) * 255.0).round() as u8;
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(alpha));
    }
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.update_textures(ctx);
        ctx.request_repaint_after(Duration::from_millis(16));

        let (project, state, assigned) = {
            let mut runtime = self.shared.lock().unwrap();
            (
                runtime.project.clone(),
                runtime.snapshot(),
                runtime.outputs.clone(),
            )
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("MapForge Player");
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
            let (videos, sounds, errors) = self.media.summary();
            for error in errors {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
            ui.label(format!(
                "{videos} picture layer(s) · {sounds} sound layer(s)"
            ));
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
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "Physical displays detected: {}",
                    self.monitors.len()
                ));
                if ui.small_button("Refresh displays").clicked() {
                    self.monitors = displays::enumerate();
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
                    if monitor.primary { " (primary)" } else { "" }
                ));
            }
            ui.label(format!("Media cache: {}", media_cache_dir().display()));
            ui.checkbox(&mut self.show_outputs, "Open projector outputs");
            ui.label("Closing Producer or the controller does not stop this Player.");
        });

        if !self.show_outputs {
            return;
        }
        let Some(project) = project else {
            return;
        };
        for (index, output) in project.outputs.iter().enumerate() {
            if assigned
                .as_ref()
                .is_some_and(|ids| !ids.contains(&output.id))
            {
                continue;
            }
            let monitor = output
                .display_index
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
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of(("output", output.id, placement)),
                viewport,
                move |ctx, _| {
                    if fullscreen && ctx.input(|i| i.viewport().fullscreen) != Some(true) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    }
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                        .show(ctx, |ui| {
                            paint_output(ui, index, project, state, output, textures)
                        });
                },
            );
        }
    }
}

fn main() -> eframe::Result<()> {
    let shared = Arc::new(Mutex::new(Runtime {
        settings: setup::load_settings(),
        port: port_setting("MAPFORGE_PORT", DEFAULT_PLAYER_PORT),
        local_ips: setup::local_ip().into_iter().collect(),
        ..Runtime::default()
    }));
    let media = Arc::new(MediaPool {
        slots: Mutex::new(HashMap::new()),
        standby: Mutex::new(None),
        audio_output: open_audio_output(),
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
        eframe::NativeOptions::default(),
        Box::new(|_| {
            Ok(Box::new(PlayerApp {
                shared,
                media,
                textures: HashMap::new(),
                show_outputs: true,
                monitors: displays::enumerate(),
                setup_draft,
                ip: setup::local_ip().unwrap_or_else(|| "unknown".into()),
            }))
        }),
    )
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
    fn scheduled_start_compensates_for_lateness() {
        let project = ShowProject::default();
        let scene_id = project.scenes[0].id;
        let media = MediaPool {
            slots: Mutex::new(HashMap::new()),
            standby: Mutex::new(None),
            audio_output: None,
        };
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
