use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
    Visuals,
};

pub const BG: Color32 = Color32::from_rgb(0x0e, 0x10, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x19, 0x1f);
pub const CARD: Color32 = Color32::from_rgb(0x1c, 0x20, 0x28);
pub const CONTROL: Color32 = Color32::from_rgb(0x2a, 0x30, 0x3b);
pub const CONTROL_HOVER: Color32 = Color32::from_rgb(0x35, 0x3c, 0x4a);
pub const CONTROL_BORDER: Color32 = Color32::from_rgb(0x3a, 0x42, 0x50);
pub const BORDER: Color32 = Color32::from_rgb(0x2b, 0x31, 0x3c);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xee);
pub const MUTED: Color32 = Color32::from_rgb(0x9a, 0xa3, 0xb2);
pub const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x7d, 0xf6);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x1d, 0x4e, 0x9e);
pub const OK: Color32 = Color32::from_rgb(0x3d, 0xc4, 0x7e);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xb2, 0x3e);
pub const ERR: Color32 = Color32::from_rgb(0xef, 0x5b, 0x5b);

pub const HEADING_FAMILY: &str = "heading";

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut visuals = Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = CARD;
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = ACCENT_DIM;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.menu_corner_radius = CornerRadius::same(8);
    let radius = CornerRadius::same(6);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = radius;
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.weak_bg_fill = CONTROL;
    visuals.widgets.inactive.bg_fill = CONTROL;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, CONTROL_BORDER);
    visuals.widgets.hovered.weak_bg_fill = CONTROL_HOVER;
    visuals.widgets.hovered.bg_fill = CONTROL_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.active.weak_bg_fill = ACCENT_DIM;
    visuals.widgets.active.bg_fill = ACCENT_DIM;
    ctx.set_visuals(visuals);

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(26.0, FontFamily::Name(HEADING_FAMILY.into())),
            ),
            (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(15.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(12.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
    });
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let windows_fonts = std::env::var_os("WINDIR")
        .map(|w| std::path::PathBuf::from(w).join("Fonts"))
        .unwrap_or_else(|| "C:\\Windows\\Fonts".into());
    let mut load = |name: &str, file: &str| -> bool {
        match std::fs::read(windows_fonts.join(file)) {
            Ok(bytes) => {
                fonts
                    .font_data
                    .insert(name.into(), FontData::from_owned(bytes).into());
                true
            }
            Err(_) => false,
        }
    };
    let regular = load("segoe", "segoeui.ttf");
    let semibold = load("segoe-semibold", "seguisb.ttf");
    let symbols = load("segoe-symbols", "seguisym.ttf");

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    if regular {
        proportional.insert(0, "segoe".into());
    }
    if symbols {
        proportional.push("segoe-symbols".into());
    }
    let mut heading = proportional.clone();
    if semibold {
        heading.insert(0, "segoe-semibold".into());
    }
    fonts
        .families
        .insert(FontFamily::Name(HEADING_FAMILY.into()), heading);
    ctx.set_fonts(fonts);
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(16))
}

pub fn heading_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(HEADING_FAMILY.into()))
}
