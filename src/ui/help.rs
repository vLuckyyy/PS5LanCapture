use egui::RichText;

use super::theme;
use super::widgets::{info_box, numbered_steps};
use crate::config::Paths;
use crate::logging;

pub fn discord_window(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }
    egui::Window::new("How to share the game on Discord")
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.input(|i| i.content_rect().center()))
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .show(ctx, |ui| {
            numbered_steps(
                ui,
                &[
                    "Open Discord.",
                    "Join a voice channel.",
                    "Click Share Your Screen.",
                    "Open the Applications tab and pick the PS5 LAN Capture window.",
                    "If Discord shows a sound toggle, make sure it is on.",
                    "Click Go Live.",
                ],
            );
            ui.add_space(6.0);
            ui.label(RichText::new("Tips").strong());
            ui.label("Share the app window, not the whole screen. That way Discord only sends the game sound.");
            ui.label("Do not minimize the window while sharing. Discord cannot capture minimized windows. Covering it with another window is fine.");
            ui.label("Your Discord microphone works as usual. The app never sends it to the console.");
            ui.label("The top bar hides after 3 seconds without mouse movement, so your friends only see the game.");
            ui.add_space(6.0);
            info_box(
                ui,
                theme::MUTED,
                "If your friends cannot hear the game, share the window (not the screen) and check that app sound is enabled in Discord.",
            );
        });
}

pub fn about_window(ctx: &egui::Context, open: &mut bool, native_version: &str) {
    if !*open {
        return;
    }
    egui::Window::new("About")
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.input(|i| i.content_rect().center()))
        .open(open)
        .collapsible(false)
        .default_width(520.0)
        .show(ctx, |ui| {
            ui.label(RichText::new(format!("PS5 LAN Capture {}", env!("CARGO_PKG_VERSION"))).font(theme::heading_font(20.0)));
            ui.label(RichText::new(native_version).color(theme::MUTED));
            ui.add_space(6.0);
            ui.label("License: GNU Affero General Public License v3.0 (AGPL-3.0)");
            ui.hyperlink_to("Source code on GitHub", "https://github.com/vLuckyyy/PS5LanCapture");
            ui.add_space(6.0);
            ui.label(RichText::new("Components").strong());
            for (name, license) in [
                ("chiaki-ng (PS Remote Play protocol)", "AGPL-3.0 with OpenSSL exception"),
                ("FFmpeg (libavcodec, libavutil, libswresample)", "LGPL-2.1+, shipped as DLLs"),
                ("Opus", "BSD-3-Clause"),
                ("OpenSSL", "Apache-2.0"),
                ("libcurl", "curl license (MIT/X)"),
                ("json-c, libevent, miniupnpc, nanopb, jerasure, gf-complete", "MIT/BSD/zlib"),
                ("egui, winit, cpal, windows-rs and other Rust crates", "MIT / Apache-2.0"),
            ] {
                ui.label(format!("• {name}: {license}"));
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Full license texts are in the licenses folder next to the EXE.").small().color(theme::MUTED));
            ui.label(
                RichText::new("PlayStation, PS5 and DualSense are trademarks of Sony Interactive Entertainment. This project is not affiliated with Sony.")
                    .small()
                    .color(theme::MUTED),
            );
            ui.label(RichText::new("No telemetry. Besides your console on the local network, the app only contacts the PSN sign in server when you ask for it during setup, and GitHub to check for updates (can be turned off in Settings).").small().color(theme::MUTED));
        });
}

pub fn logs_window(ctx: &egui::Context, open: &mut bool, paths: &Paths) {
    if !*open {
        return;
    }
    egui::Window::new("Logs")
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.input(|i| i.content_rect().center()))
        .open(open)
        .default_size([760.0, 420.0])
        .show(ctx, |ui| {
            let lines = logging::recent_lines();
            ui.horizontal(|ui| {
                if ui.button("Copy to clipboard").clicked() {
                    ui.ctx().copy_text(lines.join("\n"));
                }
                if ui.button("Open log folder").clicked() {
                    let _ = std::process::Command::new("explorer")
                        .arg(&paths.log_dir)
                        .spawn();
                }
            });
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in lines {
                        let color = if line.contains("[ERROR]") {
                            theme::ERR
                        } else if line.contains("[WARN]") {
                            theme::WARN
                        } else {
                            theme::MUTED
                        };
                        ui.label(RichText::new(line).monospace().size(12.0).color(color));
                    }
                });
        });
}
