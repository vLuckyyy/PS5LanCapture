pub mod help;
pub mod main_view;
pub mod pad;
pub mod settings;
pub mod theme;
pub mod update;
pub mod widgets;
pub mod wizard;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::audio::{self, AudioOutput, OutputDevice};
use crate::config::{AppConfig, ConsoleEntry, ConsoleSecret, Paths};
use crate::controller::ControllerManager;
use crate::native::{DiscoveredHost, Discovery, HostState};
use crate::session::{ConnectTarget, StreamController, StreamState};

pub struct VideoView {
    pub texture_id: egui::TextureId,
    pub size: [u32; 2],
    pub source: [u32; 2],
    pub scaler: Option<crate::upscale::ScalerKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRequest {
    ToggleFullscreen,
    ExitFullscreen,
    Quit,
}

pub struct DiscoveryHub {
    broadcast: Option<Discovery>,
    unicast: HashMap<String, Discovery>,
    last_retry: Instant,
}

impl DiscoveryHub {
    fn new() -> Self {
        Self {
            broadcast: None,
            unicast: HashMap::new(),
            last_retry: Instant::now() - Duration::from_secs(60),
        }
    }

    pub fn ensure(&mut self, addresses: &[String]) {
        if self.last_retry.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_retry = Instant::now();
        if self.broadcast.is_none() {
            self.broadcast = Discovery::start(None);
            if self.broadcast.is_none() {
                log::warn!("Cannot start discovery (broadcast)");
            }
        }
        self.unicast.retain(|addr, _| addresses.contains(addr));
        for addr in addresses {
            if !addr.is_empty()
                && !self.unicast.contains_key(addr)
                && let Some(d) = Discovery::start(Some(addr))
            {
                self.unicast.insert(addr.clone(), d);
            }
        }
    }

    pub fn hosts(&self) -> Vec<DiscoveredHost> {
        let mut out: Vec<DiscoveredHost> = Vec::new();
        let all = self.broadcast.iter().chain(self.unicast.values());
        for discovery in all {
            for host in discovery.hosts() {
                if !out
                    .iter()
                    .any(|h| h.host_id == host.host_id && !host.host_id.is_empty())
                {
                    out.push(host);
                }
            }
        }
        out
    }

    pub fn restart(&mut self) {
        self.broadcast = None;
        self.unicast.clear();
        self.last_retry = Instant::now() - Duration::from_secs(60);
    }
}

pub struct App {
    pub paths: Paths,
    pub config: AppConfig,
    pub audio: AudioOutput,
    pub controller: ControllerManager,
    pub updater: crate::updater::Updater,
    pub update_open: bool,
    pub stream: StreamController,
    pub discovery: DiscoveryHub,
    pub hosts: Vec<DiscoveredHost>,
    pub manual_addresses: Vec<String>,
    pub wizard: Option<wizard::Wizard>,
    pub settings_open: bool,
    pub discord_help_open: bool,
    pub logs_open: bool,
    pub about_open: bool,
    pub fullscreen: bool,
    pub native_version: String,
    pub login_pin: String,
    pub notice: Option<(String, Instant)>,
    pub audio_devices: Vec<OutputDevice>,
    pub audio_devices_refreshed: Instant,
    pub last_activity: Instant,
    pub requests: Vec<WindowRequest>,
    pub gpu_error: Option<String>,
    pub rtx_possible: bool,
    config_dirty: bool,
    last_save: Instant,
}

impl App {
    pub fn new(
        paths: Paths,
        config: AppConfig,
        audio: AudioOutput,
        controller: ControllerManager,
        updater: crate::updater::Updater,
        stream: StreamController,
        native_version: String,
    ) -> Self {
        let wizard = (!config.wizard_completed).then(wizard::Wizard::new);
        let mut app = Self {
            paths,
            config,
            audio,
            controller,
            updater,
            update_open: false,
            stream,
            discovery: DiscoveryHub::new(),
            hosts: Vec::new(),
            manual_addresses: Vec::new(),
            wizard,
            settings_open: false,
            discord_help_open: false,
            logs_open: false,
            about_open: false,
            fullscreen: false,
            native_version,
            login_pin: String::new(),
            notice: None,
            audio_devices: audio::list_output_devices(),
            audio_devices_refreshed: Instant::now(),
            last_activity: Instant::now(),
            requests: Vec::new(),
            gpu_error: None,
            rtx_possible: false,
            config_dirty: false,
            last_save: Instant::now(),
        };
        let reconnect = app.config.ui.auto_reconnect;
        app.stream.set_auto_reconnect(reconnect);
        app
    }

    pub fn mark_dirty(&mut self) {
        self.config_dirty = true;
    }

    pub fn save_now(&mut self) {
        if let Err(e) = self.config.save(&self.paths.config_file) {
            log::error!("Cannot save config: {e}");
            self.notify(format!("Cannot save settings: {e}"));
        }
        self.config_dirty = false;
        self.last_save = Instant::now();
    }

    pub fn notify(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now()));
    }

    pub fn refresh_audio_devices(&mut self) {
        self.audio_devices = audio::list_output_devices();
        self.audio_devices_refreshed = Instant::now();
    }

    pub fn console_state(&self, console: &ConsoleEntry) -> Option<HostState> {
        self.hosts
            .iter()
            .find(|h| h.host_id == console.host_id)
            .map(|h| h.state.clone())
    }

    pub fn selected_console(&self) -> Option<&ConsoleEntry> {
        self.config
            .last_console
            .as_deref()
            .and_then(|id| self.config.console(id))
            .or_else(|| self.config.consoles.first())
    }

    pub fn connect_selected(&mut self) {
        let Some(console) = self.selected_console().cloned() else {
            self.notify("Register a console in the setup first.");
            return;
        };
        self.connect_to(console);
    }

    pub fn connect_to(&mut self, console: ConsoleEntry) {
        let secret = match ConsoleSecret::open(&console.protected_secret) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Reading registration data: {e}");
                self.stream.state = StreamState::Failed {
                    message: "Cannot read the encrypted registration data (different Windows account?). Register the console again.".into(),
                };
                return;
            }
        };
        let standby = self.console_state(&console) == Some(HostState::Standby);
        self.config.last_console = Some(console.host_id.clone());
        self.mark_dirty();
        self.audio.clear();
        self.stream
            .connect(ConnectTarget { console, secret }, standby);
    }

    /// Per-frame housekeeping independent of rendering.
    pub fn tick(&mut self, device: *mut std::ffi::c_void) {
        let mut addresses: Vec<String> = self
            .config
            .consoles
            .iter()
            .map(|c| c.address.clone())
            .collect();
        addresses.extend(self.manual_addresses.iter().cloned());
        self.discovery.ensure(&addresses);
        self.hosts = self.discovery.hosts();

        let mut changed = false;
        for console in &mut self.config.consoles {
            if let Some(host) = self.hosts.iter().find(|h| h.host_id == console.host_id)
                && !host.addr.is_empty()
                && host.addr != console.address
            {
                log::info!(
                    "Console {} changed address: {} -> {}",
                    console.nickname,
                    console.address,
                    host.addr
                );
                console.address = host.addr.clone();
                changed = true;
            }
        }
        if changed {
            self.mark_dirty();
        }

        let ready = self.stream.target().and_then(|t| {
            self.hosts
                .iter()
                .find(|h| h.host_id == t.console.host_id)
                .map(|h| match h.state {
                    HostState::Ready => Some(true),
                    HostState::Standby => Some(false),
                    HostState::Unknown => None,
                })
        });
        let video = self.config.video.clone();
        self.stream.tick(&video, device, ready.flatten());

        if self.audio_devices_refreshed.elapsed() > Duration::from_secs(5)
            && (self.settings_open || self.wizard.is_some())
        {
            self.refresh_audio_devices();
        }
        if let Some(version) = self.updater.tick(self.config.ui.check_for_updates) {
            self.notify(format!(
                "Version {version} is available. Use the Update button in the top bar."
            ));
        }
        if self.config_dirty && self.last_save.elapsed() > Duration::from_millis(800) {
            self.save_now();
        }
        if let Some((_, at)) = &self.notice
            && at.elapsed() > Duration::from_secs(6)
        {
            self.notice = None;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, video: Option<&VideoView>) {
        if ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO || !i.events.is_empty()) {
            self.last_activity = Instant::now();
        }
        if ui.input(|i| i.key_pressed(egui::Key::F11)) {
            self.requests.push(WindowRequest::ToggleFullscreen);
        }
        if self.fullscreen && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.requests.push(WindowRequest::ExitFullscreen);
        }

        if let Some(mut wizard) = self.wizard.take() {
            let finished = wizard.ui(ui, self, video);
            if !finished {
                self.wizard = Some(wizard);
            }
        } else {
            main_view::ui(ui, self, video);
        }

        settings::window(ui.ctx(), self);
        help::discord_window(ui.ctx(), &mut self.discord_help_open);
        help::about_window(ui.ctx(), &mut self.about_open, &self.native_version);
        update::window(ui.ctx(), self);
        help::logs_window(ui.ctx(), &mut self.logs_open, &self.paths);
        self.login_pin_modal(ui.ctx());
        self.notice_toast(ui.ctx());
    }

    fn login_pin_modal(&mut self, ctx: &egui::Context) {
        let StreamState::WaitingForLoginPin { incorrect } = self.stream.state else {
            return;
        };
        egui::Modal::new(egui::Id::new("login-pin")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Console passcode");
            ui.label("Your PS5 account is protected with a passcode. Enter it to start streaming.");
            if incorrect {
                ui.colored_label(theme::ERR, "Wrong passcode, try again.");
            }
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.login_pin)
                    .password(true)
                    .hint_text("4 digits"),
            );
            edit.request_focus();
            ui.horizontal(|ui| {
                let valid =
                    self.login_pin.len() == 4 && self.login_pin.chars().all(|c| c.is_ascii_digit());
                if ui.add_enabled(valid, egui::Button::new("OK")).clicked()
                    || (valid && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    let pin = std::mem::take(&mut self.login_pin);
                    self.stream.submit_login_pin(&pin);
                }
                if ui.button("Cancel").clicked() {
                    self.login_pin.clear();
                    self.stream.disconnect();
                }
            });
        });
    }

    fn notice_toast(&mut self, ctx: &egui::Context) {
        let Some((message, _)) = &self.notice else {
            return;
        };
        let message = message.clone();
        egui::Area::new(egui::Id::new("notice"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                theme::card_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(message);
                        if ui.small_button("✕").clicked() {
                            self.notice = None;
                        }
                    });
                });
            });
    }

    pub fn wants_continuous_redraw(&self) -> bool {
        matches!(
            self.stream.state,
            StreamState::Streaming { .. } | StreamState::Connecting { .. }
        ) || self.wizard.as_ref().is_some_and(|w| w.is_busy())
    }

    pub fn shutdown(&mut self) {
        self.save_now();
        self.stream.shutdown();
    }
}
