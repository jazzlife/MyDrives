//! The look of the window: one palette, applied to egui's visuals.
//!
//! Modelled on macOS system windows - a tinted source list on the left, a white
//! content area, hairline separators, 13 pt text and one accent colour that is
//! the only saturated thing on screen. The same palette is defined for dark
//! mode, and [`apply`] is called again whenever the OS switches, so the window
//! never sits in the wrong theme.

use egui::{Color32, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals};

#[derive(Clone, Copy)]
pub struct Palette {
    /// Content area behind the detail pane.
    pub window: Color32,
    /// Source list on the left.
    pub sidebar: Color32,
    /// Raised surfaces: info cards, form groups.
    pub card: Color32,
    /// Hairlines. One pixel, barely there.
    pub border: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    /// Row hover in the source list.
    pub hover: Color32,
    /// Background of text inputs.
    pub field: Color32,
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        window: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        sidebar: Color32::from_rgb(0xF0, 0xF0, 0xF3),
        card: Color32::from_rgb(0xF7, 0xF7, 0xF9),
        border: Color32::from_rgb(0xDE, 0xDE, 0xE3),
        text: Color32::from_rgb(0x1D, 0x1D, 0x1F),
        dim: Color32::from_rgb(0x7B, 0x7B, 0x85),
        accent: Color32::from_rgb(0x00, 0x7A, 0xFF),
        on_accent: Color32::WHITE,
        ok: Color32::from_rgb(0x28, 0xA7, 0x45),
        warn: Color32::from_rgb(0xE0, 0x8A, 0x00),
        err: Color32::from_rgb(0xE0, 0x3A, 0x30),
        hover: Color32::from_rgb(0xE4, 0xE4, 0xE9),
        field: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    };

    pub const DARK: Palette = Palette {
        window: Color32::from_rgb(0x1E, 0x1E, 0x20),
        sidebar: Color32::from_rgb(0x17, 0x17, 0x19),
        card: Color32::from_rgb(0x27, 0x27, 0x2A),
        border: Color32::from_rgb(0x39, 0x39, 0x3E),
        text: Color32::from_rgb(0xF2, 0xF2, 0xF5),
        dim: Color32::from_rgb(0x9A, 0x9A, 0xA3),
        accent: Color32::from_rgb(0x0A, 0x84, 0xFF),
        on_accent: Color32::WHITE,
        ok: Color32::from_rgb(0x30, 0xD1, 0x58),
        warn: Color32::from_rgb(0xFF, 0xD6, 0x0A),
        err: Color32::from_rgb(0xFF, 0x45, 0x3A),
        hover: Color32::from_rgb(0x2E, 0x2E, 0x33),
        field: Color32::from_rgb(0x19, 0x19, 0x1B),
    };

    pub fn of(dark: bool) -> Self {
        if dark {
            Self::DARK
        } else {
            Self::LIGHT
        }
    }
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = Palette::of(dark);
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };

    v.panel_fill = p.window;
    v.window_fill = p.window;
    v.faint_bg_color = p.card;
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.code_bg_color = p.card;
    v.weak_text_color = Some(p.dim);
    v.hyperlink_color = p.accent;
    v.error_fg_color = p.err;
    v.warn_fg_color = p.warn;
    v.window_corner_radius = 10.into();
    v.menu_corner_radius = 8.into();
    v.window_stroke = Stroke::new(1.0, p.border);
    v.selection.bg_fill = p.accent.linear_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);

    // Buttons and inputs: a flat surface with a hairline, no gradient, no
    // expansion on hover - the colour change carries the feedback.
    let radius = egui::CornerRadius::same(7);
    v.widgets.noninteractive.bg_fill = p.card;
    v.widgets.noninteractive.weak_bg_fill = p.card;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.corner_radius = radius;

    v.widgets.inactive.bg_fill = p.field;
    v.widgets.inactive.weak_bg_fill = if dark { p.card } else { p.window };
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = radius;
    v.widgets.inactive.expansion = 0.0;

    v.widgets.hovered.bg_fill = p.hover;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.corner_radius = radius;
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.bg_fill = p.accent.linear_multiply(0.45);
    v.widgets.active.weak_bg_fill = p.accent.linear_multiply(0.45);
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.corner_radius = radius;
    v.widgets.active.expansion = 0.0;

    v.widgets.open.bg_fill = p.card;
    v.widgets.open.weak_bg_fill = p.card;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.open.corner_radius = radius;

    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        // 13 pt body is the macOS system size; everything else is relative to it.
        s.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.window_margin = Margin::ZERO;
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.icon_width = 16.0;
        s.spacing.text_edit_width = 260.0;
        s.visuals.button_frame = true;
    });
}
