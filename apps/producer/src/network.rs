//! Talking to Player PCs: one background worker per Player sends commands in
//! order and polls state every second, plus LAN tools for checking speed and
//! copying media to each Player.

use eframe::egui;
pub use mapforge_core::net::unix_time_ms;
use mapforge_core::{
    net::{connect, estimate_clock_offset_ms, exchange},
    player_host, Asset, AssetStatus, Command, PlayerState, ShowProject, CONTROLLER_PORT,
};
use std::{
    io::{Read, Write},
    sync::{
        mpsc::{self, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Default)]
pub struct LinkStatus {
    pub online: bool,
    pub state: Option<PlayerState>,
    pub reply: Option<String>,
    pub sync_failed: bool,
    pub latency_ms: Option<f64>,
    /// Player wall clock minus Producer wall clock, estimated at RTT midpoint.
    pub clock_offset_ms: Option<f64>,
}

pub struct PlayerLink {
    pub address: String,
    tx: Sender<(Command, bool)>,
    pub status: Arc<Mutex<LinkStatus>>,
    pub lan: Arc<Mutex<LanStatus>>,
    /// The show as last sent to this Player.
    pub synced: Option<ShowProject>,
    pub was_online: bool,
}

impl PlayerLink {
    pub fn spawn(address: String, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel::<(Command, bool)>();
        let status = Arc::new(Mutex::new(LinkStatus::default()));
        let worker_status = status.clone();
        let target = address.clone();
        thread::spawn(move || loop {
            let (command, report) = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => (Command::GetState, false),
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let is_sync = matches!(command, Command::LoadProject { .. });
            let is_poll = matches!(command, Command::GetState);
            let started = Instant::now();
            let sent_unix_ms = unix_time_ms() as f64;
            let result = exchange(&target, command);
            {
                let mut status = worker_status.lock().unwrap();
                match result {
                    Ok(state) => {
                        status.online = true;
                        let rtt_ms = started.elapsed().as_secs_f64() * 1000.0;
                        if is_poll || status.latency_ms.is_none() {
                            status.latency_ms = Some(rtt_ms);
                        }
                        if state.server_time_unix_ms > 0
                            && (is_poll || status.clock_offset_ms.is_none())
                        {
                            status.clock_offset_ms = Some(estimate_clock_offset_ms(
                                sent_unix_ms,
                                rtt_ms,
                                state.server_time_unix_ms as f64,
                            ));
                        }
                        if report {
                            status.reply = Some(state.message.clone());
                        }
                        status.state = Some(state);
                    }
                    Err(error) => {
                        status.online = false;
                        status.state = None;
                        status.latency_ms = None;
                        status.clock_offset_ms = None;
                        status.sync_failed |= is_sync;
                        if report {
                            status.reply = Some(format!("{target} offline: {error}"));
                        }
                    }
                }
            }
            ctx.request_repaint();
        });
        Self {
            address,
            tx,
            status,
            lan: Arc::new(Mutex::new(LanStatus::default())),
            synced: None,
            was_online: false,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send((command, true));
    }

    pub fn send_quiet(&self, command: Command) {
        let _ = self.tx.send((command, false));
    }

    pub fn host(&self) -> String {
        player_host(&self.address)
    }
}

// ---------------------------------------------------------------------------
// LAN tools

#[derive(Default)]
pub struct LanStatus {
    /// What is running right now, e.g. "Testing speed…".
    pub busy: Option<String>,
    pub media: Option<MediaReport>,
    pub speed_mbps: Option<f64>,
    pub transfer: Option<Transfer>,
    pub message: String,
}

pub struct MediaReport {
    pub ready: usize,
    pub total: usize,
    pub missing: Vec<Uuid>,
}

pub struct Transfer {
    pub name: String,
    pub file: usize,
    pub files: usize,
    pub done: u64,
    pub total: u64,
    pub started: Instant,
}

impl Transfer {
    pub fn mbps(&self) -> f64 {
        let seconds = self.started.elapsed().as_secs_f64().max(0.001);
        self.done as f64 * 8.0 / seconds / 1_000_000.0
    }
}

const SPEED_TEST_BYTES: u64 = 64 * 1024 * 1024;

/// Runs `job` on a thread unless another LAN job is already running.
fn run_job(
    lan: &Arc<Mutex<LanStatus>>,
    ctx: &egui::Context,
    label: &str,
    job: impl FnOnce(&Arc<Mutex<LanStatus>>) -> Result<String, String> + Send + 'static,
) {
    {
        let mut status = lan.lock().unwrap();
        if status.busy.is_some() {
            return;
        }
        status.busy = Some(label.to_string());
    }
    let lan = lan.clone();
    let ctx = ctx.clone();
    thread::spawn(move || {
        let result = job(&lan);
        let mut status = lan.lock().unwrap();
        status.busy = None;
        status.transfer = None;
        status.message = match result {
            Ok(message) => message,
            Err(error) => format!("Failed: {error}"),
        };
        ctx.request_repaint();
    });
}

pub fn speed_test(host: String, lan: &Arc<Mutex<LanStatus>>, ctx: &egui::Context) {
    run_job(lan, ctx, "Testing LAN speed…", move |lan| {
        let started = Instant::now();
        let mut body = std::io::repeat(0).take(SPEED_TEST_BYTES);
        let (code, _) = http_call(
            &host,
            "POST",
            "/api/bandwidth",
            Some((SPEED_TEST_BYTES, &mut body)),
            |_| {},
        )?;
        if code != 200 {
            return Err(format!("Player answered {code}"));
        }
        let mbps = SPEED_TEST_BYTES as f64 * 8.0 / started.elapsed().as_secs_f64() / 1_000_000.0;
        lan.lock().unwrap().speed_mbps = Some(mbps);
        Ok(format!("{mbps:.0} Mbit/s ({})", speed_hint(mbps)))
    });
}

pub fn speed_hint(mbps: f64) -> &'static str {
    if mbps >= 800.0 {
        "gigabit, good for show files"
    } else if mbps >= 300.0 {
        "OK; large files take a while"
    } else if mbps >= 80.0 {
        "slow: probably 100 Mbit or Wi-Fi"
    } else {
        "too slow for big media: check cables/switch"
    }
}

pub fn check_media(host: String, lan: &Arc<Mutex<LanStatus>>, ctx: &egui::Context) {
    run_job(lan, ctx, "Checking media…", move |lan| {
        let report = fetch_media_report(&host)?;
        let message = format!(
            "{} of {} media files on this PC",
            report.ready, report.total
        );
        lan.lock().unwrap().media = Some(report);
        Ok(message)
    });
}

fn fetch_media_report(host: &str) -> Result<MediaReport, String> {
    let (code, body) = http_call(host, "GET", "/api/assets", None, |_| {})?;
    if code != 200 {
        return Err(format!("Player answered {code}"));
    }
    let statuses: Vec<AssetStatus> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(MediaReport {
        ready: statuses.iter().filter(|s| s.ready).count(),
        total: statuses.len(),
        missing: statuses
            .iter()
            .filter(|s| !s.ready)
            .map(|s| s.asset_id)
            .collect(),
    })
}

/// Copies every asset the Player does not have yet. The Player checks each
/// file's SHA-256 before using it.
pub fn send_media(
    host: String,
    assets: Vec<Asset>,
    lan: &Arc<Mutex<LanStatus>>,
    ctx: &egui::Context,
) {
    let repaint = ctx.clone();
    run_job(lan, ctx, "Sending media…", move |lan| {
        // Give the show sent just before this a moment to arrive.
        thread::sleep(Duration::from_millis(400));
        let report = fetch_media_report(&host)?;
        let todo: Vec<&Asset> = assets
            .iter()
            .filter(|a| report.missing.contains(&a.id))
            .collect();
        let files = todo.len();
        for (index, asset) in todo.into_iter().enumerate() {
            let mut file =
                std::fs::File::open(&asset.path).map_err(|e| format!("{}: {e}", asset.name))?;
            let total = file.metadata().map_err(|e| e.to_string())?.len();
            lan.lock().unwrap().transfer = Some(Transfer {
                name: asset.name.clone(),
                file: index + 1,
                files,
                done: 0,
                total,
                started: Instant::now(),
            });
            let (code, body) = http_call(
                &host,
                "PUT",
                &format!("/api/media/{}", asset.id),
                Some((total, &mut file)),
                |done| {
                    if let Some(transfer) = lan.lock().unwrap().transfer.as_mut() {
                        transfer.done = done;
                    }
                    repaint.request_repaint();
                },
            )?;
            if code != 200 {
                return Err(format!("{}: {}", asset.name, body.trim()));
            }
        }
        let report = fetch_media_report(&host)?;
        let message = format!(
            "Sent {files} file(s) · {} of {} ready",
            report.ready, report.total
        );
        lan.lock().unwrap().media = Some(report);
        Ok(message)
    });
}

/// Minimal HTTP/1.1 client for the Player's port 8080. Streams `body` and
/// reports progress in bytes.
fn http_call(
    host: &str,
    method: &str,
    path: &str,
    body: Option<(u64, &mut dyn Read)>,
    mut progress: impl FnMut(u64),
) -> Result<(u16, String), String> {
    let mut stream = connect(&format!("{host}:{CONTROLLER_PORT}"), Duration::from_secs(3))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let length = body.as_ref().map_or(0, |(len, _)| *len);
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    if let Some((_, reader)) = body {
        let mut buffer = vec![0_u8; 1 << 20];
        let mut sent = 0_u64;
        loop {
            let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            stream.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
            sent += n as u64;
            progress(sent);
        }
    }
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response).to_string();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or("invalid HTTP response")?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((code, body))
}
