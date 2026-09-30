use eframe::egui;
use mapforge_core::{Command, Envelope, PlayerState, ShowProject, Transport, PROTOCOL_VERSION};
use std::{
    collections::{HashSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use uuid::Uuid;

const CONTROLLER_HTML: &str = include_str!("controller.html");

#[derive(Default)]
struct Runtime {
    state: PlayerState,
    project: Option<ShowProject>,
    seen: HashSet<Uuid>,
    order: VecDeque<Uuid>,
}

fn apply(runtime: &mut Runtime, envelope: Envelope) {
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
        Command::LoadProject { project } => match project.validate() {
            Ok(()) => {
                runtime.project = Some(project);
                runtime.state.message = "Project loaded".into();
            }
            Err(e) => runtime.state.message = format!("Rejected project: {e}"),
        },
        Command::Prepare { scene_id } => {
            runtime.state.scene_id = Some(scene_id);
            runtime.state.transport = Transport::Ready;
            runtime.state.message = "Scene ready".into();
        }
        Command::Play => {
            runtime.state.transport = Transport::Playing;
            runtime.state.message = "Playing".into();
        }
        Command::Pause => {
            runtime.state.transport = Transport::Paused;
            runtime.state.message = "Paused".into();
        }
        Command::Stop => {
            runtime.state.transport = Transport::Stopped;
            runtime.state.message = "Stopped".into();
        }
        Command::SetVolume { value } => runtime.state.volume = value.clamp(0.0, 1.0),
        Command::SetMute { value } => runtime.state.muted = value,
        Command::SetBlackout { value } => runtime.state.blackout = value,
        Command::GetState => {}
    }
}

fn handle_protocol(mut stream: TcpStream, shared: Arc<Mutex<Runtime>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_ok() && line.len() <= 8 * 1024 * 1024 {
        if let Ok(envelope) = serde_json::from_str::<Envelope>(&line) {
            apply(&mut shared.lock().unwrap(), envelope);
        }
    }
    if let Ok(json) = serde_json::to_string(&shared.lock().unwrap().state) {
        let _ = writeln!(stream, "{json}");
    }
}

fn protocol_server(shared: Arc<Mutex<Runtime>>) {
    let listener = TcpListener::bind("0.0.0.0:4777").expect("TCP port 4777 is unavailable");
    for stream in listener.incoming().flatten() {
        let state = shared.clone();
        thread::spawn(move || handle_protocol(stream, state));
    }
}

fn http_response(mut stream: TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = stream.write_all(response.as_bytes());
}

fn http_server(shared: Arc<Mutex<Runtime>>) {
    let listener = TcpListener::bind("0.0.0.0:8080").expect("HTTP port 8080 is unavailable");
    for mut stream in listener.incoming().flatten() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0_u8; 16 * 1024];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let first = request.lines().next().unwrap_or("");
        let parts: Vec<_> = first.split_whitespace().collect();
        if parts.len() < 2 {
            http_response(stream, "400 Bad Request", "text/plain", "Bad request");
            continue;
        }
        let (method, path) = (parts[0], parts[1]);
        if method == "GET" && path == "/" {
            http_response(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                CONTROLLER_HTML,
            );
            continue;
        }
        if method == "GET" && path == "/api/state" {
            let json = serde_json::to_string(&shared.lock().unwrap().state).unwrap();
            http_response(stream, "200 OK", "application/json", &json);
            continue;
        }
        if method == "POST" && path.starts_with("/api/") {
            let command = match path {
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
                apply(
                    &mut shared.lock().unwrap(),
                    Envelope {
                        protocol_version: PROTOCOL_VERSION,
                        command_id: Uuid::new_v4(),
                        command,
                    },
                );
                http_response(stream, "204 No Content", "text/plain", "");
            } else {
                http_response(stream, "404 Not Found", "text/plain", "Unknown control");
            }
            continue;
        }
        http_response(stream, "404 Not Found", "text/plain", "Not found");
    }
}

struct PlayerApp {
    shared: Arc<Mutex<Runtime>>,
    show_outputs: bool,
}

impl PlayerApp {
    fn output(&self, ctx: &egui::Context, index: usize) {
        let id = egui::ViewportId::from_hash_of(("output", index));
        let title = format!("MapForge Simulated Projector {}", index + 1);
        ctx.show_viewport_immediate(
            id,
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([640.0, 360.0]),
            move |ctx, _| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                    .show(ctx, |ui| {
                        let runtime = self.shared.lock().unwrap();
                        if runtime.state.blackout {
                            ui.centered_and_justified(|ui| {
                                ui.colored_label(egui::Color32::DARK_GRAY, "BLACKOUT");
                            });
                            return;
                        }
                        let Some(project) = &runtime.project else {
                            ui.centered_and_justified(|ui| {
                                ui.label("Waiting for project");
                            });
                            return;
                        };
                        let Some(output) = project.outputs.get(index) else {
                            return;
                        };
                        let Some(scene_id) = runtime.state.scene_id else {
                            return;
                        };
                        let Some(scene) = project.scenes.iter().find(|s| s.id == scene_id) else {
                            return;
                        };
                        let rect = ui.max_rect();
                        for (i, layer) in scene
                            .layers
                            .iter()
                            .enumerate()
                            .filter(|(_, l)| l.output_ids.contains(&output.id))
                        {
                            let x = rect.left()
                                + (layer.x - output.stage_x) / output.stage_width * rect.width();
                            let y = rect.top()
                                + (layer.y - output.stage_y) / output.stage_height * rect.height();
                            let w = layer.width / output.stage_width * rect.width();
                            let h = layer.height / output.stage_height * rect.height();
                            let color = [
                                egui::Color32::from_rgb(38, 132, 255),
                                egui::Color32::from_rgb(187, 79, 255),
                                egui::Color32::from_rgb(30, 190, 130),
                            ][i % 3];
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h)),
                                3.0,
                                color.gamma_multiply(layer.opacity),
                            );
                            ui.painter().text(
                                egui::pos2(x + 8.0, y + 8.0),
                                egui::Align2::LEFT_TOP,
                                &layer.name,
                                egui::FontId::proportional(18.0),
                                egui::Color32::WHITE,
                            );
                        }
                    });
            },
        );
    }
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("MapForge Player");
            ui.label("Producer: TCP 4777   Controller: HTTP 8080");
            let mut runtime = self.shared.lock().unwrap();
            ui.separator();
            ui.label(format!(
                "Status: {:?} — {}",
                runtime.state.transport, runtime.state.message
            ));
            ui.horizontal(|ui| {
                ui.label("Volume");
                ui.add(egui::Slider::new(&mut runtime.state.volume, 0.0..=1.0));
                ui.checkbox(&mut runtime.state.muted, "Mute");
                ui.checkbox(&mut runtime.state.blackout, "Blackout");
            });
            drop(runtime);
            ui.checkbox(&mut self.show_outputs, "Open simulated projector windows");
            ui.label("Closing Producer or the controller does not stop this Player.");
        });
        if self.show_outputs {
            self.output(ctx, 0);
            self.output(ctx, 1);
        }
    }
}

fn main() -> eframe::Result<()> {
    let shared = Arc::new(Mutex::new(Runtime::default()));
    {
        let s = shared.clone();
        thread::spawn(move || protocol_server(s));
    }
    {
        let s = shared.clone();
        thread::spawn(move || http_server(s));
    }
    eframe::run_native(
        "MapForge Player",
        eframe::NativeOptions::default(),
        Box::new(|_| {
            Ok(Box::new(PlayerApp {
                shared,
                show_outputs: true,
            }))
        }),
    )
}
