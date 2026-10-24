use egui::{Align2, Color32, CornerRadius, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::widgets::info_box;
use super::{App, theme};
use crate::controller::*;

pub fn status_chip(ui: &mut Ui, status: &ControllerStatus) {
    let (color, text) = match (&status.name, status.connection) {
        (Some(_), Some(connection)) => {
            let battery = match (status.battery_percent, status.charging) {
                (Some(level), true) => format!(" · {level}% ⚡"),
                (Some(level), false) => format!(" · {level}%"),
                _ => String::new(),
            };
            let color = if status.forwarding {
                theme::OK
            } else {
                theme::TEXT
            };
            (
                color,
                format!("🎮 Controller {}{battery}", connection.label()),
            )
        }
        _ => (theme::WARN, "🎮 No controller".to_string()),
    };
    let hover = if status.forwarding {
        "The controller connected to this PC is controlling the console"
    } else if status.name.is_some() {
        "Controller found. Input starts once you are connected to the console"
    } else {
        "Connect a DualSense to this PC with a USB-C cable to play"
    };
    ui.label(RichText::new(text).color(color))
        .on_hover_text(hover);
}

fn button_chip(ui: &mut Ui, label: &str, pressed: bool) {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::FontId::proportional(12.0),
        Color32::WHITE,
    );
    let size = vec2(galley.size().x + 14.0, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let fill = if pressed {
        theme::ACCENT
    } else {
        theme::CONTROL
    };
    ui.painter().rect_filled(rect, CornerRadius::same(11), fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        if pressed {
            Color32::WHITE
        } else {
            theme::MUTED
        },
    );
}

fn stick(ui: &mut Ui, label: &str, x: i16, y: i16, pressed: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(64.0, 76.0), Sense::hover());
    let center = pos2(rect.center().x, rect.top() + 32.0);
    let painter = ui.painter();
    painter.circle(
        center,
        28.0,
        theme::CONTROL,
        Stroke::new(
            1.0,
            if pressed {
                theme::ACCENT
            } else {
                theme::CONTROL_BORDER
            },
        ),
    );
    let dot = center + vec2(x as f32 / 32768.0 * 22.0, y as f32 / 32768.0 * 22.0);
    painter.circle_filled(dot, 6.0, theme::ACCENT);
    painter.text(
        pos2(center.x, rect.bottom() - 6.0),
        Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        theme::MUTED,
    );
}

fn trigger(ui: &mut Ui, label: &str, value: u8) {
    let (rect, _) = ui.allocate_exact_size(vec2(26.0, 76.0), Sense::hover());
    let bar = egui::Rect::from_min_size(rect.min + vec2(5.0, 4.0), vec2(16.0, 56.0));
    let painter = ui.painter();
    painter.rect_filled(bar, CornerRadius::same(4), theme::CONTROL);
    let fill_h = bar.height() * value as f32 / 255.0;
    painter.rect_filled(
        egui::Rect::from_min_max(pos2(bar.left(), bar.bottom() - fill_h), bar.max),
        CornerRadius::same(4),
        theme::ACCENT,
    );
    painter.text(
        pos2(rect.center().x, rect.bottom() - 6.0),
        Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        theme::MUTED,
    );
}

/// Live view of the pad connected to the PC, so the user can confirm it is read correctly.
pub fn live_view(ui: &mut Ui, status: &ControllerStatus) {
    let input = &status.input;
    let b = |mask: u32| input.buttons & mask != 0;
    ui.horizontal(|ui| {
        trigger(ui, "L2", input.l2);
        stick(ui, "L", input.left_x, input.left_y, b(BUTTON_L3));
        stick(ui, "R", input.right_x, input.right_y, b(BUTTON_R3));
        trigger(ui, "R2", input.r2);
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                for (label, mask) in [
                    ("✕", BUTTON_CROSS),
                    ("○", BUTTON_CIRCLE),
                    ("□", BUTTON_SQUARE),
                    ("△", BUTTON_TRIANGLE),
                ] {
                    button_chip(ui, label, b(mask));
                }
                for (label, mask) in [
                    ("↑", BUTTON_DPAD_UP),
                    ("↓", BUTTON_DPAD_DOWN),
                    ("←", BUTTON_DPAD_LEFT),
                    ("→", BUTTON_DPAD_RIGHT),
                ] {
                    button_chip(ui, label, b(mask));
                }
            });
            ui.horizontal_wrapped(|ui| {
                for (label, mask) in [
                    ("L1", BUTTON_L1),
                    ("R1", BUTTON_R1),
                    ("Create", BUTTON_CREATE),
                    ("Options", BUTTON_OPTIONS),
                    ("PS", BUTTON_PS),
                    ("Touchpad", BUTTON_TOUCHPAD),
                ] {
                    button_chip(ui, label, b(mask));
                }
                let touches = input.touch_active.iter().filter(|t| **t != 0).count();
                button_chip(ui, &format!("Touch: {touches}"), touches > 0);
                button_chip(
                    ui,
                    if input.has_motion != 0 {
                        "Gyro ✔"
                    } else {
                        "Gyro: none"
                    },
                    false,
                );
            });
        });
    });
}

pub fn controller_panel(ui: &mut Ui, app: &App) {
    let status = app.controller.status();
    match (&status.name, status.connection) {
        (Some(name), Some(connection)) => {
            ui.horizontal(|ui| {
                ui.colored_label(theme::OK, "✔");
                ui.label(RichText::new(format!("{name} ({})", connection.label())).strong());
                if let Some(level) = status.battery_percent {
                    ui.label(
                        RichText::new(format!(
                            "battery {level}%{}",
                            if status.charging { " (charging)" } else { "" }
                        ))
                        .color(theme::MUTED),
                    );
                }
            });
            if status.forwarding {
                ui.colored_label(theme::OK, "Input is being sent to the console.");
            } else if app.config.controller.forward_input {
                ui.label(
                    RichText::new(
                        "Input starts once you are connected. Press some buttons to test:",
                    )
                    .color(theme::MUTED),
                );
            } else {
                ui.colored_label(theme::WARN, "Sending input is turned off in settings.");
            }
            live_view(ui, &status);
            if connection == Connection::XInput {
                ui.label(
                    RichText::new(
                        "XInput controller: PS button = Back + Start. No touchpad or gyro.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
            }
        }
        _ => {
            info_box(
                ui,
                theme::WARN,
                "No controller found. Connect a DualSense to this PC with a USB-C cable, or pair it over Bluetooth (hold Create + PS until the light blinks fast).",
            );
        }
    }
    if let Some(err) = &status.error {
        ui.colored_label(theme::ERR, err);
    }
}

pub fn settings_tab(ui: &mut Ui, app: &mut App) {
    controller_panel(ui, app);
    ui.add_space(8.0);
    let before = app.config.controller.clone();
    let c = &mut app.config.controller;
    ui.checkbox(
        &mut c.forward_input,
        "Send input from the controller connected to this PC",
    );
    ui.checkbox(&mut c.rumble, "Rumble (also from DualSense haptics)");
    ui.checkbox(
        &mut c.adaptive_triggers,
        "Adaptive triggers L2/R2 (DualSense only)",
    );
    ui.checkbox(
        &mut c.lightbar,
        "Lightbar color and player LEDs from the console (DualSense only)",
    );
    if *c != before {
        app.controller.apply(&app.config.controller.settings());
        app.mark_dirty();
    }
    ui.add_space(6.0);
    info_box(
        ui,
        theme::MUTED,
        "Once Remote Play starts, the PS5 disconnects the controller connected directly to it, so you play with the controller connected to this PC. If Steam is running with PlayStation Configuration Support, it may fight over rumble and colors. Turn that option off if you see problems.",
    );
}
