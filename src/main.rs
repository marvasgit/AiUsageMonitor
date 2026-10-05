#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use aum::model::ProviderState;
use aum::poller::{self, SharedState};
use aum::ui::app::MonitorApp;
use std::sync::{Arc, RwLock};

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
const WIDTH: f32 = 440.0;
const INITIAL_HEIGHT: f32 = 320.0;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let Some(paths) = aum::paths::Paths::from_env() else {
        log::error!("cannot determine home directory");
        std::process::exit(1);
    };
    let cfg = aum::config::load_or_default(&paths.config_file());
    let providers = aum::registry::all_providers(&cfg, &paths);
    let state: SharedState = Arc::new(RwLock::new(
        providers
            .iter()
            .map(|p| ProviderState::new(p.id(), p.display_name()))
            .collect(),
    ));

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("AI Usage Monitor v{}", env!("CARGO_PKG_VERSION")))
        .with_inner_size([WIDTH, INITIAL_HEIGHT])
        .with_resizable(true);
    match eframe::icon_data::from_png_bytes(ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => log::warn!("icon decode failed: {e}"),
    }
    if cfg.always_on_top {
        viewport = viewport.with_always_on_top();
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ai-usage-monitor",
        options,
        Box::new(move |cc| {
            for (index, provider) in providers.into_iter().enumerate() {
                let ctx = cc.egui_ctx.clone();
                let tracker = cfg.notifications.tracker();
                poller::spawn(provider, index, state.clone(), tracker, move || {
                    ctx.request_repaint()
                });
            }
            Ok(Box::new(MonitorApp::new(
                cc,
                state,
                cfg.always_on_top,
                paths.config_dir.clone(),
            )))
        }),
    )
}
