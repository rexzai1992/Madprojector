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
    /// The last exchange failed, so the Player is known to be unreachable.
    /// A link that has not been tried yet is not unreachable.
    pub unreachable: bool,
    pub state: Option<PlayerState>,
    pub reply: Option<String>,
    pub sync_failed: bool,
    /// The show this Player last accepted. Controller edits made on the
    /// Player are the differences between this and its `project_update`.
    pub confirmed: Option<ShowProject>,
    /// A show the Player turned down, with its reason.
    pub rejected: Option<(ShowProject, String)>,
    pub latency_ms: Option<f64>,
    /// Player wall clock minus Producer wall clock, estimated at RTT midpoint.
    pub clock_offset_ms: Option<f64>,
}

/// Transport commands this old are dropped instead of sent while the Player
/// is unreachable: replaying minutes-old GOs, stops and seeks when it comes
/// back would only put it out of step with the others.
const STALE_COMMAND: Duration = Duration::from_secs(3);

/// Something waiting to be sent to one Player.
enum Item {
    /// Send the newest show waiting in the link's sync slot, if any.
    Sync,
    Command {
        command: Command,
        report: bool,
        queued: Instant,
    },
}

/// Settings where only the latest value matters, so a newer one replaces
/// any older one still waiting.
fn is_setting(command: &Command) -> bool {
    matches!(
        command,
        Command::SetVolume { .. } | Command::SetMute { .. } | Command::SetBlackout { .. }
    )
}

/// Commands waiting for one Player, in order. It stays small while a Player
/// is offline or slow: the show itself waits in a single sync slot that every
/// edit overwrites, settings keep only their newest value, and old transport
/// commands are dropped while the Player can't be reached. So a GO never
/// queues behind a backlog of stale shows.
#[derive(Default)]
struct Backlog {
    items: std::collections::VecDeque<Item>,
}

impl Backlog {
    fn push(&mut self, item: Item) {
        match &item {
            // A waiting marker already sends the newest show when reached.
            Item::Sync if self.items.iter().any(|i| matches!(i, Item::Sync)) => return,
            Item::Command { command, .. } if is_setting(command) => {
                let kind = std::mem::discriminant(command);
                self.items.retain(|i| {
                    !matches!(i, Item::Command { command, .. }
                        if std::mem::discriminant(command) == kind)
                });
            }
            _ => {}
        }
        self.items.push_back(item);
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The next item to send and how many stale commands were dropped to
    /// reach it.
    fn pop(&mut self, unreachable: bool, now: Instant) -> (Option<Item>, usize) {
        let mut dropped = 0;
        while let Some(item) = self.items.pop_front() {
            let stale = matches!(&item, Item::Command { command, queued, .. }
                if unreachable
                    && !is_setting(command)
                    && now.duration_since(*queued) > STALE_COMMAND);
            if !stale {
                return (Some(item), dropped);
            }
            dropped += 1;
        }
        (None, dropped)
    }
}

pub struct PlayerLink {
    pub address: String,
    tx: Sender<Item>,
    /// The newest show waiting to be sent; see [`Backlog`].
    pending_sync: Arc<Mutex<Option<Command>>>,
    pub status: Arc<Mutex<LinkStatus>>,
    pub lan: Arc<Mutex<LanStatus>>,
    /// The show as last sent to this Player.
    pub synced: Option<ShowProject>,
    /// The show this Player last turned down, so the same show is not sent
    /// again until it is edited.
    pub rejected: Option<ShowProject>,
    pub was_online: bool,
}

impl PlayerLink {
    pub fn spawn(address: String, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel::<Item>();
        let pending_sync = Arc::new(Mutex::new(None::<Command>));
        let worker_sync = pending_sync.clone();
        let status = Arc::new(Mutex::new(LinkStatus::default()));
        let worker_status = status.clone();
        let target = address.clone();
        thread::spawn(move || {
            let mut backlog = Backlog::default();
            loop {
                if backlog.is_empty() {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(item) => backlog.push(item),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                while let Ok(item) = rx.try_recv() {
                    backlog.push(item);
                }
                let unreachable = worker_status.lock().unwrap().unreachable;
                let (next, dropped) = backlog.pop(unreachable, Instant::now());
                if dropped > 0 {
                    worker_status.lock().unwrap().reply = Some(format!(
                        "{target} offline: skipped {dropped} old command(s)"
                    ));
                    ctx.request_repaint();
                }
                let (command, report) = match next {
                    Some(Item::Command { command, report, .. }) => (command, report),
                    Some(Item::Sync) => match worker_sync.lock().unwrap().take() {
                        Some(command) => (command, false),
                        None => continue,
                    },
                    None if dropped > 0 => continue,
                    None => (Command::GetState, false),
                };
                exchange_and_record(&target, command, report, &worker_status);
                ctx.request_repaint();
            }
        });
        Self {
            address,
            tx,
            pending_sync,
            status,
            lan: Arc::new(Mutex::new(LanStatus::default())),
            synced: None,
            rejected: None,
            was_online: false,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send(Item::Command {
            command,
            report: true,
            queued: Instant::now(),
        });
    }

    /// Queues `command` (a `LoadProject`) in place of any show still waiting
    /// to be sent to this Player.
    pub fn sync(&self, command: Command) {
        *self.pending_sync.lock().unwrap() = Some(command);
        let _ = self.tx.send(Item::Sync);
    }

    pub fn host(&self) -> String {
        player_host(&self.address)
    }
}

/// Sends one command and records the Player's answer in `status`.
fn exchange_and_record(
    target: &str,
    command: Command,
    report: bool,
    status: &Mutex<LinkStatus>,
) {
    let sent_project = match &command {
        Command::LoadProject { project, .. } => Some(project.clone()),
        _ => None,
    };
    let is_sync = sent_project.is_some();
    let is_poll = matches!(command, Command::GetState);
    let started = Instant::now();
    let sent_unix_ms = unix_time_ms() as f64;
    let result = exchange(target, command);
    let mut status = status.lock().unwrap();
    match result {
        Ok(state) => {
            status.online = true;
            status.unreachable = false;
            let rtt_ms = started.elapsed().as_secs_f64() * 1000.0;
            if is_poll || status.latency_ms.is_none() {
                status.latency_ms = Some(rtt_ms);
            }
            if state.server_time_unix_ms > 0 && (is_poll || status.clock_offset_ms.is_none()) {
                status.clock_offset_ms = Some(estimate_clock_offset_ms(
                    sent_unix_ms,
                    rtt_ms,
                    state.server_time_unix_ms as f64,
                ));
            }
            if let Some(project) = sent_project {
                // The Player answers a show it refuses with this message and
                // keeps playing its old show.
                if state.message.starts_with("Rejected project") {
                    status.rejected = Some((project, state.message.clone()));
                } else {
                    status.confirmed = Some(project);
                }
            }
            if report {
                status.reply = Some(state.message.clone());
            }
            status.state = Some(state);
        }
        Err(error) => {
            status.online = false;
            status.unreachable = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn command(command: Command, queued: Instant) -> Item {
        Item::Command {
            command,
            report: true,
            queued,
        }
    }

    fn drain(backlog: &mut Backlog, unreachable: bool, now: Instant) -> Vec<String> {
        let mut sent = Vec::new();
        loop {
            match backlog.pop(unreachable, now).0 {
                Some(Item::Sync) => sent.push("Sync".to_string()),
                Some(Item::Command { command, .. }) => sent.push(format!("{command:?}")),
                None => return sent,
            }
        }
    }

    #[test]
    fn repeated_syncs_and_settings_do_not_pile_up() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        for _ in 0..500 {
            backlog.push(Item::Sync);
            backlog.push(command(Command::SetVolume { value: 0.5 }, now));
        }
        backlog.push(command(Command::SetBlackout { value: true }, now));
        backlog.push(command(Command::SetVolume { value: 0.2 }, now));
        backlog.push(command(Command::Play, now));
        assert_eq!(
            drain(&mut backlog, false, now),
            [
                "Sync",
                "SetBlackout { value: true }",
                "SetVolume { value: 0.2 }",
                "Play"
            ]
        );
    }

    #[test]
    fn old_transport_commands_are_dropped_only_while_unreachable() {
        let old = Instant::now();
        let now = old + Duration::from_secs(10);
        let mut backlog = Backlog::default();
        backlog.push(command(Command::Stop, old));
        backlog.push(command(Command::SetMute { value: true }, old));
        backlog.push(command(Command::Play, now));
        assert_eq!(
            drain(&mut backlog, true, now),
            ["SetMute { value: true }", "Play"]
        );

        backlog.push(command(Command::Stop, old));
        assert_eq!(drain(&mut backlog, false, now), ["Stop"]);
    }
}
