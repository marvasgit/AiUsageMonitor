# AI Usage Monitor

**Version 2.0.0**

A small native desktop app for **Windows and Linux** that shows how much of your AI subscription you have used, at a glance. It supports **Claude** (Claude Code), **OpenAI** (ChatGPT plan used through the Codex CLI), **GitHub Copilot**, **Cursor** and **MiniMax** (Coding Plan), and is built so more providers can be added with one source file each.

One window, one card per provider (and one per account if you use several Claude or Codex logins), each with its rate-limit windows, a usage bar and a reset countdown.

## What it shows

| Provider | Windows | Extra |
|---|---|---|
| Claude | 5-hour, 7-day | Current Claude Code session: model, effort, input/output/cache tokens, context-window usage |
| OpenAI | 5-hour, weekly | Plan (e.g. `plus`) |
| GitHub Copilot | Monthly Premium requests or AI credits; Chat and Completions when limited | Plan |
| Cursor | Plan and API usage of the billing cycle | Plan, on-demand spend |
| MiniMax | Interval (5-hour), weekly | Plan |

Bars turn amber at 60 % and red at 85 %. Click a reset time to switch between a countdown and the local reset time. A coloured dot next to each provider shows its status: green (ok), grey (loading), amber (network or unexpected response), red (credentials missing or rejected). The error text is shown under the provider name.

## How it works

The monitor finds providers automatically by looking for the credentials their CLIs already store. It only **reads** those files; it never writes to them and never stores its own copy.

- **Claude** — reads the OAuth token from `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`) and calls Anthropic's usage endpoint (`api.anthropic.com/api/oauth/usage`) every 60 s. The session block is computed from the newest Claude Code log under `~/.claude/projects/`. Only token counts, model and effort are read; conversation content is ignored.
- **OpenAI** — reads the ChatGPT token from `~/.codex/auth.json` (or `$CODEX_HOME`) and calls the ChatGPT usage endpoint (`chatgpt.com/backend-api/wham/usage`) every 60 s. If that fails, it falls back to the rate limits Codex last wrote into its session logs under `~/.codex/sessions/` and marks the card "as of HH:MM (local)".
- **GitHub Copilot** — uses the first GitHub login it finds: `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN`, the Copilot CLI entry (`copilot-cli`) in the OS keychain, copilot.vim/lua's `github-copilot/apps.json`, or the `gh` CLI login (`hosts.yml`, or its `gh:github.com` keychain entry). Classic `ghp_` tokens in the environment are skipped because Copilot does not accept them. A GitHub login without a Copilot subscription shows "no Copilot subscription"; set `[copilot] enabled = false` to hide the card. It calls GitHub's internal Copilot usage endpoint (`api.github.com/copilot_internal/user`) every 5 minutes.
- **Cursor** — uses the `cursor-agent` login (`~/.config/cursor/auth.json`, Windows `%APPDATA%\Cursor\auth.json`, or `$CURSOR_CLI_AUTH_FILE`) or the Cursor app's local database (opened read-only), and calls Cursor's usage summary (`cursor.com/api/usage-summary`) every 5 minutes. Cursor logins expire; when that happens the card turns red until you open Cursor (or run `cursor-agent`) again.
- **MiniMax** — uses `$MINIMAX_API_KEY` or the key stored by the `mmx` CLI in `~/.mmx/config.json`, and calls the Coding Plan endpoint (`api.minimax.io/v1/api/openplatform/coding_plan/remains`, or `api.minimaxi.com` for the `cn` region) every 120 s.

The Copilot and Cursor endpoints are internal to those services and may change without notice.

**Multiple accounts.** Claude Code and Codex keep each login in its own config folder (selected with `CLAUDE_CONFIG_DIR` / `CODEX_HOME`). The monitor shows `~/.claude` and `~/.codex` plus every folder directly in your home that holds valid credentials, whatever it is called. Each gets its own card, named after the folder: `~/.claude-personal` → **Claude · personal**, `~/.codex_work` → **OpenAI · work**. Folders elsewhere can be added in the configuration.

Unlike version 1.0, the monitor no longer installs or modifies a Claude Code `statusLine`.

## Install

**Windows:** download `ai-usage-monitor-windows-x86_64.zip` from the releases page, unzip, and run `ai-usage-monitor.exe`. The executable is not signed, so SmartScreen may warn on first run.

**Linux:** download `ai-usage-monitor-linux-x86_64.tar.gz`, extract it, then:

```sh
install -Dm755 ai-usage-monitor ~/.local/bin/ai-usage-monitor
install -Dm644 ai-usage-monitor.desktop ~/.local/share/applications/ai-usage-monitor.desktop
install -Dm644 icon.png ~/.local/share/icons/hicolor/256x256/apps/ai-usage-monitor.png
```

To start it on login:

```sh
mkdir -p ~/.config/autostart
cp ~/.local/share/applications/ai-usage-monitor.desktop ~/.config/autostart/
```

## Configuration

No configuration is needed. To change behaviour, create `config.toml` at:

- Linux: `~/.config/ai-usage-monitor/config.toml`
- Windows: `%APPDATA%\ai-usage-monitor\config.toml`

Every key is optional (see `config.example.toml`):

```toml
always_on_top = false
order = ["claude", "openai", "copilot", "cursor", "minimax"]

[claude]
enabled = true
# context_limit = 200000
# hide = ["personal"]     # account names to hide; "default" = ~/.claude
# [[claude.accounts]]     # extra account folder (outside home or nested deeper)
# name = "work"
# dir = "/mnt/other/.claude"

[openai]
enabled = true
live_poll = true          # false = only use local Codex logs
# hide = []
# [[openai.accounts]]
# name = "work"
# dir = "~/projects/.codex-work"

[copilot]
enabled = true

[cursor]
enabled = true

[minimax]
enabled = true
api_key_env = "MINIMAX_API_KEY"
# region = "global"      # or "cn"
models = ["general"]      # add "video" etc. to show more MiniMax quotas
```

On Windows write folder paths with forward slashes (`"D:/other/.claude"`) or in single quotes (`'D:\other\.claude'`); backslashes inside double quotes make the file invalid. An `[[...accounts]]` entry pointing at a folder that was already found automatically just renames it. API keys are never read from this file — only from an environment variable or the CLI's own credential file. Unknown keys are reported in the log and ignored; an invalid file falls back to defaults. Right-click the window for always-on-top, the time format toggle and a shortcut to the config folder.

## Privacy & security

- No telemetry, no account, no cloud service of its own.
- Credentials are read when each poll runs, held in memory that is wiped after use, and never logged, displayed, cached or written.
- Logs (enable with `RUST_LOG`) contain only provider names, HTTP status codes and error kinds.
- All provider requests use HTTPS with a 15 s timeout; responses are capped at 2 MB.
- Conversation contents are never read for display or sent anywhere.
- For GitHub Copilot the app reads the `copilot-cli` and `gh:github.com` entries from the OS keychain (Secret Service on Linux, Credential Manager on Windows). A locked keychain is skipped; the app never asks you to unlock it.
- For Cursor the app opens Cursor's local settings database read-only and never modifies it.

## Building from source

Requires Rust 1.95 or newer.

```sh
cargo build --release
```

On Linux, install the GUI development packages first (Debian/Ubuntu names):

```sh
sudo apt-get install libxkbcommon-dev libgl1-mesa-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev
```

Run the tests with `cargo test`.

## Adding a provider

1. Create `src/providers/<name>.rs` implementing the `Provider` trait (`id`, `display_name`, `poll_interval`, `poll`) and a `detect(&Config, &Paths) -> Option<Self>` constructor.
2. Add a `[<name>]` section to `Config` (at least `enabled`).
3. Add one line to `registry::all_providers`.
4. Add fixtures and tests.

The UI renders any provider's windows automatically; no UI change is needed.

## Troubleshooting

- **A provider is missing** — check that its credential file exists (see *How it works*) or that the environment variable is set, then restart the monitor.
- **Amber dot** — network problem or an unexpected response; the text under the provider name says which. The monitor retries with backoff (up to 15 minutes).
- **Red dot** — credentials missing or rejected; log in again with that provider's CLI.
- **Logs** — run `RUST_LOG=debug ai-usage-monitor` from a terminal.

## Credits & licence

Released under the [MIT License](LICENSE). This project is not affiliated with Anthropic, OpenAI or MiniMax. Claude, OpenAI, ChatGPT, Codex and MiniMax are trademarks of their respective owners; the MIT licence grants no rights to those services or marks.
