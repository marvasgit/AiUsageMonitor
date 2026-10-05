use aum::model::{ProviderError, ProviderSnapshot, ProviderState, Session, Window};
use aum::ui::app::{draw_body, draw_with_menu};
use aum::ui::card::TimeMode;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use std::time::{Duration, SystemTime};

fn states() -> Vec<ProviderState> {
    let now = SystemTime::now();
    let mut claude = ProviderState::new("claude", "Claude");
    let mut snap = ProviderSnapshot::new(vec![
        Window::new("5h", 15.0, Some(now + Duration::from_secs(4 * 3600))),
        Window::new("7d", 91.0, Some(now + Duration::from_secs(3 * 86_400))),
    ]);
    snap.plan = Some("max".into());
    snap.source = Some("omo · live".into());
    snap.session = Some(Session {
        model: "claude-sonnet-5".into(),
        effort: Some("medium".into()),
        input: 180,
        output: 31_800,
        cache_create: 1_000,
        cache_read: 4_970_000,
        requests: 90,
        context_tokens: 72_200,
        context_limit: 200_000,
    });
    claude.apply(Ok(snap));

    let mut openai = ProviderState::new("openai", "OpenAI");
    let mut o = ProviderSnapshot::new(vec![Window::new("5h", 31.0, None)]);
    o.note = Some("as of 10:06 (local)".into());
    openai.apply(Ok(o));

    let mut minimax = ProviderState::new("minimax", "MiniMax");
    minimax.apply(Err(ProviderError::Auth));
    vec![claude, openai, minimax]
}

#[test]
fn renders_all_provider_cards() {
    let states = states();
    let mut harness = Harness::new_ui(|ui| {
        draw_body(ui, &states, SystemTime::now(), TimeMode::Countdown);
    });
    harness.run();
    harness.get_by_label_contains("CLAUDE");
    harness.get_by_label_contains("OPENAI");
    harness.get_by_label_contains("MINIMAX");
    harness.get_by_label_contains("claude-sonnet-5 (medium)");
    harness.get_by_label("omo · live");
    harness.get_by_label_contains("72.2K / 200.0K");
    harness.get_by_label_contains("as of 10:06 (local)");
    harness.get_by_label_contains("authentication rejected");
}

#[test]
fn renders_empty_state_message() {
    let mut harness = Harness::new_ui(|ui| {
        draw_body(ui, &[], SystemTime::now(), TimeMode::Absolute);
    });
    harness.run();
    harness.get_by_label_contains("No providers detected");
    harness.get_by_label_contains("any folder in your home");
    harness.get_by_label_contains("Copilot");
}

fn right_click_opens_menu(target: &str) {
    let states = states();
    let mut harness = Harness::new_ui(|ui| {
        aum::ui::theme::apply(ui.ctx());
        draw_with_menu(ui, &states, SystemTime::now(), TimeMode::Countdown, |ui| {
            ui.label("MENU-OPEN");
        });
    });
    harness.run();
    harness.get_by_label(target).click_secondary();
    harness.run();
    assert!(
        harness.query_by_label("MENU-OPEN").is_some(),
        "right-click on {target:?} did not open the context menu"
    );
}

#[test]
fn right_click_on_card_header_opens_menu() {
    right_click_opens_menu("CLAUDE");
}

#[test]
fn right_click_on_reset_time_opens_menu() {
    right_click_opens_menu("--");
}

#[test]
fn renders_one_card_per_account() {
    let mut first = ProviderState::new("claude", "Claude");
    first.apply(Ok(ProviderSnapshot::new(vec![Window::new(
        "5h", 10.0, None,
    )])));
    let mut second = ProviderState::new("claude:personal", "Claude · personal");
    second.apply(Ok(ProviderSnapshot::new(vec![Window::new(
        "5h", 20.0, None,
    )])));
    let states = vec![first, second];
    let mut harness = Harness::new_ui(|ui| {
        draw_body(ui, &states, SystemTime::now(), TimeMode::Countdown);
    });
    harness.run();
    harness.get_by_label("CLAUDE");
    harness.get_by_label("CLAUDE · PERSONAL");
}
