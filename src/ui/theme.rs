//! Colours and visuals modelled on the 1.0.0 Win32 look.

use crate::format::Level;
use egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x16);
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x20);
pub const HEADER: Color32 = Color32::from_rgb(0x4f, 0xc3, 0xf7);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
pub const MUTED: Color32 = Color32::from_rgb(0x9a, 0x9a, 0xa0);
pub const TRACK: Color32 = Color32::from_rgb(0x2e, 0x2e, 0x33);
pub const OK: Color32 = Color32::from_rgb(0x45, 0xc0, 0x62);
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xb4, 0x29);
pub const CRIT: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
pub const PENDING: Color32 = Color32::from_rgb(0x70, 0x70, 0x78);

pub fn level_color(level: Level) -> Color32 {
    match level {
        Level::Ok => OK,
        Level::Warn => WARN,
        Level::Crit => CRIT,
    }
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.override_text_color = Some(TEXT);
    ctx.set_visuals_of(egui::Theme::Dark, visuals);
    // Selectable labels would swallow right-clicks meant for the context menu.
    ctx.all_styles_mut(|style| style.interaction.selectable_labels = false);
}
