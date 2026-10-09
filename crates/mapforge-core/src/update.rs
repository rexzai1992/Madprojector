//! Finds a newer MapForge on GitHub Releases and installs it. Only Windows
//! builds look for updates, because the update is a Windows installer.

use crate::tool_command;
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

pub const RELEASES_API: &str =
    "https://api.github.com/repos/rexzai1992/Madprojector/releases/latest";
const FIRST_CHECK: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);

/// This build's version: set by the release build, otherwise Cargo's.
pub fn current_version() -> &'static str {
    option_env!("MAPFORGE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn version_numbers(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    version_numbers(candidate) > version_numbers(current)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release's one-line summary.
    pub notes: String,
    /// The small installer that replaces only the MapForge programs.
    pub installer_url: String,
    pub installer_name: String,
}

fn parse_release(json: &str) -> Result<Release, String> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct Latest {
        tag_name: String,
        #[serde(default)]
        body: Option<String>,
        assets: Vec<Asset>,
    }
    let latest: Latest = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let asset = latest
        .assets
        .into_iter()
        .find(|a| a.name.starts_with("MapForge-Update-") && a.name.ends_with(".exe"))
        .ok_or("the release has no update installer")?;
    let notes = latest
        .body
        .unwrap_or_default()
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    Ok(Release {
        version: latest.tag_name.trim_start_matches('v').to_string(),
        notes,
        installer_url: asset.browser_download_url,
        installer_name: asset.name,
    })
}

/// Downloads with the `curl` built into Windows 10 and later.
fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = tool_command("curl")
        .args(["-fsSL", "-H", "User-Agent: MapForge"])
        .args(args)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn fetch_latest() -> Result<Release, String> {
    let body = curl(&[
        "--max-time",
        "20",
        "-H",
        "Accept: application/vnd.github+json",
        RELEASES_API,
    ])?;
    parse_release(&String::from_utf8_lossy(&body))
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    /// Up to date, not checked yet, or offline.
    Idle,
    /// "Check for updates" was pressed and the answer hasn't come yet.
    Checking,
    /// The answer to "Check for updates" when nothing newer exists.
    UpToDate,
    Available(Release),
    Downloading(Release),
    /// The installer is open; the app should close so it can be replaced.
    Installing,
    Failed(String),
}

/// Checks for updates in the background and installs one on request.
pub struct Updater {
    state: Arc<Mutex<UpdateState>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    /// The version the operator chose "Later" for.
    dismissed: Option<String>,
}

impl Updater {
    pub fn start(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let updater = Self {
            state: Arc::new(Mutex::new(UpdateState::Idle)),
            repaint: Arc::new(repaint),
            dismissed: None,
        };
        let enabled = cfg!(windows) && std::env::var_os("MAPFORGE_NO_UPDATE_CHECK").is_none();
        if enabled {
            let state = updater.state.clone();
            let repaint = updater.repaint.clone();
            thread::spawn(move || {
                thread::sleep(FIRST_CHECK);
                loop {
                    // Offline show networks simply stay quiet.
                    if let Ok(release) = fetch_latest() {
                        let mut state = state.lock().unwrap();
                        let busy = matches!(
                            *state,
                            UpdateState::Downloading(_) | UpdateState::Installing
                        );
                        if !busy && is_newer(&release.version, current_version()) {
                            *state = UpdateState::Available(release);
                            repaint();
                        }
                    }
                    thread::sleep(CHECK_EVERY);
                }
            });
        }
        updater
    }

    /// What to show; an update put off with "Later" is hidden.
    pub fn state(&self) -> UpdateState {
        let state = self.state.lock().unwrap().clone();
        match &state {
            UpdateState::Available(r) if self.dismissed.as_ref() == Some(&r.version) => {
                UpdateState::Idle
            }
            _ => state,
        }
    }

    pub fn later(&mut self) {
        let state = self.state.lock().unwrap().clone();
        match state {
            UpdateState::Available(release) => self.dismissed = Some(release.version),
            UpdateState::Failed(_) | UpdateState::UpToDate => {
                *self.state.lock().unwrap() = UpdateState::Idle
            }
            _ => {}
        }
    }

    /// Asks GitHub now and always answers, even when up to date.
    pub fn check_now(&mut self) {
        {
            let mut state = self.state.lock().unwrap();
            if matches!(
                *state,
                UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Installing
            ) {
                return;
            }
            *state = UpdateState::Checking;
        }
        self.dismissed = None;
        let state = self.state.clone();
        let repaint = self.repaint.clone();
        thread::spawn(move || {
            let answer = match fetch_latest() {
                Ok(release) if is_newer(&release.version, current_version()) => {
                    UpdateState::Available(release)
                }
                Ok(_) => UpdateState::UpToDate,
                Err(error) => UpdateState::Failed(format!("Can't reach GitHub: {error}")),
            };
            *state.lock().unwrap() = answer;
            repaint();
        });
    }

    /// Downloads the update installer and opens it.
    pub fn install(&self) {
        let UpdateState::Available(release) = self.state() else {
            return;
        };
        *self.state.lock().unwrap() = UpdateState::Downloading(release.clone());
        let state = self.state.clone();
        let repaint = self.repaint.clone();
        thread::spawn(move || {
            let path = std::env::temp_dir().join(&release.installer_name);
            let result = download(&release, &path).and_then(|()| open_installer(&path));
            *state.lock().unwrap() = match result {
                Ok(()) => UpdateState::Installing,
                Err(error) => UpdateState::Failed(error),
            };
            repaint();
        });
    }
}

fn download(release: &Release, path: &PathBuf) -> Result<(), String> {
    let target = path.to_string_lossy().to_string();
    curl(&["--max-time", "1800", "-o", &target, &release.installer_url]).map(|_| ())
}

/// Opens the installer the way Explorer would, so Windows asks for
/// administrator permission.
#[cfg(windows)]
fn open_installer(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let verb = wide("open".as_ref());
    let file = wide(path.as_os_str());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success with a value above 32.
    if result as isize > 32 {
        Ok(())
    } else {
        Err("Windows did not open the installer".into())
    }
}

#[cfg(not(windows))]
fn open_installer(_: &std::path::Path) -> Result<(), String> {
    Err("updates install on Windows only".into())
}

#[cfg(feature = "ui")]
pub mod bubble {
    use super::{current_version, UpdateState, Updater};
    use egui::{Align2, Color32, RichText};

    /// The update bubble in the corner of the window. `blocked` explains why
    /// updating has to wait (e.g. unsaved work). Returns true when the app
    /// should close so the installer can replace it.
    pub fn show(ctx: &egui::Context, updater: &mut Updater, blocked: Option<&str>) -> bool {
        let state = updater.state();
        if state == UpdateState::Idle {
            return false;
        }
        if state == UpdateState::Installing {
            return true;
        }
        let mut install = false;
        let mut later = false;
        egui::Area::new(egui::Id::new("mapforge_update_bubble"))
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -40.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgb(24, 32, 52))
                    .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(66, 126, 255)))
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_max_width(320.0);
                        match &state {
                            UpdateState::Available(release) => {
                                ui.label(RichText::new("⬆ Update available").strong().size(16.0));
                                ui.label(format!(
                                    "MapForge {} is ready. This PC has {}.",
                                    release.version,
                                    current_version()
                                ));
                                if !release.notes.is_empty() {
                                    ui.label(RichText::new(&release.notes).small().weak());
                                }
                                if let Some(reason) = blocked {
                                    ui.colored_label(Color32::from_rgb(240, 170, 40), reason);
                                }
                                ui.horizontal(|ui| {
                                    install = ui
                                        .add_enabled(
                                            blocked.is_none(),
                                            egui::Button::new(RichText::new("Update now").strong())
                                                .fill(Color32::from_rgb(36, 107, 254)),
                                        )
                                        .clicked();
                                    later = ui.button("Later").clicked();
                                });
                            }
                            UpdateState::Downloading(release) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(format!("Downloading MapForge {}…", release.version));
                                });
                            }
                            UpdateState::Checking => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Checking for updates…");
                                });
                            }
                            UpdateState::UpToDate => {
                                ui.label(RichText::new("✔ Up to date").strong());
                                ui.label(format!(
                                    "MapForge {} is the newest version.",
                                    current_version()
                                ));
                                later = ui.button("Close").clicked();
                            }
                            UpdateState::Failed(error) => {
                                ui.label(RichText::new("Update failed").strong());
                                ui.label(RichText::new(error).small());
                                later = ui.button("Close").clicked();
                            }
                            UpdateState::Idle | UpdateState::Installing => {}
                        }
                    });
            });
        if install {
            updater.install();
        }
        if later {
            updater.later();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_number() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("v0.2.0", "0.1.99"));
        assert!(!is_newer("0.1.5", "0.1.5"));
        assert!(!is_newer("0.1.4", "0.1.5"));
    }

    #[test]
    fn reads_the_update_installer_from_a_release() {
        let json = r#"{
            "tag_name": "v0.1.12",
            "body": "Fill each projector with real fullscreen\n\nCo-Authored-By: x",
            "assets": [
                {"name": "MapForge-Setup-0.1.12.exe", "browser_download_url": "https://x/setup"},
                {"name": "MapForge-Update-0.1.12.exe", "browser_download_url": "https://x/update"}
            ]
        }"#;
        let release = parse_release(json).unwrap();
        assert_eq!(release.version, "0.1.12");
        assert_eq!(release.installer_url, "https://x/update");
        assert_eq!(release.notes, "Fill each projector with real fullscreen");
    }
}
