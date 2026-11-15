//! The master Player runs the show without Producer: it passes the iPad's
//! commands on to the other Players, starts everyone together, and brings a
//! Player that comes online late (or after a restart) into the running show.

use crate::{apply_local, MediaPool, Runtime};
use mapforge_core::{
    net::{estimate_clock_offset_ms, exchange, unix_time_ms},
    Command, FollowerState, PlayerState, ShowProject, Transport,
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

/// Failed exchanges in a row before a follower counts as offline. One slow
/// answer (a busy LAN during a media copy) is not an outage, and coming back
/// online re-cues the follower, which hitches its picture.
const OFFLINE_AFTER_FAILURES: u32 = 3;

/// How far apart, in seconds, a follower and this Player may be and still
/// count as playing together: well above clock-sync error, and well below
/// any loop.
const IN_STEP_SECONDS: f64 = 0.05;
/// Least time between re-cues of a follower that fell out of step without
/// going offline (a Player restarted within the offline window comes back
/// stopped; a start can land off time), so a follower still starting the last
/// cue (or missing media) is not cued over and over.
const RECUE_EVERY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Status {
    online: bool,
    /// Failed exchanges since the last answer.
    failures: u32,
    /// A command was not confirmed, so the follower may have missed it and
    /// is brought up to date on its next answer.
    missed_command: bool,
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

    /// The follower answered. Returns whether it just came online, and
    /// whether it may have missed a command since its last answer.
    fn answered(&mut self) -> (bool, bool) {
        self.failures = 0;
        (
            !std::mem::replace(&mut self.online, true),
            std::mem::take(&mut self.missed_command),
        )
    }

    /// An exchange failed. Clock readings are kept through a short gap; only
    /// a follower that stops answering is offline. A command other than a
    /// status poll may not have arrived.
    fn failed(&mut self, polling: bool) {
        self.failures += 1;
        self.missed_command |= !polling;
        if self.failures >= OFFLINE_AFTER_FAILURES {
            *self = Status::default();
        }
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

    /// Sends a changed show to every follower with its own projectors and
    /// address. Passing on this Player's LoadProject as it is would make
    /// every follower draw this PC's projectors instead of its own.
    fn send_project(&self, project: &ShowProject) {
        for follower in self.followers.lock().unwrap().iter() {
            let _ = follower.tx.send(load_for(project, &follower.address));
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
        let mut last_recue: Option<Instant> = None;
        thread::spawn(move || loop {
            let command = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => Command::GetState,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let polling = matches!(command, Command::GetState);
            let releasing = matches!(command, Command::ReleaseLoop);
            let sent_ms = unix_time_ms() as f64;
            let started = Instant::now();
            let result = exchange(&target, command);
            let (came_online, missed_command) = {
                let mut status = worker_status.lock().unwrap();
                match &result {
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
                        status.answered()
                    }
                    Err(_) => {
                        status.failed(polling);
                        (false, false)
                    }
                }
            };
            let mut latest = result.ok();
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
                        latest = Some(state);
                    }
                }
            }
            let Some(state) = latest else {
                continue;
            };
            let offset = worker_status.lock().unwrap().clock_offset_ms.unwrap_or(0.0);
            let recue = !came_online
                && !missed_command
                && last_recue.is_none_or(|t| t.elapsed() >= RECUE_EVERY)
                && lost_step(&shared.lock().unwrap(), &state, offset);
            if recue {
                last_recue = Some(Instant::now());
            }
            let commands = if came_online || missed_command || recue {
                catch_up(&shared, &target, offset, &state)
            } else if releasing {
                after_release(&shared, &state, offset)
            } else {
                Vec::new()
            };
            for command in commands {
                let _ = exchange(&target, command);
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

/// The show as the follower at `address` takes it: with only its own
/// projectors, and its own address.
fn load_for(project: &ShowProject, address: &str) -> Command {
    let outputs = project
        .outputs
        .iter()
        .filter(|o| o.player_address() == address)
        .map(|o| o.id)
        .collect();
    Command::LoadProject {
        project: project.clone(),
        outputs: Some(outputs),
        player: Some(address.to_string()),
    }
}

/// How far ahead of this Player, in seconds, a follower was when it gave
/// `state`. `None` unless both are playing the same scene.
fn follower_drift(runtime: &Runtime, state: &PlayerState, offset_ms: f64) -> Option<f64> {
    let playing = |transport: &Transport| *transport == Transport::Playing;
    if !playing(&runtime.state.transport)
        || runtime.scheduled.is_some()
        || !playing(&state.transport)
        || state.scheduled_start_unix_ms.is_some()
        || state.server_time_unix_ms == 0
        || state.scene_id.is_none()
        || state.scene_id != runtime.state.scene_id
    {
        return None;
    }
    // When the follower answered, on our clock, and how long ago that was.
    let answered_ms = state.server_time_unix_ms as f64 - offset_ms;
    let since = ((unix_time_ms() as f64 - answered_ms) / 1000.0).max(0.0);
    Some(state.position_seconds + since - runtime.display_position())
}

/// Whether a follower that answered with `state` has fallen out of the show
/// while this Player plays: stopped, on another scene, or well off time.
/// Followers waiting for a scheduled start are left alone.
fn lost_step(runtime: &Runtime, state: &PlayerState, offset_ms: f64) -> bool {
    if runtime.state.transport != Transport::Playing
        || runtime.scheduled.is_some()
        || runtime.state.scene_id.is_none()
        || state.scheduled_start_unix_ms.is_some()
    {
        return false;
    }
    // A cue lands a lead from now; if the master jumps (scene or loop end)
    // before then, the follower would be placed past the jump and wrap on
    // its own. Wait until the jump has passed.
    let lead = LEAD_MS as f64 / 1000.0 + 0.3;
    let position = runtime.clock.position();
    if runtime.next_jump().is_some_and(|(_, _, at)| at > 0.0 && position + lead >= at) {
        return false;
    }
    if state.transport != Transport::Playing || state.scene_id != runtime.state.scene_id {
        return true;
    }
    follower_drift(runtime, state, offset_ms).is_some_and(|drift| drift.abs() >= IN_STEP_SECONDS)
}

/// Whether a follower placed at `seconds` should let go of the loops there:
/// this Player has let go of every one of them.
fn released_at(runtime: &Runtime, seconds: f64) -> bool {
    let Some(scene) = runtime.scene() else {
        return false;
    };
    let mut inside = scene
        .loops
        .iter()
        .filter(|l| l.contains(seconds))
        .peekable();
    inside.peek().is_some() && inside.all(|l| runtime.released.contains(&l.id))
}

/// Starts a follower in this Player's playing scene at the same moment, a
/// cue lead from now.
fn cue_in_step(runtime: &Runtime, scene_id: Uuid, offset_ms: f64) -> Vec<Command> {
    let seconds = runtime.display_position() + LEAD_MS as f64 / 1000.0;
    let start = unix_time_ms() + LEAD_MS;
    let mut commands = vec![Command::CueAt {
        scene_id,
        seconds,
        start_time_unix_ms: start.saturating_add_signed(offset_ms.round() as i64),
    }];
    // A cue arms every loop again, so pass on a loop this Player let go of.
    if released_at(runtime, seconds) {
        commands.push(Command::ReleaseLoop);
    }
    commands
}

/// After an exit-loop press reached a follower. Each PC lets go of the loop
/// it is in when the press arrives, so a press near the loop end can find
/// this Player still before the end and the follower already jumped back
/// (or the other way round), leaving them a whole loop apart for the rest of
/// the scene. A follower that is out of step is started again at this
/// Player's position, with the loop let go there too.
fn after_release(shared: &Mutex<Runtime>, state: &PlayerState, offset_ms: f64) -> Vec<Command> {
    let runtime = shared.lock().unwrap();
    let out_of_step = follower_drift(&runtime, state, offset_ms)
        .is_some_and(|drift| drift.abs() >= IN_STEP_SECONDS);
    match runtime.state.scene_id {
        Some(scene_id) if out_of_step => cue_in_step(&runtime, scene_id, offset_ms),
        _ => Vec::new(),
    }
}

/// Commands that make a follower that just answered match this Player: the
/// current show, output settings, and the scene at the same moment. A
/// follower already playing the scene in step is left playing rather than
/// cued again, which would hold its picture for the cue lead.
fn catch_up(
    shared: &Mutex<Runtime>,
    address: &str,
    offset_ms: f64,
    follower: &PlayerState,
) -> Vec<Command> {
    let runtime = shared.lock().unwrap();
    let Some(project) = runtime.project.as_ref() else {
        return Vec::new();
    };
    let at_follower = |ms: u64| ms.saturating_add_signed(offset_ms.round() as i64);
    let in_step = follower_drift(&runtime, follower, offset_ms)
        .is_some_and(|drift| drift.abs() < IN_STEP_SECONDS);
    let state = &runtime.state;
    let mut commands = vec![
        load_for(project, address),
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
        (Transport::Playing, Some(_), None) if in_step => {
            // It may have missed an exit-loop press while it didn't answer.
            if released_at(&runtime, runtime.display_position()) {
                commands.push(Command::ReleaseLoop);
            }
        }
        (Transport::Playing, Some(scene_id), None) => {
            commands.extend(cue_in_step(&runtime, scene_id, offset_ms));
        }
        (Transport::Paused | Transport::Ready, Some(scene_id), None) => {
            let seconds = runtime.clock.position();
            commands.push(Command::Prepare { scene_id });
            commands.push(Command::Seek { seconds });
            if released_at(&runtime, seconds) {
                commands.push(Command::ReleaseLoop);
            }
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
            match &command {
                Command::LoadProject { project, .. } => relay.send_project(project),
                _ => relay.send(&command),
            }
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

    #[test]
    fn one_failed_poll_does_not_take_a_follower_offline() {
        let mut status = Status::default();
        assert_eq!(status.answered(), (true, false));
        status.record(5.0, 2.0);
        status.failed(true);
        status.failed(true);
        assert!(status.online);
        assert_eq!(status.clock_offset_ms, Some(5.0));
        // Answering again is not "coming online", so no re-cue.
        assert_eq!(status.answered(), (false, false));
        for _ in 0..OFFLINE_AFTER_FAILURES {
            status.failed(true);
        }
        assert!(!status.online);
        assert_eq!(status.clock_offset_ms, None);
        assert_eq!(status.answered(), (true, false));
    }

    #[test]
    fn a_failed_command_brings_the_follower_up_to_date_on_its_next_answer() {
        let mut status = Status::default();
        status.answered();
        status.failed(false);
        assert_eq!(status.answered(), (false, true));
        assert_eq!(status.answered(), (false, false));
    }

    #[test]
    fn each_follower_gets_its_own_projectors() {
        let mut project = ShowProject::default();
        project.outputs[1].player = "192.168.1.9".into();
        let Command::LoadProject {
            outputs, player, ..
        } = load_for(&project, "192.168.1.9:4777")
        else {
            panic!("not a show");
        };
        assert_eq!(outputs, Some(vec![project.outputs[1].id]));
        assert_eq!(player.as_deref(), Some("192.168.1.9:4777"));
    }

    /// A master playing its first scene at `position`, inside a 0-10 s loop.
    fn master_at(position: f64) -> (Runtime, Uuid) {
        let mut project = ShowProject::default();
        let loop_id = Uuid::new_v4();
        project.scenes[0].loops.push(mapforge_core::LoopRegion {
            id: loop_id,
            name: "Idle".into(),
            start: 0.0,
            end: 10.0,
            count: 0,
            hotkey: String::new(),
            exit_hotkey: String::new(),
            button: Default::default(),
        });
        let mut runtime = Runtime {
            project: Some(project),
            ..Default::default()
        };
        runtime.state.scene_id = runtime.project.as_ref().map(|p| p.scenes[0].id);
        runtime.state.transport = Transport::Playing;
        runtime.clock = crate::Clock {
            base: position,
            since: Some(Instant::now()),
        };
        runtime.prev_position = position;
        (runtime, loop_id)
    }

    fn follower_at(runtime: &Runtime, position: f64) -> PlayerState {
        let mut state = runtime.state.clone();
        state.position_seconds = position;
        state.server_time_unix_ms = unix_time_ms();
        state
    }

    #[test]
    fn drift_is_measured_on_the_masters_clock() {
        let (runtime, _) = master_at(9.99);
        let drift = follower_drift(&runtime, &follower_at(&runtime, 9.99), 0.0).unwrap();
        assert!(drift.abs() < 0.05);
        // The follower jumped back to the loop start; the master did not.
        let drift = follower_drift(&runtime, &follower_at(&runtime, 0.01), 0.0).unwrap();
        assert!((drift + 9.98).abs() < 0.05);
        // A follower whose wall clock runs 2 s ahead of the master's.
        let mut ahead = follower_at(&runtime, 7.99);
        ahead.server_time_unix_ms += 2000;
        assert!(follower_drift(&runtime, &ahead, 2000.0).unwrap() < -1.9);
        let mut stopped = follower_at(&runtime, 9.99);
        stopped.transport = Transport::Stopped;
        assert!(follower_drift(&runtime, &stopped, 0.0).is_none());
    }

    /// A sub restarted mid-show answers again before it counts as offline,
    /// but stopped: the master must notice and bring it back.
    #[test]
    fn a_stopped_or_lost_follower_is_brought_back() {
        let (runtime, _) = master_at(5.0);
        assert!(!lost_step(&runtime, &follower_at(&runtime, 5.0), 0.0));
        let mut stopped = follower_at(&runtime, 0.0);
        stopped.transport = Transport::Stopped;
        stopped.scene_id = None;
        assert!(lost_step(&runtime, &stopped, 0.0));
        assert!(lost_step(&runtime, &follower_at(&runtime, 3.0), 0.0), "2 s behind");
        // Already cued and waiting for its start: leave it alone.
        let mut waiting = stopped.clone();
        waiting.scheduled_start_unix_ms = Some(unix_time_ms() + 500);
        assert!(!lost_step(&runtime, &waiting, 0.0));
        // A few frames off is corrected too.
        assert!(lost_step(&runtime, &follower_at(&runtime, 4.9), 0.0), "100 ms behind");
        // Not just before the master's loop end at 10 s: the cue would land past it.
        let (near_end, _) = master_at(9.5);
        assert!(!lost_step(&near_end, &stopped, 0.0));
        // Nothing to bring it back to while the master is stopped.
        let mut idle = runtime;
        idle.state.transport = Transport::Stopped;
        assert!(!lost_step(&idle, &stopped, 0.0));
    }

    #[test]
    fn exit_loop_restarts_a_follower_left_a_loop_behind() {
        let (mut runtime, loop_id) = master_at(9.0);
        runtime.released.insert(loop_id);
        let scene_id = runtime.state.scene_id.unwrap();
        let shared = Mutex::new(runtime);
        let follower = follower_at(&shared.lock().unwrap(), 9.0);
        assert!(after_release(&shared, &follower, 0.0).is_empty());
        // The follower had already jumped back to the loop start.
        let follower = follower_at(&shared.lock().unwrap(), 0.02);
        let commands = after_release(&shared, &follower, 0.0);
        assert!(matches!(
            commands[0],
            Command::CueAt { scene_id: id, seconds, .. }
                if id == scene_id && (seconds - 9.8).abs() < 0.05
        ));
        assert!(matches!(commands[1], Command::ReleaseLoop));
    }

    #[test]
    fn a_follower_still_in_step_is_not_cued_again() {
        let (runtime, _) = master_at(4.0);
        let follower = follower_at(&runtime, 4.0);
        let shared = Mutex::new(runtime);
        let commands = catch_up(&shared, "192.168.1.9:4777", 0.0, &follower);
        assert!(!commands.iter().any(|c| matches!(c, Command::CueAt { .. })));
        let mut stopped = follower.clone();
        stopped.transport = Transport::Stopped;
        let commands = catch_up(&shared, "192.168.1.9:4777", 0.0, &stopped);
        assert!(commands.iter().any(|c| matches!(c, Command::CueAt { .. })));
    }
}
