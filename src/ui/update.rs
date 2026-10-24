use std::sync::atomic::Ordering;

use egui::{RichText, Ui};

use super::widgets::{info_box, primary_button};
use super::{App, WindowRequest, theme};
use crate::updater::{self, UpdateState, Version};

const MAX_NOTES_CHARS: usize = 2000;

pub fn top_bar_button(ui: &mut Ui, app: &mut App) {
    let Some(release) = app.updater.available() else {
        return;
    };
    let label = RichText::new(format!("⬆ Update {}", release.version))
        .color(egui::Color32::WHITE)
        .strong();
    if ui
        .add(egui::Button::new(label).fill(theme::OK.gamma_multiply(0.7)))
        .on_hover_text("A new version is available")
        .clicked()
    {
        app.update_open = true;
    }
}

pub fn window(ctx: &egui::Context, app: &mut App) {
    if !app.update_open {
        return;
    }
    let mut open = true;
    egui::Window::new("Update")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(480.0)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.input(|i| i.content_rect().center()))
        .show(ctx, |ui| contents(ui, app));
    if !open {
        app.update_open = false;
    }
}

fn contents(ui: &mut Ui, app: &mut App) {
    let current = Version::current();
    match app.updater.state.clone() {
        UpdateState::Available(release) => {
            ui.label(
                RichText::new(format!("Version {} is available", release.version))
                    .font(theme::heading_font(20.0)),
            );
            ui.label(RichText::new(format!("You have version {current}.")).color(theme::MUTED));
            notes(ui, &release.notes);
            ui.hyperlink_to("Release page on GitHub", &release.page_url);
            ui.add_space(8.0);
            let installed = updater::is_installed();
            if installed && release.installer.is_some() {
                if app.stream.state.is_active() {
                    info_box(ui, theme::WARN, "Updating will end the current stream.");
                }
                ui.horizontal(|ui| {
                    if primary_button(ui, "Update now").clicked() {
                        app.updater.start_download();
                    }
                    if ui.button("Later").clicked() {
                        app.update_open = false;
                    }
                });
            } else {
                ui.label("This copy is not installed with the setup, so download the new version manually.");
                ui.horizontal(|ui| {
                    if primary_button(ui, "Open download page").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&release.page_url));
                    }
                    if ui.button("Later").clicked() {
                        app.update_open = false;
                    }
                });
            }
        }
        UpdateState::Downloading(release) => {
            ui.label(format!("Downloading version {}…", release.version));
            let done = app.updater.progress.done.load(Ordering::Relaxed);
            let total = app.updater.progress.total.load(Ordering::Relaxed);
            let fraction = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            ui.add(egui::ProgressBar::new(fraction).text(format!(
                "{:.1} / {:.1} MB",
                done as f64 / 1_048_576.0,
                total as f64 / 1_048_576.0
            )));
            if ui.button("Cancel").clicked() {
                app.updater.cancel_download();
            }
        }
        UpdateState::Ready(release, path) => {
            ui.label(format!(
                "Version {} is downloaded. The app will close, install the update and start again.",
                release.version
            ));
            ui.horizontal(|ui| {
                if primary_button(ui, "Install and restart").clicked() {
                    match updater::launch_installer(&path) {
                        Ok(()) => app.requests.push(WindowRequest::Quit),
                        Err(e) => app.updater.state = UpdateState::Failed(e),
                    }
                }
                if ui.button("Later").clicked() {
                    app.update_open = false;
                }
            });
        }
        UpdateState::Failed(error) => {
            info_box(ui, theme::ERR, &format!("Update failed: {error}"));
            ui.horizontal(|ui| {
                if ui.button("Try again").clicked() {
                    app.updater.check_now();
                }
                ui.hyperlink_to(
                    "Open releases page",
                    format!("{}/releases", updater::REPOSITORY_URL),
                );
            });
        }
        UpdateState::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking for updates…");
            });
        }
        UpdateState::UpToDate | UpdateState::Idle => {
            ui.label(format!("You have the latest version ({current})."));
        }
    }
}

fn notes(ui: &mut Ui, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut shown: String = text.chars().take(MAX_NOTES_CHARS).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    ui.add_space(4.0);
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                ui.label(shown);
            });
    });
}

pub fn settings_section(ui: &mut Ui, app: &mut App) {
    ui.add_space(8.0);
    ui.label(RichText::new("Updates").strong());
    if ui
        .checkbox(
            &mut app.config.ui.check_for_updates,
            "Check for updates automatically",
        )
        .changed()
    {
        app.mark_dirty();
    }
    ui.horizontal(|ui| {
        if ui.button("Check for updates").clicked() {
            app.updater.check_now();
            app.update_open = true;
        }
        let status = match &app.updater.state {
            UpdateState::Checking => "Checking…".to_string(),
            UpdateState::UpToDate => format!("Up to date ({})", Version::current()),
            UpdateState::Available(r) | UpdateState::Downloading(r) | UpdateState::Ready(r, _) => {
                format!("Version {} is available", r.version)
            }
            UpdateState::Failed(_) => "Last check failed".to_string(),
            UpdateState::Idle => format!("Version {}", Version::current()),
        };
        ui.label(RichText::new(status).color(theme::MUTED));
    });
}
