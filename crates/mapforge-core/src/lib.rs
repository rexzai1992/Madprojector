use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use uuid::Uuid;

pub mod net;

pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowProject {
    pub schema_version: u32,
    pub id: Uuid,
    pub name: String,
    pub stage: VirtualStage,
    pub assets: Vec<Asset>,
    pub scenes: Vec<Scene>,
    pub outputs: Vec<ProjectorOutput>,
    #[serde(default)]
    pub test_pattern: TestPattern,
    #[serde(default)]
    pub controller: ControllerSettings,
    #[serde(default)]
    pub show: ShowSettings,
}

/// How the show runs on its own once the Players have it, without Producer.
/// Which PC is the master is chosen on each Player when it first starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShowSettings {
    /// Play sound on every Player PC instead of on the master only.
    pub audio_everywhere: bool,
}

/// A Player's job in a multi-PC show, chosen on the Player itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerRole {
    /// Takes the iPad's commands, passes them to the subs and can start the
    /// show by itself.
    Master,
    /// Gets the show and media from the master and follows it.
    Sub,
}

/// Look and layout of the phone/tablet web controller served by the Player.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ControllerSettings {
    pub title: String,
    pub note: String,
    pub accent: [u8; 3],
    pub columns: u8,
    pub show_transport: bool,
    pub show_scenes: bool,
    pub show_blackout: bool,
    pub show_volume: bool,
}

impl Default for ControllerSettings {
    fn default() -> Self {
        Self {
            title: "MapForge Live Control".into(),
            note: String::new(),
            accent: [36, 107, 254],
            columns: 2,
            show_transport: true,
            show_scenes: true,
            show_blackout: true,
            show_volume: true,
        }
    }
}

/// How a scene appears as a button on the web controller.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SceneButton {
    /// Button text; the scene name is used when empty.
    pub label: String,
    pub color: Option<[u8; 3]>,
    pub hidden: bool,
}

/// Full-stage calibration image shown instead of the scene content.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestPattern {
    #[default]
    Off,
    White,
    Gray,
    Grid,
    /// Big projector number, name, Player address and resolution.
    Identify,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VirtualStage {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    pub kind: AssetKind,
    pub checksum_sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Whether a Player PC has a usable local copy of an asset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetStatus {
    pub asset_id: Uuid,
    pub ready: bool,
    /// The file was received over the LAN into the Player's media cache.
    pub cached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Image,
    Video,
    Audio,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scene {
    pub id: Uuid,
    pub name: String,
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub button: SceneButton,
    /// Key that takes this scene live, e.g. "F1" or "S". Empty means the
    /// automatic F-key for its position.
    #[serde(default)]
    pub hotkey: String,
    #[serde(default)]
    pub end_action: EndAction,
    /// Named start points inside the scene, each with its own trigger.
    #[serde(default)]
    pub cues: Vec<Cue>,
    /// Sections of the timeline that repeat until released.
    #[serde(default)]
    pub loops: Vec<LoopRegion>,
}

impl Scene {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            layers: vec![],
            button: SceneButton::default(),
            hotkey: String::new(),
            end_action: EndAction::default(),
            cues: vec![],
            loops: vec![],
        }
    }

    /// Where the last clip ends.
    pub fn duration(&self) -> f64 {
        self.layers
            .iter()
            .map(Layer::timeline_end)
            .fold(0.0, f64::max)
    }
}

/// Hotkey for the scene at `index`: its own, or F1–F12 by position.
pub fn scene_hotkey(index: usize, scene: &Scene) -> String {
    if !scene.hotkey.trim().is_empty() {
        scene.hotkey.clone()
    } else if index < 12 {
        format!("F{}", index + 1)
    } else {
        String::new()
    }
}

/// What a scene does when the show clock passes its last clip.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndAction {
    #[default]
    Loop,
    Hold,
    Stop,
    Next,
}

/// A section of a scene that plays over and over until it is released (or
/// has played `count` times), e.g. an idle look held until the next cue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoopRegion {
    pub id: Uuid,
    pub name: String,
    pub start: f64,
    pub end: f64,
    /// How many times it plays before continuing; 0 means until released.
    #[serde(default)]
    pub count: u32,
    /// Jumps to the loop's start and plays.
    #[serde(default)]
    pub hotkey: String,
    /// Releases the loop so playback continues past its end.
    #[serde(default)]
    pub exit_hotkey: String,
    #[serde(default)]
    pub button: SceneButton,
}

impl LoopRegion {
    pub fn contains(&self, seconds: f64) -> bool {
        seconds >= self.start && seconds < self.end
    }
}

/// A named point in a scene's timeline that can be jumped to live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cue {
    pub id: Uuid,
    pub name: String,
    pub time: f64,
    #[serde(default)]
    pub hotkey: String,
    #[serde(default)]
    pub button: SceneButton,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Layer {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub opacity: f32,
    pub output_ids: Vec<Uuid>,
    #[serde(default)]
    pub timeline_start: f64,
    #[serde(default = "default_layer_duration")]
    pub timeline_duration: f64,
    #[serde(default)]
    pub source_offset: f64,
    #[serde(default = "default_true")]
    pub looping: bool,
    /// Audio level for music and video sound, 0–2.
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// Plays the sound of a video or audio file.
    #[serde(default = "default_true")]
    pub audio: bool,
}

fn default_volume() -> f32 {
    1.0
}

impl Layer {
    pub fn timeline_end(&self) -> f64 {
        self.timeline_start + self.timeline_duration
    }

    /// True while the show clock is inside this clip.
    pub fn active_at(&self, seconds: f64) -> bool {
        seconds >= self.timeline_start && seconds < self.timeline_end()
    }

    /// Position inside the media file at show time `seconds`.
    pub fn source_time(&self, seconds: f64) -> f64 {
        self.source_offset + (seconds - self.timeline_start).max(0.0)
    }
}

fn default_layer_duration() -> f64 {
    10.0
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectorOutput {
    pub id: Uuid,
    pub name: String,
    /// Address of the Player PC that drives this projector, e.g.
    /// "192.168.1.50:4777". "local-player" or empty means this computer.
    pub player: String,
    /// Native projector resolution in pixels.
    #[serde(default = "default_resolution")]
    pub resolution: [u32; 2],
    /// Zero-based physical display number on the assigned Player PC.
    /// `None` keeps this output in a safe, movable preview window.
    #[serde(default)]
    pub display_index: Option<u32>,
    /// Output-space four-corner correction: top-left, top-right,
    /// bottom-right and bottom-left, normalized to the projector window.
    #[serde(default = "default_warp_corners")]
    pub warp_corners: [[f32; 2]; 4],
    /// Optional hard-edged polygon mask in normalized projector coordinates.
    #[serde(default)]
    pub mask: OutputMask,
    pub stage_x: f32,
    pub stage_y: f32,
    pub stage_width: f32,
    pub stage_height: f32,
    #[serde(default)]
    pub blend: EdgeBlend,
    #[serde(default)]
    pub color: OutputColor,
}

fn default_resolution() -> [u32; 2] {
    [1920, 1080]
}

fn default_warp_corners() -> [[f32; 2]; 4] {
    [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OutputMask {
    pub enabled: bool,
    /// Clockwise or counter-clockwise polygon vertices, normalized to 0–1.
    pub points: Vec<[f32; 2]>,
}

impl Default for OutputMask {
    fn default() -> Self {
        Self {
            enabled: false,
            points: default_warp_corners().to_vec(),
        }
    }
}

pub const DEFAULT_PLAYER_PORT: u16 = 4777;
pub const CONTROLLER_PORT: u16 = 8080;

impl ProjectorOutput {
    /// The Player's protocol address with the default port filled in.
    pub fn player_address(&self) -> String {
        normalize_player_address(&self.player)
    }
}

/// Starts a helper program such as FFmpeg: the copy installed next to
/// MapForge if there is one, otherwise the one on PATH. On Windows it runs
/// without opening a console window.
pub fn tool_command(name: &str) -> std::process::Command {
    let bundled = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path.exists());
    #[allow(unused_mut)]
    let mut command = match bundled {
        Some(path) => std::process::Command::new(path),
        None => std::process::Command::new(name),
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub fn normalize_player_address(address: &str) -> String {
    let address = address.trim();
    if address.is_empty() || address == "local-player" || address == "localhost" {
        return format!("127.0.0.1:{DEFAULT_PLAYER_PORT}");
    }
    if address
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        address.to_string()
    } else {
        format!("{address}:{DEFAULT_PLAYER_PORT}")
    }
}

/// Host part of a Player address, used for its HTTP controller port.
pub fn player_host(address: &str) -> String {
    let address = normalize_player_address(address);
    address
        .rsplit_once(':')
        .map_or(address.clone(), |(host, _)| host.to_string())
}

/// Feather widths are in virtual-stage pixels. The ramp follows Paul Bourke's
/// projector blend: `luminance` is the light level at the seam midpoint (0.5
/// makes two opposing ramps sum to 100%), `power` shapes the curve, and
/// `gamma` is the projector's display gamma used to convert light to pixels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeBlend {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
    pub gamma: f32,
    #[serde(default = "default_luminance")]
    pub luminance: f32,
    #[serde(default = "default_power")]
    pub power: f32,
}

fn default_luminance() -> f32 {
    0.5
}

fn default_power() -> f32 {
    2.0
}

impl Default for EdgeBlend {
    fn default() -> Self {
        Self {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
            gamma: 2.2,
            luminance: default_luminance(),
            power: default_power(),
        }
    }
}

impl EdgeBlend {
    /// Light contribution (0–1) at `t`, where 0 is the outer edge of the
    /// feather and 1 is where it meets the unblended image.
    pub fn light(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        let a = self.luminance.clamp(0.0, 1.0);
        let p = self.power.max(0.01);
        if t < 0.5 {
            a * (2.0 * t).powf(p)
        } else {
            1.0 - (1.0 - a) * (2.0 * (1.0 - t)).powf(p)
        }
    }

    /// Pixel multiplier that produces [`EdgeBlend::light`] on a projector with
    /// this blend's display gamma.
    pub fn pixel(&self, t: f32) -> f32 {
        self.light(t).powf(1.0 / self.gamma.max(0.1))
    }
}

/// Per-projector correction applied after blending.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputColor {
    /// Overall brightness multiplier, 0–1, for matching a brighter projector.
    pub brightness: f32,
    /// Raises black outside the blend zones so it matches the doubled black
    /// level inside the overlap, 0–0.5.
    pub black_lift: f32,
}

impl Default for OutputColor {
    fn default() -> Self {
        Self {
            brightness: 1.0,
            black_lift: 0.0,
        }
    }
}

impl Default for ShowProject {
    fn default() -> Self {
        let left = ProjectorOutput {
            id: Uuid::new_v4(),
            name: "Projector 1".into(),
            player: "127.0.0.1:4777".into(),
            resolution: [1920, 1080],
            display_index: None,
            warp_corners: default_warp_corners(),
            mask: OutputMask::default(),
            stage_x: 0.0,
            stage_y: 0.0,
            stage_width: 1020.0,
            stage_height: 1080.0,
            blend: EdgeBlend {
                right: 120.0,
                ..Default::default()
            },
            color: OutputColor::default(),
        };
        let right = ProjectorOutput {
            id: Uuid::new_v4(),
            name: "Projector 2".into(),
            player: "127.0.0.1:4777".into(),
            resolution: [1920, 1080],
            display_index: None,
            warp_corners: default_warp_corners(),
            mask: OutputMask::default(),
            stage_x: 900.0,
            stage_y: 0.0,
            stage_width: 1020.0,
            stage_height: 1080.0,
            blend: EdgeBlend {
                left: 120.0,
                ..Default::default()
            },
            color: OutputColor::default(),
        };
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            id: Uuid::new_v4(),
            name: "Untitled Show".into(),
            stage: VirtualStage {
                width: 1920.0,
                height: 1080.0,
            },
            assets: vec![],
            scenes: vec![Scene::new("Scene 1")],
            outputs: vec![left, right],
            test_pattern: TestPattern::Off,
            controller: ControllerSettings::default(),
            show: ShowSettings::default(),
        }
    }
}

impl ShowProject {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != PROJECT_SCHEMA_VERSION {
            bail!("unsupported project schema {}", self.schema_version);
        }
        if !(1.0..=32768.0).contains(&self.stage.width)
            || !(1.0..=32768.0).contains(&self.stage.height)
        {
            bail!("stage dimensions are out of range");
        }
        if !(1..=4).contains(&self.controller.columns)
            || self.controller.title.len() > 200
            || self.controller.note.len() > 2000
        {
            bail!("controller settings are out of range");
        }
        if self.scenes.len() > 10_000 || self.assets.len() > 100_000 {
            bail!("project exceeds safety limits");
        }
        for scene in &self.scenes {
            if scene
                .loops
                .iter()
                .any(|l| !(l.start >= 0.0 && l.end > l.start + 0.05 && l.end.is_finite()))
            {
                bail!("loops need an end after their start");
            }
            if scene.cues.len() > 1000 {
                bail!("scene {} has too many cues", scene.name);
            }
            if scene
                .cues
                .iter()
                .any(|c| !c.time.is_finite() || c.time < 0.0)
            {
                bail!("cue times must be zero or positive");
            }
            for layer in &scene.layers {
                if !(0.0..=2.0).contains(&layer.volume)
                    || !layer.timeline_start.is_finite()
                    || layer.timeline_start < 0.0
                    || !(layer.timeline_duration > 0.0)
                {
                    bail!("layer {} has invalid timing or volume", layer.name);
                }
            }
        }
        for output in &self.outputs {
            if output.stage_width <= 0.0 || output.stage_height <= 0.0 {
                bail!("output dimensions must be positive");
            }
            if !(0.1..=10.0).contains(&output.blend.gamma)
                || !(0.0..=1.0).contains(&output.blend.luminance)
                || !(0.01..=10.0).contains(&output.blend.power)
                || !(0.0..=1.0).contains(&output.color.brightness)
                || !(0.0..=0.5).contains(&output.color.black_lift)
                || !(0.0..=output.stage_width).contains(&output.blend.left)
                || !(0.0..=output.stage_width).contains(&output.blend.right)
                || !(0.0..=output.stage_height).contains(&output.blend.top)
                || !(0.0..=output.stage_height).contains(&output.blend.bottom)
            {
                bail!("output blend settings are out of range");
            }
            if output
                .warp_corners
                .iter()
                .flatten()
                .any(|value| !value.is_finite() || !(-0.5..=1.5).contains(value))
            {
                bail!("output warp corners are out of range");
            }
            if output.mask.enabled
                && (output.mask.points.len() < 3
                    || output.mask.points.len() > 64
                    || output
                        .mask
                        .points
                        .iter()
                        .flatten()
                        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value)))
            {
                bail!("output mask needs 3–64 points inside the output");
            }
        }
        Ok(())
    }

    /// Distinct Player addresses, in projector order.
    pub fn players(&self) -> Vec<String> {
        let mut players: Vec<String> = Vec::new();
        for output in &self.outputs {
            let address = output.player_address();
            if !players.contains(&address) {
                players.push(address);
            }
        }
        players
    }

    /// A projector is a physical device: its area on the canvas is always
    /// its native resolution, so it can be moved but never stretched.
    pub fn lock_output_sizes(&mut self) {
        for output in &mut self.outputs {
            output.stage_width = output.resolution[0].max(1) as f32;
            output.stage_height = output.resolution[1].max(1) as f32;
            let blend = &mut output.blend;
            blend.left = blend.left.min(output.stage_width);
            blend.right = blend.right.min(output.stage_width);
            blend.top = blend.top.min(output.stage_height);
            blend.bottom = blend.bottom.min(output.stage_height);
        }
    }

    /// Sets each output's feathers to the width it actually overlaps other
    /// outputs on each side.
    pub fn auto_blend(&mut self) {
        let rects: Vec<_> = self
            .outputs
            .iter()
            .map(|o| {
                (
                    o.stage_x,
                    o.stage_y,
                    o.stage_x + o.stage_width,
                    o.stage_y + o.stage_height,
                )
            })
            .collect();
        for (i, output) in self.outputs.iter_mut().enumerate() {
            let (l, t, r, b) = rects[i];
            let (mut left, mut right, mut top, mut bottom) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
            for (j, &(ol, ot, or, ob)) in rects.iter().enumerate() {
                if i == j {
                    continue;
                }
                let overlap_x = r.min(or) - l.max(ol);
                let overlap_y = b.min(ob) - t.max(ot);
                if overlap_x <= 0.0 || overlap_y <= 0.0 {
                    continue;
                }
                // A neighbour covering the full height blends a vertical edge.
                if overlap_y >= overlap_x {
                    if ol <= l {
                        left = left.max(overlap_x);
                    } else {
                        right = right.max(overlap_x);
                    }
                } else if ot <= t {
                    top = top.max(overlap_y);
                } else {
                    bottom = bottom.max(overlap_y);
                }
            }
            output.blend.left = left.min(output.stage_width);
            output.blend.right = right.min(output.stage_width);
            output.blend.top = top.min(output.stage_height);
            output.blend.bottom = bottom.min(output.stage_height);
        }
    }
}

pub fn save_project_atomic(project: &ShowProject, path: &Path) -> Result<()> {
    project.validate()?;
    let parent = path.parent().context("project path has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("mapforge.json.tmp");
    let bytes = serde_json::to_vec_pretty(project)?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_project(path: &Path) -> Result<ShowProject> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > 32 * 1024 * 1024 {
        bail!("project file is larger than 32 MiB");
    }
    let project: ShowProject = serde_json::from_slice(&fs::read(path)?)?;
    project.validate()?;
    Ok(project)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u32,
    pub command_id: Uuid,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    LoadProject {
        project: ShowProject,
        /// Outputs this Player should draw; all of them when absent.
        #[serde(default)]
        outputs: Option<Vec<Uuid>>,
        /// This Player's address in the project, so it knows whether it is
        /// the master and which Players follow it.
        #[serde(default)]
        player: Option<String>,
    },
    Prepare {
        scene_id: Uuid,
    },
    Play,
    /// Starts an already prepared scene at this Player's local Unix time.
    PlayAt {
        start_time_unix_ms: u64,
    },
    Pause,
    Stop,
    Seek {
        seconds: f64,
    },
    /// Prepares a scene at a position and plays it in one step.
    /// Lets playback continue past the end of the loop it is in.
    ReleaseLoop,
    Cue {
        scene_id: Uuid,
        seconds: f64,
    },
    /// Prepares a scene now and starts it at this Player's local Unix time.
    CueAt {
        scene_id: Uuid,
        seconds: f64,
        start_time_unix_ms: u64,
    },
    SetVolume {
        value: f32,
    },
    SetMute {
        value: bool,
    },
    SetBlackout {
        value: bool,
    },
    GetState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stopped,
    Ready,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub protocol_version: u32,
    pub transport: Transport,
    pub scene_id: Option<Uuid>,
    pub volume: f32,
    pub muted: bool,
    pub blackout: bool,
    pub position_seconds: f64,
    /// Player wall-clock time at the instant this state was generated.
    #[serde(default)]
    pub server_time_unix_ms: u64,
    /// Future scheduled start, if the Player is prepared and waiting.
    #[serde(default)]
    pub scheduled_start_unix_ms: Option<u64>,
    /// How late the most recent scheduled start fired. Negative means early.
    #[serde(default)]
    pub start_error_ms: Option<f64>,
    /// Name of the loop currently repeating, if any.
    #[serde(default)]
    pub loop_name: Option<String>,
    /// On the master Player: the other Players and whether they answer.
    #[serde(default)]
    pub followers: Vec<FollowerState>,
    /// Master or sub, as chosen on the Player; `None` until it is set up.
    #[serde(default)]
    pub role: Option<PlayerRole>,
    /// The master starts the first scene by itself when it opens.
    #[serde(default)]
    pub autoplay: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FollowerState {
    pub address: String,
    pub online: bool,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            transport: Transport::Stopped,
            scene_id: None,
            volume: 1.0,
            muted: false,
            blackout: false,
            position_seconds: 0.0,
            server_time_unix_ms: 0,
            scheduled_start_unix_ms: None,
            start_error_ms: None,
            loop_name: None,
            followers: Vec::new(),
            role: None,
            autoplay: false,
            message: "Waiting for Producer".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_round_trip_is_deterministic() {
        let p = ShowProject::default();
        let a = serde_json::to_string_pretty(&p).unwrap();
        let decoded: ShowProject = serde_json::from_str(&a).unwrap();
        assert_eq!(p, decoded);
        assert_eq!(a, serde_json::to_string_pretty(&decoded).unwrap());
    }
    #[test]
    fn default_outputs_cover_stage_with_overlap() {
        let p = ShowProject::default();
        assert_eq!(p.outputs.len(), 2);
        assert_eq!(p.outputs[0].stage_x, 0.0);
        assert_eq!(
            p.outputs[1].stage_x + p.outputs[1].stage_width,
            p.stage.width
        );
        assert_eq!(p.outputs[0].stage_width - p.outputs[1].stage_x, 120.0);
    }

    #[test]
    fn opposing_ramps_sum_to_full_light() {
        let blend = EdgeBlend::default();
        for step in 0..=20 {
            let t = step as f32 / 20.0;
            let total = blend.light(t) + blend.light(1.0 - t);
            assert!((total - 1.0).abs() < 1e-4, "t={t} total={total}");
        }
        assert_eq!(blend.light(0.0), 0.0);
        assert_eq!(blend.light(1.0), 1.0);
    }

    #[test]
    fn projector_sizes_follow_resolution() {
        let mut p = ShowProject::default();
        p.outputs[0].stage_width = 4740.0;
        p.outputs[0].stage_height = 2666.0;
        p.outputs[0].blend.right = 3000.0;
        p.lock_output_sizes();
        assert_eq!(p.outputs[0].stage_width, 1920.0);
        assert_eq!(p.outputs[0].stage_height, 1080.0);
        assert_eq!(p.outputs[0].blend.right, 1920.0);
    }

    #[test]
    fn auto_blend_matches_overlap() {
        let mut p = ShowProject::default();
        p.outputs[0].blend.right = 0.0;
        p.outputs[1].blend.left = 0.0;
        p.auto_blend();
        assert_eq!(p.outputs[0].blend.right, 120.0);
        assert_eq!(p.outputs[1].blend.left, 120.0);
        assert_eq!(p.outputs[0].blend.left, 0.0);
    }

    #[test]
    fn older_projects_get_new_defaults() {
        let mut value = serde_json::to_value(ShowProject::default()).unwrap();
        value.as_object_mut().unwrap().remove("test_pattern");
        for output in value["outputs"].as_array_mut().unwrap() {
            output.as_object_mut().unwrap().remove("color");
            output.as_object_mut().unwrap().remove("display_index");
            output.as_object_mut().unwrap().remove("warp_corners");
            output.as_object_mut().unwrap().remove("mask");
            output["blend"].as_object_mut().unwrap().remove("luminance");
        }
        let project: ShowProject = serde_json::from_value(value).unwrap();
        assert_eq!(project.test_pattern, TestPattern::Off);
        assert_eq!(project.outputs[0].blend.luminance, 0.5);
        assert_eq!(project.outputs[0].color.brightness, 1.0);
        assert_eq!(project.outputs[0].display_index, None);
        assert_eq!(project.outputs[0].warp_corners, default_warp_corners());
        assert!(!project.outputs[0].mask.enabled);
    }

    #[test]
    fn clips_are_active_inside_their_window() {
        let layer = Layer {
            id: Uuid::new_v4(),
            asset_id: Uuid::new_v4(),
            name: "clip".into(),
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            opacity: 1.0,
            output_ids: vec![],
            timeline_start: 5.0,
            timeline_duration: 10.0,
            source_offset: 2.0,
            looping: false,
            volume: 1.0,
            audio: true,
        };
        assert!(!layer.active_at(4.9));
        assert!(layer.active_at(5.0));
        assert!(!layer.active_at(15.0));
        assert_eq!(layer.source_time(8.0), 5.0);
        let mut scene = Scene::new("A");
        scene.layers.push(layer);
        assert_eq!(scene.duration(), 15.0);
    }

    #[test]
    fn scene_hotkeys_default_to_function_keys() {
        let mut scene = Scene::new("Sea");
        assert_eq!(scene_hotkey(0, &scene), "F1");
        assert_eq!(scene_hotkey(12, &scene), "");
        scene.hotkey = "S".into();
        assert_eq!(scene_hotkey(0, &scene), "S");
    }

    #[test]
    fn player_addresses_are_normalized() {
        assert_eq!(normalize_player_address("local-player"), "127.0.0.1:4777");
        assert_eq!(normalize_player_address("192.168.1.9"), "192.168.1.9:4777");
        assert_eq!(normalize_player_address(" 10.0.0.2:5000 "), "10.0.0.2:5000");
        assert_eq!(player_host("10.0.0.2:5000"), "10.0.0.2");
        let mut p = ShowProject::default();
        p.outputs[1].player = "192.168.1.9".into();
        assert_eq!(p.players(), vec!["127.0.0.1:4777", "192.168.1.9:4777"]);
    }

    #[test]
    fn rejects_blend_wider_than_output() {
        let mut project = ShowProject::default();
        project.outputs[0].blend.right = project.outputs[0].stage_width + 1.0;
        assert!(project.validate().is_err());
    }

    #[test]
    fn rejects_invalid_warp_and_mask_geometry() {
        let mut project = ShowProject::default();
        project.outputs[0].warp_corners[0][0] = f32::NAN;
        assert!(project.validate().is_err());

        let mut project = ShowProject::default();
        project.outputs[0].mask.enabled = true;
        project.outputs[0].mask.points = vec![[0.0, 0.0], [1.0, 0.0]];
        assert!(project.validate().is_err());
    }
}
