# AI Usage Monitor

**Version 2.0.3**

A small native desktop app for **Windows, Linux and macOS** that shows how much of your AI subscription you have used, at a glance. It supports **Claude** (Claude Code), **OpenAI** (ChatGPT plan used through the Codex CLI), **GitHub Copilot**, **Cursor** and **MiniMax** (Coding Plan), and is built so more providers can be added with one source file each.

One window, one card per provider (and one per account if you use several Claude or Codex logins), each with its rate-limit windows, a usage bar and a reset countdown.

When a window first passes 80 % and again at 95 %, a desktop notification says so, once per window until it resets (thresholds are configurable under `[notifications]`).

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

- **Claude** — reads the OAuth token from `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`) and calls Anthropic's usage endpoint (`api.anthropic.com/api/oauth/usage`) every 120 s (`[claude] poll_seconds`, at least 60). On macOS, Claude Code keeps the token in the login keychain instead of that file: the monitor reads the `Claude Code-credentials` item for `~/.claude`, and `Claude Code-credentials-<hash>` for other folders (first 8 hex digits of SHA-256 of the folder path). Only Pro/Max logins (`claudeAiOauth`) have usage data; API-key logins show no card. Claude Code uses the same endpoint with the same token, so polling too often gets it rate-limited. The last reading is kept in the cache folder (`~/.cache/ai-usage-monitor`, Windows `%LOCALAPPDATA%\ai-usage-monitor`, macOS `~/Library/Caches/ai-usage-monitor`) so a restart shows it at once. The session block is computed from the newest Claude Code log under `~/.claude/projects/`. Only token counts, model and effort are read; conversation content is ignored.
- **Claude logins of other coding agents (omo, omp)** — usage limits belong to the Claude account, not to the tool, so these logins add tokens to the Claude card instead of new cards. The monitor reads omo's credential file (`[omo] auth`, default `~/.omo/agent/auth.json`; the first unexpired account under `anthropic-subscription.accounts`) and omp's database (`[omp] db`, default `~/.omp/agent/agent.db`, opened read-only; the newest enabled `anthropic` OAuth row). At startup it asks Anthropic's profile endpoint (`/api/oauth/profile`) which account each token belongs to; when that call fails, omp's saved account id is used. A login of the same account as a Claude Code card joins that card; otherwise it gets its own card, named "Claude" when there is no Claude Code card, else "Claude · omo" / "Claude · omp". A card asks its tokens in order (Claude Code, omo, omp) and shows the first answer. Each token has its own backoff, so a 429 on one does not stop the others. When every token fails, an omp token adds the usage omp last saved (`usage_history`, including extra limits such as "7d fable") as a fallback. omo and omp refresh their tokens; the monitor never does. Turn either off with `[omo] enabled = false` / `[omp] enabled = false`. Their file layouts are internal and may change.

  Next to the plan, the Claude card shows the account and where its numbers came from: the account's email, masked (only the first two letters of the name and of the domain, plus the top-level domain: `of…@ex….com`), the login that answered (**CC** = Claude Code, **omo**, **omp**) and how. **live** means fetched now. **cached** means the card's last reading, reused within half the poll interval or kept while all tokens fail; a reading loaded from the cache file at startup shows just "cached". **omp saved** means omp's own last reading. Example: `max · of…@ex….com · omo · live`. The email and, when Claude Code gives no plan, the plan (max or pro) come from the profile endpoint, asked once per card at startup or on its first poll. The email is shown only in this masked form and never logged or stored.
- **OpenAI** — reads the ChatGPT token from `~/.codex/auth.json` (or `$CODEX_HOME`) and calls the ChatGPT usage endpoint (`chatgpt.com/backend-api/wham/usage`) every 60 s. On macOS, when Codex keeps its login in the keychain (`cli_auth_credentials_store = "keyring"` or `"auto"`), the monitor reads the `Codex Auth` item whose account is `cli|<first 16 hex digits of SHA-256 of the Codex folder path>`. If the live call fails, it falls back to the rate limits Codex last wrote into its session logs under `~/.codex/sessions/` and marks the card "as of HH:MM (local)".
- **GitHub Copilot** — uses the first GitHub login it finds: `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN`, the Copilot CLI entry (`copilot-cli`) in the OS keychain, copilot.vim/lua's `github-copilot/apps.json`, or the `gh` CLI login (`hosts.yml`, or its `gh:github.com` keychain entry). Classic `ghp_` tokens in the environment are skipped because Copilot does not accept them. A GitHub login without a Copilot subscription shows "no Copilot subscription"; set `[copilot] enabled = false` to hide the card. It calls GitHub's internal Copilot usage endpoint (`api.github.com/copilot_internal/user`) every 5 minutes.
- **Cursor** — uses the `cursor-agent` login (`~/.config/cursor/auth.json`, Windows `%APPDATA%\Cursor\auth.json`, or `$CURSOR_CLI_AUTH_FILE`) or the Cursor app's local database (opened read-only), and calls Cursor's usage summary (`cursor.com/api/usage-summary`) every 5 minutes. Cursor logins expire; when that happens the card turns red until you open Cursor (or run `cursor-agent`) again.
- **MiniMax** — uses `$MINIMAX_API_KEY` or the key stored by the `mmx` CLI in `~/.mmx/config.json`, and calls the Coding Plan endpoint (`api.minimax.io/v1/api/openplatform/coding_plan/remains`, or `api.minimaxi.com` for the `cn` region) every 120 s.
- **OpenRouter** — uses `$OPENROUTER_API_KEY` (`[openrouter] api_key_env`) and calls `openrouter.ai/api/v1/key` every 5 minutes. The card shows the dollars the key spent today, this week and this month. These are calendar periods in UTC, as OpenRouter counts them (the week starts on Monday), not rolling 1, 7 and 30 days. Each row's reset time is the end of its period. Without a credit limit there are no bars. With a limit, the period the limit resets on gets a bar for its share of the limit (a limit that never resets adds a "total" row), and the note shows what is left.

The Copilot and Cursor endpoints are internal to those services and may change without notice.

**Multiple accounts.** Claude Code and Codex keep each login in its own config folder (selected with `CLAUDE_CONFIG_DIR` / `CODEX_HOME`). The monitor shows `~/.claude` and `~/.codex` plus every folder directly in your home that holds valid credentials, whatever it is called. Each gets its own card, named after the folder: `~/.claude-personal` → **Claude · personal**, `~/.codex_work` → **OpenAI · work**. Names do not depend on `CLAUDE_CONFIG_DIR` / `CODEX_HOME`, so starting the monitor from a shell that has them set changes nothing. Folders elsewhere can be added in the configuration.

**Rate limits and outages.** When a usage endpoint fails or answers HTTP 429, the card keeps the last numbers with a note ("usage as of 14:05 · rate limited") and the monitor waits longer between calls: 2, 4, 8 minutes up to 15, or as long as the server's `Retry-After` asks. The Claude session block keeps updating meanwhile, since it is read from local files.

Unlike version 1.0, the monitor no longer installs or modifies a Claude Code `statusLine`.

## Install

**Windows:** download `ai-usage-monitor-windows-x86_64.zip` from the releases page, unzip, and run `ai-usage-monitor.exe`. The executable is not signed, so SmartScreen may warn on first run.

**Linux:** download `ai-usage-monitor-linux-x86_64.tar.gz`, extract it, then run the installer from the extracted folder:

```sh
./install.sh              # binary in ~/.local/bin, menu entry and icon
./install.sh --autostart  # the same, and start on login
./install.sh --uninstall  # remove it again (configuration and cache are kept)
```

Run it again to update. It also works from a source checkout after `cargo build --release`. Start the monitor from the menu rather than from a terminal: a terminal may carry variables such as `CLAUDE_CONFIG_DIR` meant for one CLI session.

**macOS:** there is no prebuilt release. Build from source (see *Building from source*), then:

```sh
mkdir -p ~/.local/bin
install -m755 target/release/ai-usage-monitor ~/.local/bin/ai-usage-monitor
```

Run it from a terminal with `ai-usage-monitor`. To start it on login, add it under System Settings → General → Login Items.

## Configuration

No configuration is needed. To change behaviour, create `config.toml` at:

- Linux: `~/.config/ai-usage-monitor/config.toml`
- Windows: `%APPDATA%\ai-usage-monitor\config.toml`
- macOS: `~/Library/Application Support/ai-usage-monitor/config.toml`

Every key is optional (see `config.example.toml`):

```toml
always_on_top = false
order = ["claude", "openai", "copilot", "cursor", "minimax", "openrouter"]

[claude]
enabled = true
poll_seconds = 120        # minimum 60; the token is shared with Claude Code
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

[openrouter]
enabled = true
api_key_env = "OPENROUTER_API_KEY"

[omp]                     # omp's Claude login (also off when [claude] is off)
enabled = true
db = "~/.omp/agent/agent.db"

[omo]                     # omo's Claude login (also off when [claude] is off)
enabled = true
auth = "~/.omo/agent/auth.json"

[notifications]           # desktop alert when a window first crosses each threshold
enabled = true
thresholds = [80, 95]     # percent
```

On Windows write folder paths with forward slashes (`"D:/other/.claude"`) or in single quotes (`'D:\other\.claude'`); backslashes inside double quotes make the file invalid. An `[[...accounts]]` entry pointing at a folder that was already found automatically just renames it. API keys are never read from this file — only from an environment variable or the CLI's own credential file. Unknown keys are reported in the log and ignored; an invalid file falls back to defaults. Right-click the window for always-on-top, the time format toggle and a shortcut to the config folder.

## Privacy & security

- No telemetry, no account, no cloud service of its own.
- Credentials are read when each poll runs, held in memory that is wiped after use, and never logged, displayed, cached or written. The usage cache holds only percentages and reset times.
- Logs (enable with `RUST_LOG`) contain only provider names, HTTP status codes and error kinds.
- All provider requests use HTTPS with a 15 s timeout; responses are capped at 2 MB.
- Conversation contents are never read for display or sent anywhere.
- For GitHub Copilot the app reads the `copilot-cli` and `gh:github.com` entries from the OS keychain (Secret Service on Linux, Credential Manager on Windows, login keychain on macOS). On macOS it also reads Claude Code's `Claude Code-credentials` items and Codex's `Codex Auth` items. A locked keychain is skipped; the app never asks you to unlock it. On macOS it uses `/usr/bin/security`. Claude Code and `gh` store their items with that tool, so no access prompt appears for them. Codex stores its item itself, so macOS may ask once whether `security` may read `Codex Auth`; choose **Always Allow**.
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

On macOS, install the Xcode Command Line Tools (`xcode-select --install`) and Rust (`brew install rust` or rustup). No other packages are needed.

Run the tests with `cargo test`.

## Releases

Versions follow [semantic versioning](https://semver.org): bug fixes raise the patch number (2.0.3 → 2.0.4), new features the minor number (2.0.3 → 2.1.0), and changes that break existing configuration the major number. To release, set the version in `Cargo.toml` and at the top of this README, commit, and push a tag `vX.Y.Z`: CI then builds the Linux and Windows packages and publishes them with a `SHA256SUMS` file on the releases page.

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
