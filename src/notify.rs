//! Desktop notifications (D-Bus on Linux, Notification Center on macOS, toasts on Windows).

use crate::alerts::Alert;

const APP_NAME: &str = "AI Usage Monitor";

/// Best effort: a desktop without a notification service only loses the alert.
pub fn send(alert: &Alert) {
    let result = notify_rust::Notification::new()
        .appname(APP_NAME)
        .summary(&alert.title)
        .body(&alert.body)
        .show();
    if let Err(e) = result {
        log::warn!("cannot show notification: {e}");
    }
}
