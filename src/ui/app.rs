//! Top-level eframe app: stacked provider cards plus a context menu.

use super::card::{self, TimeMode};
use super::theme;
use crate::config::ContextConfig;
use crate::model::ProviderState;
use crate::poller::SharedState;
use crate::update::{self, Status, Updater};
use egui::{RichText, Sense, ViewportCommand, WindowLevel};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const TIME_MODE_KEY: &str = "time_mode";
const ALWAYS_ON_TOP_KEY: &str = "always_on_top";
const REPAINT_EVERY: Duration = Duration::from_secs(1);
const CARD_GAP: f32 = 6.0;
const WINDOW_PADDING: f32 = 24.0;
const NO_PROVIDERS: &str = "No providers detected.\n\nLooked for:\n  Claude Code: .credentials.json (macOS: keychain) in ~/.claude or any folder in your home\n  Codex CLI: auth.json in ~/.codex or any folder in your home\n  GitHub Copilot: Copilot CLI, copilot.vim or gh login\n  Cursor: Cursor app or cursor-agent login\n  MiniMax: ~/.mmx/config.json or $MINIMAX_API_KEY\n  OpenRouter: $OPENROUTER_API_KEY or $OPENROUTER_MANAGEMENT_KEY";

/// Draws all cards; returns the responses of every clickable reset-time label.
pub fn draw_body(
    ui: &mut egui::Ui,
    states: &[ProviderState],
    now: SystemTime,
    mode: TimeMode,
    context: &ContextConfig,
) -> Vec<egui::Response> {
    if states.is_empty() {
        ui.label(RichText::new(NO_PROVIDERS).color(theme::MUTED));
        return Vec::new();
    }
    let mut resets = Vec::new();
    for state in states {
        resets.extend(card::show(ui, state, now, mode, context));
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
    context: &ContextConfig,
    mut menu: impl FnMut(&mut egui::Ui),
) -> bool {
    let background = ui.interact(ui.max_rect(), egui::Id::new("background"), Sense::click());
    let resets = draw_body(ui, states, now, mode, context);
    // Reset labels sense clicks, so they win the hit test: give them the menu too.
    for response in resets.iter().chain(std::iter::once(&background)) {
        response.context_menu(|ui| menu(ui));
    }
    resets.iter().any(egui::Response::clicked)
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateAction {
    Install,
    OpenPage(String),
    Restart,
}

/// A clickable line about a found or installed update; returns the action clicked.
pub fn draw_update_banner(ui: &mut egui::Ui, status: &Status) -> Option<UpdateAction> {
    let (text, action) = match status {
        Status::Available(r) if r.binary.is_some() => (
            format!("Update to v{} available - click to install", r.version),
            Some(UpdateAction::Install),
        ),
        Status::Available(r) => (
            format!("v{} available - click to open the download page", r.version),
            Some(UpdateAction::OpenPage(r.page.clone())),
        ),
        Status::Installing(r) => (format!("Installing v{}...", r.version), None),
        Status::Installed(r) => (
            format!("v{} installed - click to restart", r.version),
            Some(UpdateAction::Restart),
        ),
        Status::Idle | Status::Checking | Status::UpToDate | Status::Failed(_) => return None,
    };
    let clicked = if action.is_some() {
        ui.button(RichText::new(text).color(theme::HEADER))
            .clicked()
    } else {
        ui.label(RichText::new(text).color(theme::MUTED));
        false
    };
    action.filter(|_| clicked)
}

fn update_status_line(status: &Status) -> String {
    match status {
        Status::Idle => "Updates not checked yet".into(),
        Status::Checking => "Checking for updates...".into(),
        Status::UpToDate => "Up to date".into(),
        Status::Available(r) => format!("v{} available", r.version),
        Status::Installing(r) => format!("Installing v{}...", r.version),
        Status::Installed(r) => format!("v{} installed; restart to use it", r.version),
        Status::Failed(e) => format!("Update failed: {e}"),
    }
}

pub struct MonitorApp {
    state: SharedState,
    updater: Updater,
    time_mode: TimeMode,
    always_on_top: bool,
    config_dir: PathBuf,
    context: ContextConfig,
    last_height: f32,
}

impl MonitorApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        state: SharedState,
        always_on_top: bool,
        config_dir: PathBuf,
        context: ContextConfig,
        updater: Updater,
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
            updater,
            time_mode,
            always_on_top,
            config_dir,
            context,
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
        ui.label(format!("AI Usage Monitor v{}", update::CURRENT));
        let status = self.updater.status();
        ui.label(RichText::new(update_status_line(&status)).color(theme::MUTED));
        if !matches!(status, Status::Installing(_) | Status::Installed(_))
            && ui.button("Check for updates").clicked()
        {
            self.updater.check_now();
        }
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

    fn run_update_action(&self, ctx: &egui::Context, action: UpdateAction) {
        match action {
            UpdateAction::Install => self.updater.install(),
            UpdateAction::OpenPage(url) => open(OsStr::new(&url)),
            UpdateAction::Restart => match self.updater.restart() {
                Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Err(e) => log::warn!("cannot restart: {e}"),
            },
        }
    }
}

fn open_folder(dir: &Path) {
    let target = if dir.exists() {
        dir
    } else {
        dir.parent().unwrap_or(dir)
    };
    open(target.as_os_str());
}

/// Opens a folder or URL with the desktop's default handler.
fn open(target: &OsStr) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(target).spawn() {
        log::warn!("cannot open {}: {e}", target.to_string_lossy());
    }
}

impl eframe::App for MonitorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(REPAINT_EVERY);
        let states = self.state.read().unwrap_or_else(|p| p.into_inner()).clone();
        let mode = self.time_mode;
        let update_status = self.updater.status();
        let inner = egui::Frame::central_panel(ui.style())
            .fill(theme::BG)
            .show(ui, |ui| {
                let context = self.context.clone();
                let reset_clicked =
                    draw_with_menu(ui, &states, SystemTime::now(), mode, &context, |ui| {
                        self.context_menu(ui)
                    });
                // Drawn after the cards so it sits above the menu's background hit area.
                (reset_clicked, draw_update_banner(ui, &update_status))
            });
        let (reset_clicked, update_action) = inner.inner;
        if reset_clicked {
            self.toggle_time_mode();
        }
        if let Some(action) = update_action {
            self.run_update_action(&ctx, action);
        }
        self.fit_height(&ctx, inner.response.rect.height());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, TIME_MODE_KEY, &self.time_mode);
        eframe::set_value(storage, ALWAYS_ON_TOP_KEY, &self.always_on_top);
    }
}
