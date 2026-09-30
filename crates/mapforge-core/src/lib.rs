use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use uuid::Uuid;

pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowProject {
    pub schema_version: u32,
    pub id: Uuid,
    pub name: String,
    pub stage: VirtualStage,
    pub assets: Vec<Asset>,
    pub scenes: Vec<Scene>,
    pub outputs: Vec<ProjectorOutput>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Image,
    Video,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scene {
    pub id: Uuid,
    pub name: String,
    pub layers: Vec<Layer>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectorOutput {
    pub id: Uuid,
    pub name: String,
    pub player: String,
    pub stage_x: f32,
    pub stage_y: f32,
    pub stage_width: f32,
    pub stage_height: f32,
}

impl Default for ShowProject {
    fn default() -> Self {
        let left = ProjectorOutput {
            id: Uuid::new_v4(),
            name: "Projector 1".into(),
            player: "local-player".into(),
            stage_x: 0.0,
            stage_y: 0.0,
            stage_width: 960.0,
            stage_height: 1080.0,
        };
        let right = ProjectorOutput {
            id: Uuid::new_v4(),
            name: "Projector 2".into(),
            player: "local-player".into(),
            stage_x: 960.0,
            stage_y: 0.0,
            stage_width: 960.0,
            stage_height: 1080.0,
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
            scenes: vec![Scene {
                id: Uuid::new_v4(),
                name: "Scene 1".into(),
                layers: vec![],
            }],
            outputs: vec![left, right],
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
        if self.scenes.len() > 10_000 || self.assets.len() > 100_000 {
            bail!("project exceeds safety limits");
        }
        Ok(())
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
    LoadProject { project: ShowProject },
    Prepare { scene_id: Uuid },
    Play,
    Pause,
    Stop,
    SetVolume { value: f32 },
    SetMute { value: bool },
    SetBlackout { value: bool },
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
    pub message: String,
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
    fn default_outputs_cover_stage() {
        let p = ShowProject::default();
        assert_eq!(p.outputs.len(), 2);
        assert_eq!(
            p.outputs.iter().map(|o| o.stage_width).sum::<f32>(),
            p.stage.width
        );
    }
}
