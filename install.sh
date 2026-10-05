#!/bin/sh
# Installs AI Usage Monitor for the current user on Linux: the binary, a menu entry
# and the icon, optionally a login autostart entry. Run it from the extracted release
# archive or from a source checkout after `cargo build --release`.
#
#   ./install.sh               install or update
#   ./install.sh --autostart   also start on login
#   ./install.sh --uninstall   remove everything this script installed
#
# Configuration (~/.config/ai-usage-monitor) and cache (~/.cache/ai-usage-monitor) are
# never touched.
set -eu

APP=ai-usage-monitor
BIN_DIR="$HOME/.local/bin"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}"
DESKTOP="$DATA_DIR/applications/$APP.desktop"
ICON="$DATA_DIR/icons/hicolor/256x256/apps/$APP.png"
AUTOSTART="$CONFIG_DIR/autostart/$APP.desktop"

here=$(cd "$(dirname "$0")" && pwd)

die() {
    echo "install.sh: $*" >&2
    exit 1
}

# First existing file among the arguments.
pick() {
    for f in "$@"; do
        if [ -f "$f" ]; then
            echo "$f"
            return
        fi
    done
}

refresh_menu() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$DATA_DIR/applications" 2>/dev/null || true
    fi
}

uninstall() {
    rm -f "$BIN_DIR/$APP" "$DESKTOP" "$ICON" "$AUTOSTART"
    refresh_menu
    echo "Removed $APP. Configuration and cache were left in place."
}

install_app() {
    autostart=$1
    [ "$(uname -s)" = Linux ] || die "this script is for Linux; see README for other systems"
    bin=$(pick "$here/$APP" "$here/target/release/$APP")
    [ -n "$bin" ] || die "no $APP binary next to this script or in target/release (run: cargo build --release)"
    entry=$(pick "$here/$APP.desktop" "$here/assets/$APP.desktop")
    [ -n "$entry" ] || die "missing $APP.desktop"
    icon=$(pick "$here/icon.png" "$here/assets/icon.png")
    [ -n "$icon" ] || die "missing icon.png"

    install -Dm755 "$bin" "$BIN_DIR/$APP"
    install -Dm644 "$icon" "$ICON"
    # Absolute Exec: the desktop session's PATH may not include ~/.local/bin.
    mkdir -p "$(dirname "$DESKTOP")"
    sed "s|^Exec=.*|Exec=$BIN_DIR/$APP|" "$entry" >"$DESKTOP"
    chmod 644 "$DESKTOP"
    if [ "$autostart" = yes ]; then
        install -Dm644 "$DESKTOP" "$AUTOSTART"
    fi
    refresh_menu

    echo "Installed $APP to $BIN_DIR/$APP"
    echo "Menu entry: $DESKTOP"
    if [ "$autostart" = yes ]; then
        echo "Starts on login: $AUTOSTART"
    elif [ -f "$AUTOSTART" ]; then
        echo "Still starts on login ($AUTOSTART); run with --uninstall to remove it"
    fi
    # Linux shortens process names to 15 characters.
    if pgrep -x "$(printf '%.15s' "$APP")" >/dev/null 2>&1; then
        echo "$APP is running; restart it to use the new version."
    fi
}

autostart=no
action=install
for arg in "$@"; do
    case "$arg" in
    --autostart) autostart=yes ;;
    --uninstall) action=uninstall ;;
    -h | --help)
        sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *) die "unknown option: $arg (try --help)" ;;
    esac
done

if [ "$action" = uninstall ]; then
    uninstall
else
    install_app "$autostart"
fi
