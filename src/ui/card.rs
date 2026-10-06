//! One provider card: header, optional session block, rate-limit windows.

use super::theme;
use crate::format::{fmt_absolute, fmt_countdown, fmt_tokens, pct_level};
use crate::model::{ProviderState, ProviderStatus, Session, Window};
use egui::{Color32, RichText, Sense, Vec2};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

const LABEL_WIDTH: f32 = 64.0;
const BAR_WIDTH: f32 = 180.0;
const BAR_HEIGHT: f32 = 14.0;
const DOT_RADIUS: f32 = 4.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TimeMode {
    #[default]
    Countdown,
    Absolute,
}

fn status_color(status: &ProviderStatus) -> Color32 {
    match status {
        ProviderStatus::Pending => theme::PENDING,
        ProviderStatus::Ok => theme::OK,
        ProviderStatus::Error(e) if e.is_transient() => theme::WARN,
        ProviderStatus::Error(_) => theme::CRIT,
    }
}

fn bar(ui: &mut egui::Ui, pct: f64) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(BAR_WIDTH, BAR_HEIGHT), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, theme::TRACK);
    let mut fill = rect;
    fill.set_width(rect.width() * (pct.clamp(0.0, 100.0) / 100.0) as f32);
    painter.rect_filled(fill, 2.0, theme::level_color(pct_level(pct)));
}

fn row_label(ui: &mut egui::Ui, text: &str) {
    ui.add_sized(
        [LABEL_WIDTH, BAR_HEIGHT],
        egui::Label::new(RichText::new(text).monospace()),
    );
}

fn reset_text(window: &Window, now: SystemTime, mode: TimeMode) -> String {
    match (window.resets_at, mode) {
        (None, _) => "--".to_string(),
        (Some(t), TimeMode::Countdown) => fmt_countdown(t, now),
        (Some(t), TimeMode::Absolute) => fmt_absolute(t, now),
    }
}

/// Returns the reset-time label's response (clickable to toggle the time mode).
fn window_row(
    ui: &mut egui::Ui,
    window: &Window,
    now: SystemTime,
    mode: TimeMode,
) -> egui::Response {
    ui.horizontal(|ui| {
        row_label(ui, &window.label);
        if window.bar {
            bar(ui, window.used_pct);
        } else {
            ui.allocate_exact_size(Vec2::new(BAR_WIDTH, BAR_HEIGHT), Sense::hover());
        }
        match &window.amount {
            Some(amount) => ui.monospace(format!("{amount:>6}")),
            None => ui.monospace(format!("{:>5.1}%", window.used_pct)),
        };
        let reset = RichText::new(reset_text(window, now, mode))
            .monospace()
            .color(theme::MUTED);
        ui.add(egui::Label::new(reset).sense(Sense::click()))
            .on_hover_text("Click to toggle countdown / reset time")
    })
    .inner
}

fn session_block(ui: &mut egui::Ui, s: &Session) {
    ui.horizontal(|ui| {
        row_label(ui, "ctx");
        bar(ui, s.context_pct());
        ui.monospace(format!("{:>5.1}%", s.context_pct()));
    });
    let context = format!(
        "{} / {} tokens",
        fmt_tokens(s.context_tokens),
        fmt_tokens(s.context_limit)
    );
    ui.label(RichText::new(context).monospace().color(theme::MUTED));
    let tokens = format!(
        "In {}  Out {}  Cache {}  Tot {}  Req {}",
        fmt_tokens(s.input),
        fmt_tokens(s.output),
        fmt_tokens(s.cache_create + s.cache_read),
        fmt_tokens(s.total()),
        s.requests
    );
    ui.label(RichText::new(tokens).monospace().color(theme::MUTED));
}

fn header(ui: &mut egui::Ui, state: &ProviderState, now: SystemTime) {
    let snap = state.last_ok.as_ref();
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(DOT_RADIUS * 2.0 + 2.0), Sense::hover());
        ui.painter()
            .circle_filled(dot.center(), DOT_RADIUS, status_color(&state.status));
        ui.label(
            RichText::new(state.name.to_uppercase())
                .strong()
                .size(16.0)
                .color(theme::HEADER),
        );
        if let Some(plan) = snap.and_then(|s| s.plan.as_deref()) {
            ui.label(RichText::new(plan).color(theme::MUTED));
        }
        if let Some(source) = snap.and_then(|s| s.source.as_deref()) {
            ui.label(RichText::new(source).small().color(theme::MUTED))
                .on_hover_text(
                    "Login that answered (CC = Claude Code) · how the numbers were obtained",
                );
        }
        if let Some(session) = snap.and_then(|s| s.session.as_ref()) {
            let model = match &session.effort {
                Some(effort) => format!("{} ({effort})", session.model),
                None => session.model.clone(),
            };
            ui.label(RichText::new(model).monospace());
        }
        if let Some(next) = state.next_poll {
            let secs = next.duration_since(now).map_or(0, |d| d.as_secs());
            ui.label(RichText::new("⏱").small().color(theme::MUTED))
                .on_hover_text(format!("Next check in {secs} s"));
        }
    });
    let mut notes = Vec::new();
    if let ProviderStatus::Error(e) = &state.status {
        notes.push(e.to_string());
    }
    if let Some(note) = snap.and_then(|s| s.note.as_deref()) {
        notes.push(note.to_string());
    }
    if !notes.is_empty() {
        ui.label(RichText::new(notes.join(" · ")).small().color(theme::MUTED));
    }
}

/// Draws one provider card; returns the responses of its clickable reset-time labels.
pub fn show(
    ui: &mut egui::Ui,
    state: &ProviderState,
    now: SystemTime,
    mode: TimeMode,
) -> Vec<egui::Response> {
    let mut resets = Vec::new();
    egui::Frame::NONE
        .fill(theme::PANEL)
        .corner_radius(6.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            header(ui, state, now);
            match &state.last_ok {
                Some(snap) => {
                    if let Some(session) = &snap.session {
                        session_block(ui, session);
                    }
                    for window in &snap.windows {
                        resets.push(window_row(ui, window, now, mode));
                    }
                }
                None if state.status == ProviderStatus::Pending => {
                    ui.label(RichText::new("loading…").color(theme::MUTED));
                }
                None => {}
            }
        });
    resets
}
