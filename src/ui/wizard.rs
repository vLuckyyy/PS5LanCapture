use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

use egui::{RichText, Ui};

use super::widgets::{
    self, info_box, numbered_steps, primary_button, primary_button_enabled, yes_no,
};
use super::{App, VideoView, theme};
use crate::config::{ConsoleEntry, ConsoleSecret};
use crate::native::{self, DiscoveredHost, HostState, Regist, RegistStatus};
use crate::psn;
use crate::session::StreamState;

const STEP_LABELS: [&str; 7] = [
    "Start",
    "PS5",
    "Console",
    "Register",
    "Controller",
    "Audio",
    "Done",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Welcome = 0,
    ConfigurePs5 = 1,
    FindConsole = 2,
    Register = 3,
    Controller = 4,
    Audio = 5,
    Finish = 6,
}

impl Step {
    fn next(self) -> Step {
        match self {
            Step::Welcome => Step::ConfigurePs5,
            Step::ConfigurePs5 => Step::FindConsole,
            Step::FindConsole => Step::Register,
            Step::Register => Step::Controller,
            Step::Controller => Step::Audio,
            Step::Audio | Step::Finish => Step::Finish,
        }
    }

    fn previous(self) -> Step {
        match self {
            Step::Welcome | Step::ConfigurePs5 => Step::Welcome,
            Step::FindConsole => Step::ConfigurePs5,
            Step::Register => Step::FindConsole,
            Step::Controller => Step::Register,
            Step::Audio => Step::Controller,
            Step::Finish => Step::Audio,
        }
    }
}

pub struct Wizard {
    pub step: Step,
    checklist: [bool; 4],
    manual_ip: String,
    selected: Option<DiscoveredHost>,
    search_started: Instant,
    redirect_input: String,
    account_input: String,
    psn_job: Option<Receiver<Result<[u8; 8], String>>>,
    account_error: Option<String>,
    change_account: bool,
    pin: String,
    regist: Option<Regist>,
    regist_started: Option<Instant>,
    regist_error: Option<String>,
    registered: Option<ConsoleEntry>,
    local_ip: Option<String>,
}

impl Wizard {
    pub fn new() -> Self {
        Self {
            step: Step::Welcome,
            checklist: [false; 4],
            manual_ip: String::new(),
            selected: None,
            search_started: Instant::now(),
            redirect_input: String::new(),
            account_input: String::new(),
            psn_job: None,
            account_error: None,
            change_account: false,
            pin: String::new(),
            regist: None,
            regist_started: None,
            regist_error: None,
            registered: None,
            local_ip: local_ipv4(),
        }
    }

    pub fn starting_at(step: Step) -> Self {
        let mut wizard = Self::new();
        wizard.step = step;
        if step as u8 >= Step::FindConsole as u8 {
            wizard.checklist = [true; 4];
        }
        wizard
    }

    pub fn is_busy(&self) -> bool {
        self.psn_job.is_some() || self.regist.is_some()
    }

    /// Returns true when the wizard finished.
    pub fn ui(&mut self, ui: &mut Ui, app: &mut App, video: Option<&VideoView>) -> bool {
        let mut finished = false;
        egui::Panel::bottom("wizard-footer")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.step != Step::Welcome && ui.button("◀ Back").clicked() {
                        self.step = self.step.previous();
                    }
                    if app.config.wizard_completed && ui.button("Close setup").clicked() {
                        finished = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.step == Step::Finish {
                            if primary_button(ui, "Start ▶").clicked() {
                                finished = self.finish(app);
                            }
                        } else {
                            let (can_continue, hint) = self.can_continue(app);
                            if primary_button_enabled(ui, can_continue, "Next ▶").clicked() {
                                self.on_next(app);
                            }
                            if let Some(hint) = hint {
                                ui.label(RichText::new(hint).color(theme::MUTED).small());
                            }
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(24, 16)),
            )
            .show(ui, |ui| {
                widgets::step_indicator(ui, self.step as usize, &STEP_LABELS);
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.set_max_width(820.0);
                            ui.with_layout(
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| match self.step {
                                    Step::Welcome => self.welcome(ui),
                                    Step::ConfigurePs5 => self.configure_ps5(ui),
                                    Step::FindConsole => self.find_console(ui, app),
                                    Step::Register => self.register(ui, app),
                                    Step::Controller => self.controller(ui, app, video),
                                    Step::Audio => audio_step(ui, app),
                                    Step::Finish => self.finish_view(ui, app),
                                },
                            );
                        });
                    });
            });
        finished
    }

    fn can_continue(&self, app: &App) -> (bool, Option<&'static str>) {
        match self.step {
            Step::ConfigurePs5 if !self.checklist[..3].iter().all(|c| *c) => {
                (false, Some("Tick the first three items"))
            }
            Step::FindConsole if self.selected.is_none() => {
                (false, Some("Pick a console from the list"))
            }
            Step::Register if self.registered.is_none() => {
                (false, Some("Register the console first"))
            }
            Step::Controller => {
                let c = &app.config.controller_check;
                let answered = [
                    c.pad_controls_game,
                    c.monitor_shows_game,
                    c.app_shows_video,
                    c.pc_plays_audio,
                ]
                .iter()
                .all(Option::is_some);
                if answered {
                    (true, None)
                } else {
                    (false, Some("Answer all questions or skip the test"))
                }
            }
            _ => (true, None),
        }
    }

    fn on_next(&mut self, app: &mut App) {
        if self.step == Step::FindConsole
            && let Some(selected) = &self.selected
        {
            self.registered = app.config.console(&selected.host_id).cloned();
        }
        self.step = self.step.next();
        if self.step == Step::FindConsole {
            self.search_started = Instant::now();
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.add_space(12.0);
        ui.label(RichText::new("Welcome to PS5 LAN Capture!").font(theme::heading_font(32.0)));
        ui.add_space(4.0);
        ui.label(
            "This app shows your PS5 picture and sound on this PC over your home network using Remote Play. No capture card or OBS needed, the PS5 and the PC just have to be on the same router.",
        );
        ui.add_space(12.0);
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::network_diagram(ui);
        });
        ui.add_space(12.0);
        ui.label(RichText::new("How it works").strong());
        numbered_steps(
            ui,
            &[
                "You play as usual and watch your monitor connected to the PS5.",
                "The app shows the same picture here and plays the game sound in your PC headset.",
                "Share this window on Discord. Your friends see and hear the game while you talk to them on the same headset.",
            ],
        );
        ui.add_space(8.0);
        info_box(
            ui,
            theme::WARN,
            "Remote Play disconnects the controller connected to the PS5 when a session starts. You will play with a controller connected to this PC instead. The app forwards it to the console.",
        );
    }

    fn configure_ps5(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Set up your PS5").font(theme::heading_font(24.0)));
        ui.label("Do this once on the console:");
        ui.add_space(6.0);
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            numbered_steps(
                ui,
                &[
                    "Turn on your PlayStation 5.",
                    "Open Settings (gear icon, top right of the home screen).",
                    "Go to System > Remote Play.",
                    "Turn on Enable Remote Play.",
                    "Make sure the PS5 and this PC are on the same local network (same router).",
                    "For the best stability connect both with an Ethernet cable.",
                ],
            );
            ui.add_space(4.0);
            ui.label(RichText::new("").small().color(theme::MUTED));
        });
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Optional: connect while the console is in rest mode").show(
            ui,
            |ui| {
                numbered_steps(
                    ui,
                    &[
                        "Settings > System > Power Saving > Features Available in Rest Mode.",
                        "Turn on Stay Connected to the Internet.",
                        "Turn on Enable Turning On PS5 from Network.",
                    ],
                );
                ui.label(
                    RichText::new("This lets the app wake up the console before connecting.")
                        .small()
                        .color(theme::MUTED),
                );
            },
        );
        ui.add_space(10.0);
        ui.label(RichText::new("Checklist").strong());
        let items = [
            "The PS5 is on",
            "Remote Play is enabled",
            "The PS5 and this PC are on the same network",
            "I use an Ethernet cable (recommended, optional)",
        ];
        for (i, item) in items.iter().enumerate() {
            ui.checkbox(&mut self.checklist[i], *item);
        }
    }

    fn find_console(&mut self, ui: &mut Ui, app: &mut App) {
        ui.label(RichText::new("Find your console").font(theme::heading_font(24.0)));
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Searching the local network for PlayStation consoles…");
        });
        ui.add_space(6.0);

        let hosts = app.hosts.clone();
        if hosts.is_empty() {
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("No console found yet.").color(theme::MUTED));
            });
        }
        for host in &hosts {
            let selected = self
                .selected
                .as_ref()
                .is_some_and(|s| s.host_id == host.host_id);
            let frame = theme::card_frame().stroke(egui::Stroke::new(
                if selected { 2.0 } else { 1.0 },
                if selected {
                    theme::ACCENT
                } else {
                    theme::BORDER
                },
            ));
            frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&host.name).font(theme::heading_font(18.0)));
                        ui.label(
                            RichText::new(format!(
                                "{} · IP {} · system {}",
                                if host.is_ps5 { "PlayStation 5" } else { "PlayStation 4" },
                                host.addr,
                                format_system_version(&host.system_version)
                            ))
                            .color(theme::MUTED),
                        );
                        let (color, text) = match host.state {
                            HostState::Ready => (theme::OK, "Ready to connect"),
                            HostState::Standby => (theme::WARN, "Rest mode (will be woken up)"),
                            HostState::Unknown => (theme::MUTED, "Unknown state"),
                        };
                        ui.colored_label(color, text);
                        if !host.running_app.is_empty() {
                            ui.label(RichText::new(format!("Running: {}", host.running_app)).small().color(theme::MUTED));
                        }
                        if app.config.console(&host.host_id).is_some() {
                            ui.colored_label(theme::OK, "✔ Already registered");
                        }
                        if !host.is_ps5 {
                            ui.colored_label(theme::WARN, "This is a PS4. The app is made for the PS5, but a PS4 should work too.");
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if selected { "Selected ✔" } else { "Select" };
                        if ui.add(egui::Button::new(label).selected(selected)).clicked() {
                            self.selected = Some(host.clone());
                            self.registered = app.config.console(&host.host_id).cloned();
                        }
                    });
                });
            });
        }

        ui.add_space(10.0);
        ui.label(RichText::new("Console not listed? Enter its IP address").strong());
        ui.label(
            RichText::new(
                "On the PS5: Settings > Network > Connection Status > View Connection Status.",
            )
            .small()
            .color(theme::MUTED),
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.manual_ip)
                    .hint_text("np. 192.168.1.33")
                    .desired_width(180.0),
            );
            let valid = self.manual_ip.trim().parse::<std::net::Ipv4Addr>().is_ok();
            if ui
                .add_enabled(valid, egui::Button::new("Search this address"))
                .clicked()
            {
                let ip = self.manual_ip.trim().to_string();
                if !app.manual_addresses.contains(&ip) {
                    app.manual_addresses.push(ip);
                }
                app.discovery.restart();
                self.search_started = Instant::now();
            }
            if ui.button("Search again").clicked() {
                app.discovery.restart();
                self.search_started = Instant::now();
            }
        });

        let waited = self.search_started.elapsed().as_secs();
        if hosts.is_empty() && waited >= 6 {
            ui.add_space(10.0);
            self.diagnostics(ui, app);
        }
    }

    fn diagnostics(&self, ui: &mut Ui, app: &App) {
        ui.label(RichText::new("Logs").strong());
        info_box(
            ui,
            theme::WARN,
            "The console does not respond. Check that the PS5 is on (or in rest mode with Stay Connected to the Internet) and that Remote Play is enabled.",
        );
        let manual = self.manual_ip.trim();
        if let (Some(local), Ok(target)) = (&self.local_ip, manual.parse::<std::net::Ipv4Addr>()) {
            if let Ok(local_addr) = local.parse::<std::net::Ipv4Addr>()
                && local_addr.octets()[..3] != target.octets()[..3]
            {
                info_box(
                    ui,
                    theme::ERR,
                    &format!(
                        "Probably a different network: this PC is {local} and the console address is {manual}. Both must be on the same router (guest networks and Wi-Fi client isolation block Remote Play)."
                    ),
                );
            }
        } else if let Some(local) = &self.local_ip {
            ui.label(RichText::new(format!("This PC's IP address: {local}")).color(theme::MUTED));
        }
        info_box(
            ui,
            theme::MUTED,
            "Windows Firewall: when Windows asks about network access for PS5 LAN Capture, allow Private networks. Your home network should use the Private profile. Ports used: UDP 9302-9319 (discovery), TCP/UDP 9295-9297 (streaming).",
        );
        if !app.manual_addresses.is_empty() {
            info_box(
                ui,
                theme::MUTED,
                "Nothing responds at that address. Check the address and that the console is on.",
            );
        }
    }

    fn register(&mut self, ui: &mut Ui, app: &mut App) {
        ui.label(RichText::new("Register your console").font(theme::heading_font(24.0)));
        let Some(host) = self.selected.clone() else {
            ui.label("Go back and pick a console.");
            return;
        };
        if let Some(entry) = &self.registered {
            info_box(
                ui,
                theme::OK,
                &format!(
                    "Console {} is registered. You can continue.",
                    entry.nickname
                ),
            );
            if ui.button("Register again").clicked() {
                self.registered = None;
            }
            return;
        }

        self.poll_jobs(app, &host);

        ui.label(RichText::new("1. PlayStation Network account").strong());
        let saved_account = app.config.account_id();
        if let (Some(id), false) = (saved_account, self.change_account) {
            ui.horizontal(|ui| {
                ui.colored_label(
                    theme::OK,
                    format!(
                        "✔ Account ID saved (encrypted): {}",
                        mask(&psn::account_id_base64(&id))
                    ),
                );
                if ui.small_button("Change").clicked() {
                    self.change_account = true;
                }
            });
        } else {
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    "The PS5 needs the Account ID of the PSN account you use on the console. You sign in on Sony's own page in your browser. The app never sees your password.",
                );
                ui.add_space(6.0);
                numbered_steps(
                    ui,
                    &[
                        "Click Open PlayStation sign in and sign in.",
                        "After signing in the page shows the word redirect. Copy the whole address from the address bar.",
                        "Paste it below and click Get Account ID.",
                    ],
                );
                ui.horizontal(|ui| {
                    if ui.button("🌐 Open PlayStation sign in").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(psn::login_url()));
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.redirect_input)
                        .hint_text("https://remoteplay.dl.playstation.net/remoteplay/redirect?code=…")
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    let busy = self.psn_job.is_some();
                    if ui.add_enabled(!busy && !self.redirect_input.trim().is_empty(), egui::Button::new("Get Account ID")).clicked() {
                        match psn::parse_redirect_code(&self.redirect_input) {
                            Ok(code) => {
                                let (tx, rx) = mpsc::channel();
                                std::thread::spawn(move || {
                                    let _ = tx.send(psn::fetch_account_id(&code));
                                });
                                self.psn_job = Some(rx);
                                self.account_error = None;
                            }
                            Err(e) => self.account_error = Some(e),
                        }
                    }
                    if busy {
                        ui.spinner();
                        ui.label("Contacting PSN…");
                    }
                });
                egui::CollapsingHeader::new("I already know my Account ID").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.account_input).hint_text("Base64, for example AbCdEfGhIjK=").desired_width(220.0));
                        if ui.button("Save").clicked() {
                            match psn::parse_account_id(&self.account_input) {
                                Ok(id) => self.store_account(app, id),
                                Err(e) => self.account_error = Some(e),
                            }
                        }
                    });
                });
                if let Some(err) = &self.account_error {
                    ui.colored_label(theme::ERR, err);
                }
            });
        }

        ui.add_space(12.0);
        ui.label(RichText::new("2. Pairing code").strong());
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            numbered_steps(
                ui,
                &[
                    "On the PS5 open Settings > System > Remote Play.",
                    "Select Link Device. An 8 digit code appears.",
                    "Enter the code below and click Register. The code is only valid for a few minutes.",
                ],
            );
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.pin)
                        .hint_text("12345678")
                        .char_limit(8)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(120.0),
                );
                self.pin.retain(|c| c.is_ascii_digit());
                let ready = self.pin.len() == 8
                    && app.config.account_id().is_some()
                    && self.regist.is_none();
                if (primary_button_enabled(ui, ready, "Register").clicked()
                    || (ready
                        && response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))))
                    && let Some(account) = app.config.account_id()
                {
                    self.start_regist(&host, &account);
                }
                if self.regist.is_some() {
                    ui.spinner();
                    let secs = self
                        .regist_started
                        .map(|s| s.elapsed().as_secs())
                        .unwrap_or(0);
                    ui.label(format!("Registering… {secs} s"));
                }
            });
            if app.config.account_id().is_none() {
                ui.label(
                    RichText::new("Fill in the Account ID first (step 1).")
                        .small()
                        .color(theme::MUTED),
                );
            }
            if let Some(err) = &self.regist_error {
                info_box(ui, theme::ERR, err);
            }
        });
    }

    fn store_account(&mut self, app: &mut App, id: [u8; 8]) {
        match app.config.set_account_id(&id) {
            Ok(()) => {
                app.save_now();
                self.change_account = false;
                self.account_error = None;
                self.redirect_input.clear();
                self.account_input.clear();
            }
            Err(e) => self.account_error = Some(e),
        }
    }

    fn start_regist(&mut self, host: &DiscoveredHost, account: &[u8; 8]) {
        let pin: u32 = self.pin.parse().unwrap_or(0);
        let target = if host.target != 0 {
            host.target
        } else {
            native::TARGET_PS5_1
        };
        log::info!("Registering with {} ({})", host.name, host.addr);
        match Regist::start(&host.addr, false, target, account, pin) {
            Ok(regist) => {
                self.regist = Some(regist);
                self.regist_started = Some(Instant::now());
                self.regist_error = None;
            }
            Err(e) => self.regist_error = Some(e),
        }
    }

    fn poll_jobs(&mut self, app: &mut App, host: &DiscoveredHost) {
        if let Some(rx) = &self.psn_job
            && let Ok(result) = rx.try_recv()
        {
            self.psn_job = None;
            match result {
                Ok(id) => self.store_account(app, id),
                Err(e) => self.account_error = Some(e),
            }
        }
        let Some(regist) = &self.regist else { return };
        match regist.poll() {
            RegistStatus::Pending => {}
            RegistStatus::Success(console) => {
                self.regist = None;
                let secret = ConsoleSecret {
                    regist_key: console.regist_key,
                    rp_key: console.rp_key,
                };
                match secret.seal() {
                    Ok(sealed) => {
                        let mac = console
                            .server_mac
                            .iter()
                            .map(|b| format!("{b:02X}"))
                            .collect::<Vec<_>>()
                            .join(":");
                        let host_id = if host.host_id.is_empty() {
                            mac.replace(':', "")
                        } else {
                            host.host_id.clone()
                        };
                        let entry = ConsoleEntry {
                            host_id: host_id.clone(),
                            nickname: if console.nickname.is_empty() {
                                host.name.clone()
                            } else {
                                console.nickname.clone()
                            },
                            address: host.addr.clone(),
                            is_ps5: host.is_ps5,
                            target: if console.target != 0 {
                                console.target
                            } else {
                                host.target
                            },
                            mac,
                            protected_secret: sealed,
                        };
                        log::info!("Registered console {}", entry.nickname);
                        app.config.upsert_console(entry.clone());
                        app.config.last_console = Some(host_id);
                        app.save_now();
                        self.registered = Some(entry);
                        self.pin.clear();
                    }
                    Err(e) => {
                        self.regist_error = Some(format!("Cannot encrypt registration data: {e}"))
                    }
                }
            }
            RegistStatus::Failed(detail) => {
                self.regist = None;
                log::warn!("Registration failed: {detail}");
                self.regist_error = Some(format!(
                    "Registration failed. Common causes:\n• the code expired or was mistyped (get a new one with Link Device)\n• the Account ID belongs to a different account than the one signed in on the PS5\n• the console is on another network or a firewall blocks the connection\nDetails: {}",
                    if detail.is_empty() {
                        "brak".into()
                    } else {
                        detail
                    }
                ));
            }
            RegistStatus::Canceled => {
                self.regist = None;
                self.regist_error = Some("Registration was cancelled.".into());
            }
        }
    }

    fn controller(&mut self, ui: &mut Ui, app: &mut App, video: Option<&VideoView>) {
        ui.label(RichText::new("Controller").font(theme::heading_font(24.0)));
        ui.label(
            "When Remote Play starts, the PS5 disconnects the controller connected directly to it. Connect your DualSense to this PC with a USB-C cable. The app sends buttons, sticks, triggers, touchpad and gyro to the console and gets rumble, adaptive triggers and the lightbar color back. The microphone is never sent.",
        );
        ui.add_space(6.0);
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            super::pad::controller_panel(ui, app);
        });
        ui.add_space(8.0);
        let state = app.stream.state.clone();
        ui.horizontal(|ui| {
            widgets::status_chip(ui, &state);
            if state.is_active() {
                if ui.button("Disconnect").clicked() {
                    app.stream.disconnect();
                }
            } else if primary_button(ui, "Test connection").clicked()
                && let Some(console) = self.registered.clone()
            {
                app.connect_to(console);
            }
        });
        if let StreamState::Failed { message } = &state {
            info_box(ui, theme::ERR, message);
        }
        let width = ui.available_width().min(640.0);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, width * 9.0 / 16.0), egui::Sense::hover());
        widgets::video_area(ui, rect, video);

        ui.add_space(8.0);
        ui.label(RichText::new("While connected, check and answer:").strong());
        let check = &mut app.config.controller_check;
        let before = check.clone();
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            yes_no(
                ui,
                "The controller connected to this PC controls the game",
                &mut check.pad_controls_game,
            );
            yes_no(
                ui,
                "Your monitor connected to the PS5 still shows the game",
                &mut check.monitor_shows_game,
            );
            yes_no(
                ui,
                "The PS5 picture shows up in this window",
                &mut check.app_shows_video,
            );
            yes_no(
                ui,
                "Game sound plays on this PC (headset or speakers)",
                &mut check.pc_plays_audio,
            );
        });
        if *check != before {
            app.mark_dirty();
        }
        let check = app.config.controller_check.clone();
        if check.pad_controls_game == Some(false) {
            info_box(
                ui,
                theme::WARN,
                "The controller does not control the game. Check above that the app sees it and the buttons light up. If they do but the console does not react, press the PS button. Also check that sending input is on in Settings > Controller.",
            );
        }
        if check.monitor_shows_game == Some(false) {
            info_box(
                ui,
                theme::WARN,
                "The monitor connected to the PS5 stopped showing the picture during Remote Play. This is console behavior the app cannot change.",
            );
        }
        if check.app_shows_video == Some(false) || check.pc_plays_audio == Some(false) {
            info_box(
                ui,
                theme::WARN,
                "No picture or sound in the app: check the status above and the next step. Details are in Settings > Logs.",
            );
        }
        ui.add_space(6.0);
        if ui.button("Skip test").clicked() {
            app.config.controller_check = Default::default();
            app.mark_dirty();
            self.step = Step::Audio;
        }
    }

    fn finish_view(&mut self, ui: &mut Ui, app: &App) {
        ui.add_space(24.0);
        ui.label(
            RichText::new("All set! You can start streaming.").font(theme::heading_font(30.0)),
        );
        ui.add_space(8.0);
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(console) = app.selected_console() {
                ui.label(format!("Console: {} ({})", console.nickname, console.address));
            }
            let device = app
                .config
                .audio
                .device_id
                .as_ref()
                .and_then(|id| app.audio_devices.iter().find(|d| &d.id == id))
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "Windows default device".into());
            ui.label(format!("Audio: {device}, volume {:.0}%", app.config.audio.volume * 100.0));
            let c = &app.config.controller_check;
            let verified = [c.pad_controls_game, c.monitor_shows_game, c.app_shows_video, c.pc_plays_audio];
            if verified.iter().all(|v| *v == Some(true)) {
                ui.colored_label(theme::OK, "✔ Controller, monitor, picture and sound confirmed by you");
            } else {
                ui.colored_label(theme::WARN, "The test was not fully confirmed. You can run it again from Settings > Run setup again.");
            }
        });
        ui.add_space(8.0);
        ui.label("Tip: to show the game to friends, share the PS5 LAN Capture window on Discord (Discord button in the top bar).");
    }

    fn finish(&mut self, app: &mut App) -> bool {
        app.config.wizard_completed = true;
        app.save_now();
        if !app.stream.state.is_active() && app.selected_console().is_some() {
            app.connect_selected();
        }
        true
    }
}

pub fn audio_step(ui: &mut Ui, app: &mut App) {
    ui.label(RichText::new("Audio").font(theme::heading_font(24.0)));
    ui.label("Pick where the console sound should play, usually your PC headset.");
    ui.add_space(6.0);
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        super::settings::audio_controls(ui, app);
    });
    ui.add_space(6.0);
    info_box(
        ui,
        theme::MUTED,
        "The app only plays PS5 sound and does not capture any other system sound. Your microphone is never sent to the console.",
    );
}

fn mask(value: &str) -> String {
    let visible: String = value.chars().take(3).collect();
    format!("{visible}•••••••")
}

pub fn format_system_version(raw: &str) -> String {
    if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_digit()) {
        let major = raw[0..2].trim_start_matches('0');
        let minor = &raw[2..4];
        format!("{}.{}", if major.is_empty() { "0" } else { major }, minor)
    } else {
        raw.to_string()
    }
}

fn local_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_navigation_is_linear() {
        let mut step = Step::Welcome;
        let mut seen = vec![step];
        while step != Step::Finish {
            step = step.next();
            seen.push(step);
        }
        assert_eq!(seen.len(), 7);
        while step != Step::Welcome {
            step = step.previous();
        }
        assert_eq!(step, Step::Welcome);
        assert_eq!(Step::Finish.next(), Step::Finish);
    }

    #[test]
    fn system_version_formatting() {
        assert_eq!(format_system_version("14200007"), "14.20");
        assert_eq!(format_system_version("07610001"), "7.61");
        assert_eq!(format_system_version("odd"), "odd");
    }

    #[test]
    fn account_id_is_masked() {
        assert_eq!(mask("AbCdEfGhIjK="), "AbC•••••••");
    }
}
