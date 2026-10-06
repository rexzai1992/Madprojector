//! The master Player runs the show without Producer: it passes the iPad's
//! commands on to the other Players, starts everyone together, and brings a
//! Player that comes online late (or after a restart) into the running show.

use crate::{apply_local, MediaPool, Runtime};
use mapforge_core::{
    net::{estimate_clock_offset_ms, exchange, unix_time_ms},
    Command, FollowerState, Transport,
};
use std::{
    sync::{
        mpsc::{self, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Time from a button press to every Player starting together. Long enough
/// for the commands to cross the LAN and the decoders to open their files.
pub const LEAD_MS: u64 = 800;

#[derive(Default)]
struct Status {
    online: bool,
    /// Follower wall clock minus ours.
    clock_offset_ms: Option<f64>,
    /// Answers since it came online, and the quickest recent round trip.
    /// The offset is taken from quick round trips, which are the most
    /// accurate; a PC that is busy starting up answers slowly at first.
    samples: u32,
    best_rtt_ms: f64,
}

/// Clock readings wanted before starting a follower on its own clock.
const READY_SAMPLES: u32 = 3;

impl Status {
    fn record(&mut self, offset_ms: f64, rtt_ms: f64) {
        // Let the best round trip age slowly, so the offset keeps tracking
        // drift between the PCs' clocks.
        self.best_rtt_ms *= 1.02;
        if self.samples == 0 || rtt_ms <= self.best_rtt_ms {
            self.best_rtt_ms = rtt_ms;
            self.clock_offset_ms = Some(offset_ms);
        }
        self.samples += 1;
    }
}

struct Follower {
    address: String,
    tx: Sender<Command>,
    status: Arc<Mutex<Status>>,
}

#[derive(Default)]
pub struct Relay {
    followers: Mutex<Vec<Follower>>,
}

impl Relay {
    /// Keeps one link per follower named by the show, every half second.
    pub fn run(self: Arc<Self>, shared: Arc<Mutex<Runtime>>) {
        loop {
            let wanted = shared.lock().unwrap().followers();
            {
                let mut followers = self.followers.lock().unwrap();
                followers.retain(|f| wanted.contains(&f.address));
                for address in &wanted {
                    if !followers.iter().any(|f| &f.address == address) {
                        followers.push(Follower::spawn(address.clone(), shared.clone()));
                    }
                }
            }
            shared.lock().unwrap().state.followers = self.states();
            thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn states(&self) -> Vec<FollowerState> {
        self.followers
            .lock()
            .unwrap()
            .iter()
            .map(|f| FollowerState {
                address: f.address.clone(),
                online: f.status.lock().unwrap().online,
            })
            .collect()
    }

    /// Whether every follower in `wanted` answers and its clock is known.
    pub fn all_ready(&self, wanted: &[String]) -> bool {
        let followers = self.followers.lock().unwrap();
        wanted.iter().all(|address| {
            followers.iter().any(|f| {
                let status = f.status.lock().unwrap();
                &f.address == address && status.online && status.samples >= READY_SAMPLES
            })
        })
    }

    fn send(&self, command: &Command) {
        for follower in self.followers.lock().unwrap().iter() {
            let _ = follower.tx.send(command.clone());
        }
    }

    /// Starts `scene_id` at `seconds` on every follower at our `start_ms`,
    /// converted to each follower's own clock.
    fn cue_at(&self, scene_id: Uuid, seconds: f64, start_ms: u64) {
        for follower in self.followers.lock().unwrap().iter() {
            let _ = follower.tx.send(Command::CueAt {
                scene_id,
                seconds,
                start_time_unix_ms: follower.start_time(start_ms),
            });
        }
    }
}

impl Follower {
    fn spawn(address: String, shared: Arc<Mutex<Runtime>>) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let status = Arc::new(Mutex::new(Status::default()));
        let worker_status = status.clone();
        let target = address.clone();
        thread::spawn(move || loop {
            let command = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => Command::GetState,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let sent_ms = unix_time_ms() as f64;
            let started = Instant::now();
            let result = exchange(&target, command);
            let came_online = {
                let mut status = worker_status.lock().unwrap();
                match result {
                    Ok(state) => {
                        let rtt_ms = started.elapsed().as_secs_f64() * 1000.0;
                        if state.server_time_unix_ms > 0 {
                            status.record(
                                estimate_clock_offset_ms(
                                    sent_ms,
                                    rtt_ms,
                                    state.server_time_unix_ms as f64,
                                ),
                                rtt_ms,
                            );
                        }
                        !std::mem::replace(&mut status.online, true)
                    }
                    Err(_) => {
                        *status = Status::default();
                        false
                    }
                }
            };
            if came_online {
                // A few quick readings first, so a late joiner starts on time.
                for _ in 1..READY_SAMPLES {
                    thread::sleep(Duration::from_millis(100));
                    let sent_ms = unix_time_ms() as f64;
                    let started = Instant::now();
                    if let Ok(state) = exchange(&target, Command::GetState) {
                        let rtt_ms = started.elapsed().as_secs_f64() * 1000.0;
                        worker_status.lock().unwrap().record(
                            estimate_clock_offset_ms(
                                sent_ms,
                                rtt_ms,
                                state.server_time_unix_ms as f64,
                            ),
                            rtt_ms,
                        );
                    }
                }
                let offset = worker_status.lock().unwrap().clock_offset_ms;
                for command in catch_up(&shared, &target, offset.unwrap_or(0.0)) {
                    let _ = exchange(&target, command);
                }
            }
        });
        Self {
            address,
            tx,
            status,
        }
    }

    fn start_time(&self, start_ms: u64) -> u64 {
        let offset = self.status.lock().unwrap().clock_offset_ms.unwrap_or(0.0);
        start_ms.saturating_add_signed(offset.round() as i64)
    }
}

/// Commands that make a follower that just answered match this Player: the
/// current show, output settings, and the scene at the same moment.
fn catch_up(shared: &Mutex<Runtime>, address: &str, offset_ms: f64) -> Vec<Command> {
    let runtime = shared.lock().unwrap();
    let Some(project) = runtime.project.clone() else {
        return Vec::new();
    };
    let outputs = project
        .outputs
        .iter()
        .filter(|o| o.player_address() == address)
        .map(|o| o.id)
        .collect();
    let at_follower = |ms: u64| ms.saturating_add_signed(offset_ms.round() as i64);
    let state = &runtime.state;
    let mut commands = vec![
        Command::LoadProject {
            project,
            outputs: Some(outputs),
            player: Some(address.to_string()),
        },
        Command::SetBlackout {
            value: state.blackout,
        },
        Command::SetVolume {
            value: state.volume,
        },
        Command::SetMute { value: state.muted },
    ];
    match (state.transport.clone(), state.scene_id, runtime.scheduled) {
        (_, _, Some(start)) => commands.push(Command::CueAt {
            scene_id: start.scene_id,
            seconds: start.seconds,
            start_time_unix_ms: at_follower(start.unix_ms),
        }),
        (Transport::Playing, Some(scene_id), None) => {
            let start = unix_time_ms() + LEAD_MS;
            commands.push(Command::CueAt {
                scene_id,
                seconds: runtime.clock.position() + LEAD_MS as f64 / 1000.0,
                start_time_unix_ms: at_follower(start),
            });
        }
        (Transport::Paused | Transport::Ready, Some(scene_id), None) => {
            commands.push(Command::Prepare { scene_id });
            commands.push(Command::Seek {
                seconds: runtime.clock.position(),
            });
        }
        _ => commands.push(Command::Stop),
    }
    commands
}

/// A button on the iPad page. The master Player starts every Player
/// together; any other Player only obeys for itself.
pub fn control(shared: &Mutex<Runtime>, media: &MediaPool, relay: &Relay, command: Command) {
    if !shared.lock().unwrap().is_master() {
        apply_local(shared, media, command);
        return;
    }
    let cue = match command {
        Command::Cue { scene_id, seconds } => Some((scene_id, seconds)),
        // Resume or start everyone from the same place, so a pause that
        // reached the Players a few milliseconds apart is evened out.
        Command::Play => {
            let runtime = shared.lock().unwrap();
            if runtime.state.transport == Transport::Playing {
                return;
            }
            let first = runtime
                .project
                .as_ref()
                .and_then(|p| p.scenes.first())
                .map(|s| s.id);
            let Some(scene_id) = runtime.state.scene_id.or(first) else {
                return;
            };
            Some((scene_id, runtime.clock.position()))
        }
        _ => None,
    };
    match cue {
        Some((scene_id, seconds)) => {
            let start = unix_time_ms() + LEAD_MS;
            apply_local(
                shared,
                media,
                Command::CueAt {
                    scene_id,
                    seconds,
                    start_time_unix_ms: start,
                },
            );
            relay.cue_at(scene_id, seconds, start);
        }
        None => {
            apply_local(shared, media, command.clone());
            relay.send(&command);
        }
    }
}

/// Starts the first scene on every Player once they all answer, or after a
/// short wait so one missing PC can't hold up the show (it joins later).
pub fn autoplay(shared: Arc<Mutex<Runtime>>, media: Arc<MediaPool>, relay: Arc<Relay>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let wanted = shared.lock().unwrap().followers();
        if relay.all_ready(&wanted) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let first = {
        let runtime = shared.lock().unwrap();
        runtime
            .project
            .as_ref()
            .and_then(|p| p.scenes.first())
            .map(|s| s.id)
    };
    if let Some(scene_id) = first {
        control(
            &shared,
            &media,
            &relay,
            Command::Cue {
                scene_id,
                seconds: 0.0,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_offset_comes_from_the_quickest_round_trip() {
        let mut status = Status::default();
        status.record(70.0, 150.0); // busy at startup
        status.record(2.0, 3.0);
        status.record(40.0, 90.0);
        assert_eq!(status.clock_offset_ms, Some(2.0));
        assert_eq!(status.samples, 3);
    }
}
