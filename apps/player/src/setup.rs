//! This PC's job, chosen once on first start (master or sub), and how a sub
//! keeps up with its master: it fetches the show and any missing media from
//! the master, so Producer only has to reach the master.

use crate::{apply_local, asset_status, cached_path, media_cache_dir, MediaPool, Runtime};
use mapforge_core::{net::connect, Asset, Command, PlayerRole, ShowProject, CONTROLLER_PORT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, UdpSocket},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlayerSettings {
    /// `None` until someone answers the first-start question.
    pub role: Option<PlayerRole>,
    /// Sub only: the master PC's IP address.
    pub master: String,
    /// Master only: start the first scene as soon as the Player opens.
    pub autoplay: bool,
}

impl PlayerSettings {
    pub fn complete(&self) -> bool {
        match self.role {
            Some(PlayerRole::Master) => true,
            Some(PlayerRole::Sub) => !self.master.trim().is_empty(),
            None => false,
        }
    }

    /// The master's controller address, `host:8080` unless a port is given.
    pub fn master_http(&self) -> Option<String> {
        let master = self.master.trim();
        if self.role != Some(PlayerRole::Sub) || master.is_empty() {
            None
        } else if master.contains(':') {
            Some(master.to_string())
        } else {
            Some(format!("{master}:{CONTROLLER_PORT}"))
        }
    }
}

fn settings_path() -> PathBuf {
    media_cache_dir().join("player-settings.json")
}

pub fn load_settings() -> PlayerSettings {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &PlayerSettings) -> std::io::Result<()> {
    fs::create_dir_all(media_cache_dir())?;
    let json = serde_json::to_string_pretty(settings).unwrap();
    fs::write(settings_path(), json)
}

/// This PC's IP on the route towards `host`. Nothing is sent.
pub fn local_ip_toward(host: &str) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect((host, 9)).ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// This PC's main LAN address, for showing to the operator.
pub fn local_ip() -> Option<String> {
    local_ip_toward("192.0.2.1")
}

// ---------------------------------------------------------------------------
// Sub: follow the master

/// Every two seconds: fetch the master's show, take it if it changed, and
/// copy any media this PC is missing.
pub fn follow_master(shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>) {
    loop {
        thread::sleep(Duration::from_secs(2));
        let Some(http) = shared.lock().unwrap().settings.master_http() else {
            continue;
        };
        let note = match sync_with_master(&http, &shared, &media) {
            Ok(note) => note,
            Err(error) => format!("Can't reach the master at {http}: {error}"),
        };
        shared.lock().unwrap().link_note = note;
    }
}

fn sync_with_master(
    http: &str,
    shared: &Mutex<Runtime>,
    media: &MediaPool,
) -> Result<String, String> {
    let (_, body) = http_get(http, "/api/show")?;
    let mut json = String::new();
    body.take(64 * 1024 * 1024)
        .read_to_string(&mut json)
        .map_err(|e| e.to_string())?;
    let project: ShowProject =
        serde_json::from_str(&json).map_err(|_| "the master has no show yet".to_string())?;
    let host = http.rsplit_once(':').map_or(http, |(host, _)| host);
    let me = {
        let mut runtime = shared.lock().unwrap();
        if let Some(ip) = local_ip_toward(host) {
            if !runtime.local_ips.contains(&ip) {
                runtime.local_ips.push(ip);
            }
        }
        let me = project.players().into_iter().find(|a| runtime.is_me(a));
        if runtime.project.as_ref() == Some(&project) {
            None
        } else {
            Some(me)
        }
    };
    if let Some(me) = me {
        let outputs = project
            .outputs
            .iter()
            .filter(|o| Some(o.player_address()) == me)
            .map(|o| o.id)
            .collect();
        apply_local(
            shared,
            media,
            Command::LoadProject {
                project: project.clone(),
                outputs: Some(outputs),
                player: me,
            },
        );
    }
    let missing: Vec<Asset> = project
        .assets
        .iter()
        .filter(|a| !asset_status(a).ready)
        .cloned()
        .collect();
    for (index, asset) in missing.iter().enumerate() {
        let label = format!(
            "Copying media from the master ({}/{}): {}",
            index + 1,
            missing.len(),
            asset.name
        );
        download(http, asset, &label, shared, media).map_err(|e| format!("{}: {e}", asset.name))?;
    }
    let runtime = shared.lock().unwrap();
    let mine: Vec<&str> = project
        .outputs
        .iter()
        .filter(|o| runtime.is_me(&o.player_address()))
        .map(|o| o.name.as_str())
        .collect();
    Ok(if mine.is_empty() {
        format!(
            "Connected to the master, but no projector uses this PC's IP ({}). \
             Set it in Producer's Edit projectors.",
            runtime.local_ips.last().map_or("?", |ip| ip.as_str())
        )
    } else {
        format!("Connected to the master · showing {}", mine.join(", "))
    })
}

/// Copies one asset from the master into the media cache, checking its
/// SHA-256 before use.
fn download(
    http: &str,
    asset: &Asset,
    label: &str,
    shared: &Mutex<Runtime>,
    media: &MediaPool,
) -> Result<(), String> {
    let (length, mut body) = http_get(http, &format!("/api/media/{}", asset.id))?;
    let target = cached_path(asset);
    let partial = target.with_extension("part");
    let result = (|| -> Result<String, String> {
        fs::create_dir_all(media_cache_dir()).map_err(|e| e.to_string())?;
        let mut file =
            std::io::BufWriter::new(fs::File::create(&partial).map_err(|e| e.to_string())?);
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; 1 << 20];
        let mut done = 0_u64;
        let mut shown = 0_u64;
        loop {
            let n = body.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
            done += n as u64;
            if done - shown >= 8 << 20 {
                shown = done;
                let percent = done * 100 / length.max(1);
                shared.lock().unwrap().link_note = format!("{label} · {percent}%");
            }
        }
        file.flush().map_err(|e| e.to_string())?;
        if done != length {
            return Err("transfer cut off".into());
        }
        Ok(format!("{:x}", hash.finalize()))
    })();
    match result {
        Ok(checksum) if checksum == asset.checksum_sha256 => {
            fs::rename(&partial, &target).map_err(|e| e.to_string())?;
            let runtime = shared.lock().unwrap();
            if let (Some(project), Some(scene_id)) = (&runtime.project, runtime.state.scene_id) {
                media.sync(project, scene_id, false, runtime.clock.position());
            }
            Ok(())
        }
        Ok(_) => {
            let _ = fs::remove_file(&partial);
            Err("checksum mismatch".into())
        }
        Err(error) => {
            let _ = fs::remove_file(&partial);
            Err(error)
        }
    }
}

/// Minimal HTTP GET: returns the body length and a reader for the body.
fn http_get(http: &str, path: &str) -> Result<(u64, BufReader<TcpStream>), String> {
    let mut stream = connect(http, Duration::from_secs(3))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {http}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let code = status
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if code != "200" {
        return Err(format!("master answered {code}"));
    }
    Ok((length, reader))
}

/// Passes an iPad button press on a sub to the master, so the iPad works
/// from any PC and every PC stays together.
pub fn forward_to_master(http: &str, path: &str) -> Result<(), String> {
    let mut stream = connect(http, Duration::from_secs(2))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {http}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut status = String::new();
    BufReader::new(stream)
        .read_line(&mut status)
        .map_err(|e| e.to_string())?;
    match status.split_whitespace().nth(1) {
        Some(code) if code.starts_with('2') => Ok(()),
        code => Err(format!("master answered {}", code.unwrap_or("nothing"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sub_needs_the_master_ip() {
        let mut settings = PlayerSettings::default();
        assert!(!settings.complete());
        settings.role = Some(PlayerRole::Master);
        assert!(settings.complete());
        assert_eq!(settings.master_http(), None);
        settings.role = Some(PlayerRole::Sub);
        assert!(!settings.complete());
        settings.master = " 192.168.50.11 ".into();
        assert!(settings.complete());
        assert_eq!(
            settings.master_http().as_deref(),
            Some("192.168.50.11:8080")
        );
        settings.master = "10.0.0.5:9000".into();
        assert_eq!(settings.master_http().as_deref(), Some("10.0.0.5:9000"));
    }
}
