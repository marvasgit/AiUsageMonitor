//! Top-level eframe app: stacked provider cards plus a context menu.

use super::card::{self, TimeMode};
use super::theme;
use crate::model::ProviderState;
use crate::poller::SharedState;
use egui::{RichText, Sense, ViewportCommand, WindowLevel};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const TIME_MODE_KEY: &str = "time_mode";
const ALWAYS_ON_TOP_KEY: &str = "always_on_top";
const REPAINT_EVERY: Duration = Duration::from_secs(1);
const CARD_GAP: f32 = 6.0;
const WINDOW_PADDING: f32 = 24.0;
const NO_PROVIDERS: &str = "No providers detected.\n\nLooked for:\n  Claude Code: .credentials.json in ~/.claude or any folder in your home\n  Codex CLI: auth.json in ~/.codex or any folder in your home\n  GitHub Copilot: Copilot CLI, copilot.vim or gh login\n  Cursor: Cursor app or cursor-agent login\n  MiniMax: ~/.mmx/config.json or $MINIMAX_API_KEY";

/// Draws all cards; returns the responses of every clickable reset-time label.
pub fn draw_body(
    ui: &mut egui::Ui,
    states: &[ProviderState],
    now: SystemTime,
    mode: TimeMode,
) -> Vec<egui::Response> {
    if states.is_empty() {
        ui.label(RichText::new(NO_PROVIDERS).color(theme::MUTED));
        return Vec::new();
    }
    let mut resets = Vec::new();
    for state in states {
        resets.extend(card::show(ui, state, now, mode));
        ui.add_space(CARD_GAP);
    }
    resets
}

/// Draws all cards with a right-click `menu`; returns true if a reset time was clicked.
pub fn draw_with_menu(
    ui: &mut egui::Ui,
    states: &[ProviderState],
    now: SystemTime,
    mode: TimeMode,
    mut menu: impl FnMut(&mut egui::Ui),
) -> bool {
    let background = ui.interact(ui.max_rect(), egui::Id::new("background"), Sense::click());
    let resets = draw_body(ui, states, now, mode);
    // Reset labels sense clicks, so they win the hit test: give them the menu too.
    for response in resets.iter().chain(std::iter::once(&background)) {
        response.context_menu(|ui| menu(ui));
    }
    resets.iter().any(egui::Response::clicked)
}

pub struct MonitorApp {
    state: SharedState,
    time_mode: TimeMode,
    always_on_top: bool,
    config_dir: PathBuf,
    last_height: f32,
}

impl MonitorApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        state: SharedState,
        always_on_top: bool,
        config_dir: PathBuf,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        let time_mode = cc
            .storage
            .and_then(|s| eframe::get_value(s, TIME_MODE_KEY))
            .unwrap_or_default();
        let always_on_top = cc
            .storage
            .and_then(|s| eframe::get_value(s, ALWAYS_ON_TOP_KEY))
            .unwrap_or(always_on_top);
        if always_on_top {
            cc.egui_ctx
                .send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
        }
        Self {
            state,
            time_mode,
            always_on_top,
            config_dir,
            last_height: 0.0,
        }
    }

    fn toggle_time_mode(&mut self) {
        self.time_mode = match self.time_mode {
            TimeMode::Countdown => TimeMode::Absolute,
            TimeMode::Absolute => TimeMode::Countdown,
        };
    }

    fn context_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.always_on_top, "Always on top")
            .changed()
        {
            let level = if self.always_on_top {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            };
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::WindowLevel(level));
        }
        if ui.button("Toggle countdown / reset time").clicked() {
            self.toggle_time_mode();
        }
        if ui.button("Open config folder").clicked() {
            open_folder(&self.config_dir);
        }
        ui.separator();
        ui.label(format!("AI Usage Monitor v{}", env!("CARGO_PKG_VERSION")));
        ui.label("MIT licence");
    }

    /// Resizes the native window height to fit the cards.
    fn fit_height(&mut self, ctx: &egui::Context, content_height: f32) {
        let wanted = content_height + WINDOW_PADDING;
        if (wanted - self.last_height).abs() > 1.0 {
            self.last_height = wanted;
            let width = ctx.content_rect().width();
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(width, wanted)));
        }
    }
}

fn open_folder(dir: &Path) {
    let target = if dir.exists() {
        dir
    } else {
        dir.parent().unwrap_or(dir)
    };
    let program = if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(target).spawn() {
        log::warn!("cannot open config folder: {e}");
    }
}

impl eframe::App for MonitorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(REPAINT_EVERY);
        let states = self.state.read().unwrap_or_else(|p| p.into_inner()).clone();
        let mode = self.time_mode;
        let inner = egui::Frame::central_panel(ui.style())
            .fill(theme::BG)
            .show(ui, |ui| {
                draw_with_menu(ui, &states, SystemTime::now(), mode, |ui| {
                    self.context_menu(ui)
                })
            });
        if inner.inner {
            self.toggle_time_mode();
        }
        self.fit_height(&ctx, inner.response.rect.height());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, TIME_MODE_KEY, &self.time_mode);
        eframe::set_value(storage, ALWAYS_ON_TOP_KEY, &self.always_on_top);
    }
}
