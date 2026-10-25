use std::time::Duration;

use egui::{Align, Layout, RichText, Ui};

use super::widgets::{self, info_box, primary_button};
use super::{App, VideoView, WindowRequest, theme};
use crate::native::HostState;
use crate::session::StreamState;

const CONTROLS_HIDE_AFTER: Duration = Duration::from_secs(3);

pub fn ui(ui: &mut Ui, app: &mut App, video: Option<&VideoView>) {
    let streaming = matches!(app.stream.state, StreamState::Streaming { .. });
    let idle_long = app.last_activity.elapsed() > CONTROLS_HIDE_AFTER;
    let hide_controls = streaming
        && video.is_some()
        && idle_long
        && (app.config.ui.auto_hide_controls || app.fullscreen);
    if !hide_controls {
        top_bar(ui, app);
    }
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(egui::Color32::BLACK))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            let showing_video = streaming && video.is_some();
            let response = widgets::video_area(ui, rect, if showing_video { video } else { None });
            if response.double_clicked() {
                app.requests.push(WindowRequest::ToggleFullscreen);
            }
            if !showing_video {
                placeholder(ui, app, rect);
            }
            if app.config.ui.show_stats && app.stream.state.is_active() {
                stats_overlay(ui, app, rect, video);
            }
            if hide_controls {
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
            }
        });
}

fn top_bar(ui: &mut Ui, app: &mut App) {
    egui::Panel::top("top-bar")
        .frame(
            egui::Frame::new()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::status_chip(ui, &app.stream.state);
                super::pad::status_chip(ui, &app.controller.status());
                console_picker(ui, app);
                if app.stream.state.is_active() {
                    if ui.button("⏹ Disconnect").clicked() {
                        app.stream.disconnect();
                    }
                } else if primary_button(ui, "▶ Connect").clicked() {
                    app.connect_selected();
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    super::update::top_bar_button(ui, app);
                    if ui
                        .button("⚙ Settings")
                        .on_hover_text("App settings")
                        .clicked()
                    {
                        app.settings_open = !app.settings_open;
                    }
                    if ui
                        .button("Discord")
                        .on_hover_text("How to share the game on Discord")
                        .clicked()
                    {
                        app.discord_help_open = !app.discord_help_open;
                    }
                    let fs_label = if app.fullscreen {
                        "🗗 Window"
                    } else {
                        "⛶ Fullscreen"
                    };
                    if ui
                        .button(fs_label)
                        .on_hover_text("F11 or double click the video")
                        .clicked()
                    {
                        app.requests.push(WindowRequest::ToggleFullscreen);
                    }
                    if ui
                        .selectable_label(app.config.ui.show_stats, "📊")
                        .on_hover_text("Stats")
                        .clicked()
                    {
                        app.config.ui.show_stats = !app.config.ui.show_stats;
                        app.mark_dirty();
                    }
                    volume_controls(ui, app);
                });
            });
        });
}

fn console_picker(ui: &mut Ui, app: &mut App) {
    let consoles = app.config.consoles.clone();
    let Some(selected) = app.selected_console().cloned() else {
        ui.label(RichText::new("No registered console").color(theme::MUTED));
        return;
    };
    let state_text = |state: Option<HostState>| match state {
        Some(HostState::Ready) => "ready",
        Some(HostState::Standby) => "rest mode",
        _ => "not found",
    };
    let label = format!(
        "{} · {}",
        selected.nickname,
        state_text(app.console_state(&selected))
    );
    if consoles.len() <= 1 {
        ui.label(RichText::new(label).color(theme::MUTED));
        return;
    }
    let active = app.stream.state.is_active();
    ui.add_enabled_ui(!active, |ui| {
        egui::ComboBox::from_id_salt("console-picker")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for console in &consoles {
                    let text = format!(
                        "{} ({}) · {}",
                        console.nickname,
                        console.address,
                        state_text(app.console_state(console))
                    );
                    if ui
                        .selectable_label(console.host_id == selected.host_id, text)
                        .clicked()
                    {
                        app.config.last_console = Some(console.host_id.clone());
                        app.mark_dirty();
                    }
                }
            });
    });
}

fn volume_controls(ui: &mut Ui, app: &mut App) {
    let mut volume = app.config.audio.volume * 100.0;
    let response = ui.add(
        egui::Slider::new(&mut volume, 0.0..=100.0)
            .show_value(false)
            .trailing_fill(true),
    );
    if response.changed() {
        app.config.audio.volume = volume / 100.0;
        app.audio.set_volume(app.config.audio.volume);
        app.mark_dirty();
    }
    let icon = if app.config.audio.muted || app.config.audio.volume == 0.0 {
        "🔇"
    } else {
        "🔊"
    };
    if ui
        .button(icon)
        .on_hover_text(if app.config.audio.muted {
            "Unmute"
        } else {
            "Mute"
        })
        .clicked()
    {
        app.config.audio.muted = !app.config.audio.muted;
        app.audio.set_muted(app.config.audio.muted);
        app.mark_dirty();
    }
}

fn placeholder(ui: &mut Ui, app: &mut App, rect: egui::Rect) {
    let card = egui::Rect::from_center_size(
        rect.center(),
        egui::vec2(rect.width().min(560.0) - 32.0, 260.0),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("placeholder")
            .max_rect(card)
            .layout(Layout::top_down(Align::Center)),
    );
    theme::card_frame().show(&mut child, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| match app.stream.state.clone() {
            StreamState::Idle => {
                ui.label(RichText::new("PS5 LAN Capture").font(theme::heading_font(26.0)));
                match app.selected_console().cloned() {
                    Some(console) => {
                        let state = app.console_state(&console);
                        ui.label(format!(
                            "Console: {} ({})",
                            console.nickname, console.address
                        ));
                        ui.label(
                            RichText::new(match state {
                                Some(HostState::Ready) => "The console is on and ready.",
                                Some(HostState::Standby) => {
                                    "The console is in rest mode and will be woken up."
                                }
                                _ => "The console does not respond (off or on another network).",
                            })
                            .color(theme::MUTED),
                        );
                        ui.add_space(8.0);
                        if primary_button(ui, "▶ Connect to console").clicked() {
                            app.connect_to(console);
                        }
                    }
                    None => {
                        ui.label("No console registered yet.");
                        if primary_button(ui, "Run setup").clicked() {
                            app.wizard = Some(super::wizard::Wizard::new());
                        }
                    }
                }
            }
            StreamState::Failed { message } => {
                ui.label(
                    RichText::new("Could not connect")
                        .font(theme::heading_font(22.0))
                        .color(theme::ERR),
                );
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, "Try again").clicked() {
                        app.connect_selected();
                    }
                    if ui.button("Logs").clicked() {
                        app.logs_open = true;
                    }
                });
            }
            StreamState::Reconnecting { reason, .. } => {
                ui.spinner();
                ui.label(
                    RichText::new(widgets::status_text(&app.stream.state))
                        .font(theme::heading_font(18.0)),
                );
                ui.label(RichText::new(reason).color(theme::MUTED));
            }
            state => {
                ui.spinner();
                ui.label(
                    RichText::new(widgets::status_text(&state)).font(theme::heading_font(18.0)),
                );
                if matches!(state, StreamState::Streaming { .. }) {
                    ui.label(
                        RichText::new("Waiting for the first video frame…").color(theme::MUTED),
                    );
                }
            }
        });
    });
    if let Some(err) = &app.gpu_error {
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("gpu-error")
                .max_rect(card.translate(egui::vec2(0.0, 280.0))),
        );
        info_box(&mut child, theme::ERR, err);
    }
}

fn stats_overlay(ui: &mut Ui, app: &App, rect: egui::Rect, video: Option<&VideoView>) {
    let stats = &app.stream.stats;
    let audio = app.audio.status();
    let area = egui::Rect::from_min_size(
        rect.right_top() + egui::vec2(-250.0, 10.0),
        egui::vec2(240.0, 270.0),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("stats").max_rect(area));
    egui::Frame::new()
        .fill(egui::Color32::from_black_alpha(180))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(10))
        .show(&mut child, |ui| {
            let mono = |text: String| RichText::new(text).monospace().size(12.0);
            let resolution = stats
                .resolution
                .map(|(w, h)| format!("{w}×{h}"))
                .unwrap_or_else(|| "n/a".into());
            ui.label(mono(format!("Video     {resolution}")));
            ui.label(mono(format!("FPS       {:.1}", stats.fps)));
            ui.label(mono(format!("Bitrate   {:.1} Mb/s", stats.bitrate_mbps)));
            ui.label(mono(format!(
                "Lost      {}  Dropped {}",
                stats.frames_lost, stats.frames_dropped
            )));
            ui.label(mono(match stats.rtt_ms {
                Some(rtt) => format!("RTT       {rtt:.1} ms"),
                None => "RTT       n/a".into(),
            }));
            ui.label(mono(format!(
                "Decoder   {}",
                match stats.hardware_decoding {
                    Some(true) => "GPU (D3D11VA)",
                    Some(false) => "CPU (software)",
                    None => "n/a",
                }
            )));
            ui.label(mono(
                match video.and_then(|v| v.scaler.map(|k| (k, v.size))) {
                    Some((kind, [w, h])) => format!("Output    {} {w}x{h}", kind.short()),
                    None => "Output    native".into(),
                },
            ));
            ui.label(mono(format!(
                "Range     {}",
                if video.is_some_and(|v| v.hdr) {
                    "HDR10, tone mapped"
                } else {
                    "SDR"
                }
            )));
            ui.label(mono(format!(
                "Audio     {} / {} ms",
                audio.buffered_ms, audio.target_ms
            )));
            ui.label(mono(format!("Audio gaps {}", audio.underruns)));
        });
}
