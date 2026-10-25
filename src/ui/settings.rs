use egui::{RichText, Ui};

use super::widgets::info_box;
use super::wizard::{Step, Wizard};
use super::{App, theme};
use crate::audio;
use crate::config::{Resolution, Upscaling};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Console,
    Video,
    Audio,
    Pad,
    Interface,
    Diagnostics,
}

pub fn window(ctx: &egui::Context, app: &mut App) {
    if !app.settings_open {
        return;
    }
    let mut open = true;
    let tab_id = egui::Id::new("settings-tab");
    let mut tab: Tab = ctx.data(|d| d.get_temp(tab_id)).unwrap_or_default();
    egui::Window::new("Settings")
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.input(|i| i.content_rect().center()))
        .open(&mut open)
        .resizable(true)
        .default_size([620.0, 520.0])
        .collapsible(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (t, label) in [
                    (Tab::Console, "Console"),
                    (Tab::Video, "Video"),
                    (Tab::Audio, "Audio"),
                    (Tab::Pad, "Controller"),
                    (Tab::Interface, "Interface"),
                    (Tab::Diagnostics, "Logs"),
                ] {
                    if ui.selectable_label(tab == t, label).clicked() {
                        tab = t;
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| match tab {
                Tab::Console => console_tab(ui, app),
                Tab::Video => video_tab(ui, app),
                Tab::Audio => audio_controls(ui, app),
                Tab::Pad => super::pad::settings_tab(ui, app),
                Tab::Interface => interface_tab(ui, app),
                Tab::Diagnostics => diagnostics_tab(ui, app),
            });
        });
    ctx.data_mut(|d| d.insert_temp(tab_id, tab));
    app.settings_open = open;
}

fn console_tab(ui: &mut Ui, app: &mut App) {
    if app.config.consoles.is_empty() {
        ui.label("No registered consoles.");
    }
    let mut forget: Option<String> = None;
    for console in app.config.consoles.clone() {
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&console.nickname).strong());
                    ui.label(
                        RichText::new(format!(
                            "IP {} · MAC {} · {}",
                            console.address,
                            console.mac,
                            if console.is_ps5 { "PS5" } else { "PS4" }
                        ))
                        .color(theme::MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Remove")
                        .on_hover_text("Deletes the saved registration from this PC")
                        .clicked()
                    {
                        forget = Some(console.host_id.clone());
                    }
                    let is_selected =
                        app.config.last_console.as_deref() == Some(console.host_id.as_str());
                    if ui
                        .add_enabled(
                            !is_selected,
                            egui::Button::new(if is_selected {
                                "Default"
                            } else {
                                "Set as default"
                            }),
                        )
                        .clicked()
                    {
                        app.config.last_console = Some(console.host_id.clone());
                        app.mark_dirty();
                    }
                });
            });
        });
    }
    if let Some(host_id) = forget {
        if app
            .stream
            .target()
            .is_some_and(|t| t.console.host_id == host_id)
        {
            app.stream.disconnect();
        }
        app.config.consoles.retain(|c| c.host_id != host_id);
        if app.config.last_console.as_deref() == Some(host_id.as_str()) {
            app.config.last_console = app.config.consoles.first().map(|c| c.host_id.clone());
        }
        app.save_now();
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("➕ Add / register console").clicked() {
            app.wizard = Some(Wizard::starting_at(Step::FindConsole));
            app.settings_open = false;
        }
        if ui.button("Run setup again").clicked() {
            app.wizard = Some(Wizard::new());
            app.settings_open = false;
        }
    });
    ui.add_space(8.0);
    if app.config.protected_account_id.is_some()
        && ui.button("Forget saved PSN Account ID").clicked()
    {
        app.config.protected_account_id = None;
        app.save_now();
    }
}

fn video_tab(ui: &mut Ui, app: &mut App) {
    let before = app.config.video.clone();
    let video = &mut app.config.video;
    egui::Grid::new("video-grid")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("Resolution");
            egui::ComboBox::from_id_salt("resolution")
                .selected_text(video.resolution.label())
                .show_ui(ui, |ui| {
                    for r in [
                        Resolution::P1080,
                        Resolution::P720,
                        Resolution::P1440,
                        Resolution::P2160,
                    ] {
                        ui.selectable_value(&mut video.resolution, r, r.label());
                    }
                });
            ui.end_row();
            ui.label("Frame rate");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut video.fps, 60, "60 FPS");
                ui.selectable_value(&mut video.fps, 30, "30 FPS");
            });
            ui.end_row();
            ui.label("Bitrate");
            ui.add(
                egui::Slider::new(&mut video.bitrate_kbps, 2_000..=crate::config::MAX_BITRATE_KBPS)
                    .suffix(" kb/s")
                    .step_by(500.0),
            );
            ui.end_row();
            ui.label("Codec");
            ui.checkbox(&mut video.hevc, "H.265/HEVC (recommended for PS5)");
            ui.end_row();
            ui.label("HDR");
            ui.add_enabled_ui(video.hevc, |ui| {
                ui.checkbox(&mut video.hdr, "Request 10 bit HDR from the PS5")
                    .on_hover_text("Less banding in skies and dark scenes. The app tone maps it for this window and Discord.");
            });
            ui.end_row();
            ui.label("Decoding");
            ui.checkbox(&mut video.hardware_decoding, "Hardware (GPU, D3D11VA)");
            ui.end_row();
        });
    ui.add_space(6.0);
    ui.label(RichText::new("Enhance").strong());
    egui::Grid::new("enhance-grid")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("Upscaling");
            egui::ComboBox::from_id_salt("upscaling")
                .selected_text(video.upscaling.label())
                .show_ui(ui, |ui| {
                    for mode in [Upscaling::Auto, Upscaling::Fsr, Upscaling::Off] {
                        ui.selectable_value(&mut video.upscaling, mode, mode.label());
                    }
                });
            ui.end_row();
            ui.label("Sharpening");
            ui.add(percent_slider(&mut video.sharpness));
            ui.end_row();
            ui.label("Deband");
            ui.add(percent_slider(&mut video.deband))
                .on_hover_text("Smooths the color steps that compression leaves in gradients");
            ui.end_row();
            ui.label("Color edges");
            ui.checkbox(
                &mut video.better_chroma,
                "Sharper color edges (luma guided chroma)",
            );
            ui.end_row();
        });
    if *video != before {
        app.mark_dirty();
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .button("Find best quality")
            .on_hover_text("Tries higher resolutions and bitrates on your console and recommends the best working setup")
            .clicked()
        {
            app.quality_open = true;
        }
    });
    ui.add_space(6.0);
    info_box(
        ui,
        theme::MUTED,
        "Remote Play officially streams up to 1080p at 60 FPS; 1440p and 2160p are experimental. Codec, HDR, resolution and bitrate apply on the next connection; enhance settings apply right away.",
    );
    if app.config.video.upscaling != Upscaling::Off {
        let text = if app.config.video.upscaling == Upscaling::Auto && app.rtx_possible {
            "Upscaling kicks in when the video on screen is bigger than the stream (for example fullscreen on a 1440p or 4K monitor). RTX Video Super Resolution needs RTX video enhancement > Super resolution turned on in the NVIDIA Control Panel (Video > Adjust video image settings). Without it the image is only resized."
        } else {
            "Upscaling kicks in when the video on screen is bigger than the stream (for example fullscreen on a 1440p or 4K monitor)."
        };
        info_box(ui, theme::MUTED, text);
    }
    if app.stream.state.is_active() && ui.button("Reconnect to apply").clicked() {
        app.stream.disconnect();
        app.connect_selected();
    }
}

fn percent_slider(value: &mut f32) -> egui::Slider<'_> {
    egui::Slider::new(value, 0.0..=1.0).custom_formatter(|v, _| {
        if v < 0.01 {
            "Off".into()
        } else {
            format!("{:.0}%", v * 100.0)
        }
    })
}

pub fn audio_controls(ui: &mut Ui, app: &mut App) {
    let status = app.audio.status();
    let default_name = audio::default_device_name().unwrap_or_else(|| "none".into());
    let current_label = match &app.config.audio.device_id {
        None => format!("Windows default ({default_name})"),
        Some(id) => app
            .audio_devices
            .iter()
            .find(|d| &d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Device unavailable (using default)".into()),
    };
    let mut selection = app.config.audio.device_id.clone();
    egui::Grid::new("audio-grid")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("Output device");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("audio-device")
                    .width(320.0)
                    .selected_text(current_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut selection,
                            None,
                            format!("Windows default ({default_name})"),
                        );
                        for device in &app.audio_devices {
                            ui.selectable_value(
                                &mut selection,
                                Some(device.id.clone()),
                                &device.name,
                            );
                        }
                    });
                if ui
                    .button("⟳")
                    .on_hover_text("Refresh device list")
                    .clicked()
                {
                    app.refresh_audio_devices();
                }
            });
            ui.end_row();

            ui.label("Volume");
            let mut volume = app.config.audio.volume * 100.0;
            if ui
                .add(egui::Slider::new(&mut volume, 0.0..=100.0).suffix(" %"))
                .changed()
            {
                app.config.audio.volume = volume / 100.0;
                app.audio.set_volume(app.config.audio.volume);
                app.mark_dirty();
            }
            ui.end_row();

            ui.label("Mute");
            if ui
                .checkbox(&mut app.config.audio.muted, "Mute console audio")
                .changed()
            {
                app.audio.set_muted(app.config.audio.muted);
                app.mark_dirty();
            }
            ui.end_row();

            ui.label("Test");
            ui.horizontal(|ui| {
                if ui.button("🔈 Play test sound").clicked() {
                    app.audio.play_test_tone();
                }
                ui.label(
                    RichText::new("2 s, 440 Hz tone")
                        .small()
                        .color(theme::MUTED),
                );
            });
            ui.end_row();
        });
    if selection != app.config.audio.device_id {
        app.config.audio.device_id = selection.clone();
        app.audio.set_device(selection);
        app.mark_dirty();
    }
    if status.device_ok {
        ui.label(
            RichText::new(format!(
                "Active output: {} ({} Hz)",
                status.device_name, status.device_rate
            ))
            .small()
            .color(theme::MUTED),
        );
    } else if let Some(err) = &status.last_error {
        ui.colored_label(theme::ERR, format!("Audio device problem: {err}"));
    }
    egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
        let mut latency = app.config.audio.latency_ms;
        if ui
            .add(
                egui::Slider::new(&mut latency, 20..=300)
                    .suffix(" ms")
                    .text("Audio buffer (latency)"),
            )
            .on_hover_text(
                "Smaller buffer means lower latency but more crackling on a weak network",
            )
            .changed()
        {
            app.config.audio.latency_ms = latency;
            app.audio.set_latency(latency);
            app.mark_dirty();
        }
        ui.label(
            RichText::new(format!(
                "Buffer: {} ms · gaps: {} · dropped frames: {}",
                status.buffered_ms, status.underruns, status.dropped_frames
            ))
            .small()
            .color(theme::MUTED),
        );
    });
}

fn interface_tab(ui: &mut Ui, app: &mut App) {
    let before = app.config.ui.clone();
    let ui_cfg = &mut app.config.ui;
    ui.checkbox(
        &mut ui_cfg.auto_reconnect,
        "Reconnect automatically when the connection drops",
    );
    ui.checkbox(
        &mut ui_cfg.auto_hide_controls,
        "Hide the top bar while streaming (after 3 s without mouse movement)",
    );
    ui.checkbox(&mut ui_cfg.minimize_to_tray, "Minimize to system tray");
    ui.checkbox(&mut ui_cfg.show_stats, "Show stats");
    if ui_cfg.minimize_to_tray {
        info_box(
            ui,
            theme::WARN,
            "Discord cannot capture minimized windows. Do not minimize the app while sharing.",
        );
    }
    if *ui_cfg != before {
        let reconnect = ui_cfg.auto_reconnect;
        app.stream.set_auto_reconnect(reconnect);
        app.mark_dirty();
    }
    super::update::settings_section(ui, app);
    ui.add_space(8.0);
    if ui.button("How to share on Discord").clicked() {
        app.discord_help_open = true;
    }
    if ui.button("About and licenses").clicked() {
        app.about_open = true;
    }
}

fn diagnostics_tab(ui: &mut Ui, app: &mut App) {
    ui.label(format!(
        "Version: PS5 LAN Capture {} · {}",
        env!("CARGO_PKG_VERSION"),
        app.native_version
    ));
    ui.label(format!("Config: {}", app.paths.config_file.display()));
    ui.label(format!("Logs: {}", app.paths.log_dir.display()));
    ui.horizontal(|ui| {
        if ui.button("Show recent logs").clicked() {
            app.logs_open = true;
        }
        if ui.button("Open log folder").clicked() {
            let _ = std::process::Command::new("explorer")
                .arg(&app.paths.log_dir)
                .spawn();
        }
        if ui.button("Search for consoles again").clicked() {
            app.discovery.restart();
        }
    });
    ui.add_space(6.0);
    ui.label(RichText::new("Visible consoles").strong());
    if app.hosts.is_empty() {
        ui.label(RichText::new("No console responds on the network.").color(theme::MUTED));
    }
    for host in &app.hosts {
        ui.label(format!(
            "{} · {} · {:?} · {}",
            host.name, host.addr, host.state, host.host_type
        ));
    }
}
