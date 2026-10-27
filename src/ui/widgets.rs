use egui::{Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, Ui, Vec2, pos2, vec2};

use super::VideoView;
use super::theme;
use crate::session::StreamState;

pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(Color32::WHITE).strong())
            .fill(theme::ACCENT)
            .min_size(vec2(120.0, 34.0)),
    )
}

pub fn primary_button_enabled(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).color(Color32::WHITE).strong())
            .fill(theme::ACCENT)
            .min_size(vec2(120.0, 34.0)),
    )
}

pub fn status_color(state: &StreamState) -> Color32 {
    match state {
        StreamState::Streaming { .. } => theme::OK,
        StreamState::Idle => theme::MUTED,
        StreamState::Failed { .. } => theme::ERR,
        _ => theme::WARN,
    }
}

pub fn status_text(state: &StreamState) -> String {
    match state {
        StreamState::Idle => "Disconnected".into(),
        StreamState::Waking { since } => {
            format!("Waking up the console… {} s", since.elapsed().as_secs())
        }
        StreamState::Connecting { since } => format!("Connecting… {} s", since.elapsed().as_secs()),
        StreamState::WaitingForLoginPin { .. } => "Waiting for passcode".into(),
        StreamState::Streaming { since } => {
            let s = since.elapsed().as_secs();
            format!("Live · {:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
        }
        StreamState::Reconnecting { attempt, at, .. } => {
            let left = at
                .saturating_duration_since(std::time::Instant::now())
                .as_secs();
            format!("Reconnecting (attempt {attempt}) in {left} s")
        }
        StreamState::Failed { .. } => "Connection error".into(),
    }
}

pub fn status_chip(ui: &mut Ui, state: &StreamState) {
    let color = status_color(state);
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 5.0, color);
                ui.label(status_text(state));
            });
        });
}

/// Paints the video letterboxed into `rect`; returns the response for the full area.
pub const VIDEO_TARGET_ID: &str = "video-target-px";

/// Paints the video letterboxed into `rect` and remembers the on-screen size in physical pixels,
/// which the renderer uses as the upscaling target on the next frame.
pub fn video_area(ui: &mut Ui, rect: Rect, video: Option<&VideoView>) -> Response {
    let response = ui.allocate_rect(rect, Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::BLACK);
    if let Some(video) = video {
        let ppp = ui.ctx().pixels_per_point();
        let aspect = video.source[0] as f32 / video.source[1].max(1) as f32;
        let mut size = vec2(rect.width(), rect.width() / aspect);
        if size.y > rect.height() {
            size = vec2(rect.height() * aspect, rect.height());
        }
        let physical = [(size.x * ppp).round() as u32, (size.y * ppp).round() as u32];
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new(VIDEO_TARGET_ID), physical));
        let exact = vec2(video.size[0] as f32, video.size[1] as f32) / ppp;
        let fits = exact.x <= rect.width() + 0.5 && exact.y <= rect.height() + 0.5;
        if (exact - size).abs().max_elem() < 3.0 || (video.pixel_perfect && fits) {
            size = exact;
        }
        let mut target = Rect::from_center_size(rect.center(), size);
        target = target.translate(vec2(
            (target.min.x * ppp).round() / ppp - target.min.x,
            (target.min.y * ppp).round() / ppp - target.min.y,
        ));
        painter.image(
            video.texture_id,
            target,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    response
}

pub fn step_indicator(ui: &mut Ui, current: usize, labels: &[&str]) {
    let width = ui.available_width();
    let height = 46.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter_at(rect);
    let n = labels.len();
    let spacing = rect.width() / n as f32;
    let y = rect.top() + 12.0;
    for (i, label) in labels.iter().enumerate() {
        let x = rect.left() + spacing * (i as f32 + 0.5);
        if i + 1 < n {
            let next = rect.left() + spacing * (i as f32 + 1.5);
            let color = if i < current {
                theme::ACCENT
            } else {
                theme::BORDER
            };
            painter.line_segment(
                [pos2(x + 9.0, y), pos2(next - 9.0, y)],
                Stroke::new(2.0, color),
            );
        }
        let (fill, text) = if i <= current {
            (theme::ACCENT, Color32::WHITE)
        } else {
            (theme::CARD, theme::MUTED)
        };
        painter.circle(
            pos2(x, y),
            9.0,
            fill,
            Stroke::new(
                1.0,
                if i == current {
                    Color32::WHITE
                } else {
                    theme::BORDER
                },
            ),
        );
        let mark = if i < current {
            "✔".to_string()
        } else {
            (i + 1).to_string()
        };
        painter.text(
            pos2(x, y),
            Align2::CENTER_CENTER,
            mark,
            egui::FontId::proportional(11.0),
            text,
        );
        painter.text(
            pos2(x, y + 22.0),
            Align2::CENTER_CENTER,
            *label,
            egui::FontId::proportional(11.5),
            if i == current {
                theme::TEXT
            } else {
                theme::MUTED
            },
        );
    }
}

pub fn network_diagram(ui: &mut Ui) {
    let width = ui.available_width().min(720.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 150.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let box_size = vec2(150.0, 64.0);
    let centers = [
        pos2(rect.left() + width * 0.16, rect.top() + 48.0),
        pos2(rect.left() + width * 0.5, rect.top() + 48.0),
        pos2(rect.left() + width * 0.84, rect.top() + 48.0),
    ];
    let labels = [
        ("PS5 Pro", "console"),
        ("Router / LAN", "cable or Wi-Fi"),
        ("Windows PC", "PS5 LAN Capture"),
    ];
    for (i, center) in centers.iter().enumerate() {
        let r = Rect::from_center_size(*center, box_size);
        painter.rect(
            r,
            CornerRadius::same(10),
            theme::CARD,
            Stroke::new(1.5, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            *center - vec2(0.0, 9.0),
            Align2::CENTER_CENTER,
            labels[i].0,
            theme::heading_font(15.0),
            theme::TEXT,
        );
        painter.text(
            *center + vec2(0.0, 13.0),
            Align2::CENTER_CENTER,
            labels[i].1,
            egui::FontId::proportional(12.0),
            theme::MUTED,
        );
    }
    for pair in centers.windows(2) {
        let from = pair[0] + vec2(box_size.x / 2.0 + 6.0, 0.0);
        let to = pair[1] - vec2(box_size.x / 2.0 + 6.0, 0.0);
        painter.arrow(from, to - from, Stroke::new(2.0, theme::ACCENT));
    }
    let monitor = pos2(centers[0].x, rect.bottom() - 18.0);
    painter.line_segment(
        [
            centers[0] + vec2(0.0, box_size.y / 2.0),
            monitor - vec2(0.0, 10.0),
        ],
        Stroke::new(1.5, theme::MUTED),
    );
    painter.text(
        monitor,
        Align2::CENTER_CENTER,
        "HDMI to your monitor",
        egui::FontId::proportional(12.0),
        theme::MUTED,
    );
    let discord = pos2(centers[2].x, rect.bottom() - 18.0);
    painter.line_segment(
        [
            centers[2] + vec2(0.0, box_size.y / 2.0),
            discord - vec2(0.0, 10.0),
        ],
        Stroke::new(1.5, theme::MUTED),
    );
    painter.text(
        discord,
        Align2::CENTER_CENTER,
        "Discord to friends",
        egui::FontId::proportional(12.0),
        theme::MUTED,
    );
}

pub fn numbered_steps(ui: &mut Ui, steps: &[&str]) {
    for (i, step) in steps.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 11.0, theme::ACCENT_DIM);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                (i + 1).to_string(),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
            ui.label(*step);
        });
    }
}

pub fn yes_no(ui: &mut Ui, question: &str, value: &mut Option<bool>) {
    ui.horizontal(|ui| {
        let icon = match value {
            Some(true) => egui::RichText::new("✔").color(theme::OK),
            Some(false) => egui::RichText::new("✖").color(theme::ERR),
            None => egui::RichText::new("•").color(theme::MUTED),
        };
        ui.label(icon);
        ui.label(question);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.selectable_label(*value == Some(false), "No").clicked() {
                *value = Some(false);
            }
            if ui.selectable_label(*value == Some(true), "Yes").clicked() {
                *value = Some(true);
            }
        });
    });
}

pub fn info_box(ui: &mut Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(text);
        });
}
