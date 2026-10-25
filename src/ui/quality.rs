use egui::{RichText, Ui};

use super::widgets::{info_box, primary_button};
use super::{App, theme};
use crate::quality_test::QualityTest;

pub fn window(ctx: &egui::Context, app: &mut App) {
    if !app.quality_open {
        return;
    }
    let mut open = true;
    egui::Window::new("Find best quality")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.input(|i| i.content_rect().center()))
        .show(ctx, |ui| contents(ui, app));
    if !open {
        app.quality_open = false;
        app.quality_test = None;
    }
}

fn contents(ui: &mut Ui, app: &mut App) {
    let Some(test) = &app.quality_test else {
        intro(ui, app);
        return;
    };
    for result in &test.results {
        let (icon, color) = if result.passed {
            ("✔", theme::OK)
        } else {
            ("✖", theme::ERR)
        };
        ui.horizontal(|ui| {
            ui.colored_label(color, icon);
            ui.label(RichText::new(result.trial.label()).strong());
            ui.label(RichText::new(&result.detail).color(theme::MUTED));
        });
    }
    if let Some((trial, stage)) = test.current() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(trial.label()).strong());
            ui.label(RichText::new(stage).color(theme::MUTED));
        });
    } else if !test.finished {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Waiting for the console to close the previous test…");
        });
    }
    ui.add_space(8.0);
    if !test.finished {
        ui.label(
            RichText::new(format!(
                "Step {} of up to {}",
                test.results.len() + 1,
                test.total_planned()
            ))
            .color(theme::MUTED),
        );
        if ui.button("Cancel").clicked()
            && let Some(test) = &mut app.quality_test
        {
            test.cancel();
        }
        return;
    }
    if let Some(reason) = &test.aborted {
        info_box(ui, theme::WARN, &format!("Test stopped: {reason}"));
    }
    let (resolution, bitrate) = test.recommendation();
    let Some(bitrate) = bitrate else {
        info_box(
            ui,
            theme::ERR,
            "No bitrate step passed. Check the network (cable instead of Wi-Fi) and try again.",
        );
        if ui.button("Run again").clicked() {
            app.quality_test = None;
        }
        return;
    };
    ui.label(RichText::new("Recommended").font(theme::heading_font(18.0)));
    ui.label(format!(
        "{} at {} kb/s ({:.1} Mb/s, with headroom below the highest step that passed)",
        resolution.label(),
        bitrate,
        bitrate as f32 / 1000.0
    ));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if primary_button(ui, "Apply recommended").clicked() {
            app.config.video.resolution = resolution;
            app.config.video.bitrate_kbps = bitrate;
            app.mark_dirty();
            app.quality_test = None;
            app.quality_open = false;
            app.notify("Quality settings applied. Connect to stream with them.");
        }
        if ui.button("Run again").clicked() {
            app.quality_test = None;
        }
    });
}

fn intro(ui: &mut Ui, app: &mut App) {
    ui.label("The app starts a few short streams with your console and measures each one:");
    ui.label(
        "• first it asks for 1440p and 2160p (experimental, the PS5 may refuse or send 1080p)",
    );
    ui.label("• then it raises the bitrate step by step up to 100 Mb/s until frames get lost");
    ui.add_space(6.0);
    info_box(
        ui,
        theme::WARN,
        "Start a game with a lot of motion first and leave it running. Menus and still scenes are too easy to compress and the result will be too optimistic. The test takes about 2 to 3 minutes and ends the current stream.",
    );
    ui.add_space(6.0);
    let target = app.selected_target();
    if let Err(e) = &target {
        info_box(ui, theme::ERR, e);
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(target.is_ok(), egui::Button::new("Start test"))
            .clicked()
            && let Ok(target) = target
        {
            app.stream.disconnect();
            app.audio.clear();
            app.quality_test = Some(QualityTest::new(
                target,
                app.config.video.clone(),
                app.render_device,
                app.stream.waker(),
            ));
        }
        if ui.button("Close").clicked() {
            app.quality_open = false;
        }
    });
}
