# Claude Code Command Deck

An [OpenAction](https://openaction.amankhanna.me) plugin for [OpenDeck](https://github.com/nekename/OpenDeck)
(and other Stream Deck–compatible software) that puts your Claude Code workflow
on your deck: plan usage with pace and threshold alerts, a session status key
that lights up when Claude needs you and jumps to that window, today's activity,
and a launcher. Keys and Stream Deck+ dials, with per-key colours.

## Actions

| Action | Shows | Tap | Hold | Dial |
|---|---|---|---|---|
| Plan Usage | Session (5 h) and weekly (7 d) usage, one of them, or the highest per-model weekly limit. A white tick marks where even spending would be ("on pace" / "+N% pace" / "−N% pace") | Next display mode | Refresh | Turn: mode · Push/touch: refresh |
| Usage Breakdown | This week's usage by surface (Claude Code, Chats, …) | Refresh | Refresh | |
| Extra Usage | Spending on usage credits beyond your plan, against its limit | Refresh | Refresh | |
| Session Status | Orange when a Claude Code session (terminal or VS Code) waits for permission or an answer; optionally blue while working and green when done | Jump to the waiting session, or clear finished ones | Open the latest session | Turn: pick a session · Push/touch: jump to it |
| Active Sessions | Running Claude Code sessions, and how many are busy | Refresh | Refresh | |
| Today's Activity | Tokens used today across your Claude Code sessions (no cost estimates) | Refresh | Refresh | |
| Launch Claude | A project folder | Open a terminal running `claude` there | | |

**Alerts:** each Plan Usage key can alert at two thresholds (default 80% and
95%; 0 turns one off). Crossing one flashes the key and sends a desktop
notification, once per window until it resets.

**Freshness:** usage keys show a small amber "16m ago" badge when their numbers
are getting old, and a red one ("rate limited", "offline", …) while refreshing
fails. When the usage API errors, the plugin backs off (doubling the wait up to
30 minutes, longer if the server asks with `Retry-After`); login problems are
shown straight away instead. The last response is kept on disk, so restarting
OpenDeck shows numbers immediately without an API call.

**Appearance:** every key has a background and accent colour (with an
**OLED black** preset); text switches between light and dark for contrast.

## Requirements

- [Claude Code](https://claude.com/claude-code), logged in with a Claude account
  (the plugin reads Claude Code's stored login; it has no login of its own).
- OpenDeck, or other software that runs Stream Deck SDK 2 plugins.

## Install

Download `com.mroneleg.claudeusage.streamDeckPlugin` from the
[latest release](https://github.com/mroneleg/claude-opendeck/releases/latest),
install it in OpenDeck (Plugins → Install from file), then restart OpenDeck.
(The bundle ID still says "claudeusage", from the plugin's first name, so
existing keys keep working across updates.)

### Session Status hooks

The Session Status key learns about sessions from Claude Code
[hooks](https://docs.claude.com/en/docs/claude-code/hooks). Install them from the
key's settings (**Install Claude Code hooks**), or from a terminal with the
plugin binary:

```sh
claude-opendeck-<platform> install-hooks     # or uninstall-hooks
```

This adds one hook command per event to Claude Code's `settings.json` (keeping
everything else, with a `settings.json.bak` backup). It's idempotent and
replaces hooks from an older install location. Running sessions pick the hooks
up after `/hooks` or a restart; in VS Code, open a new Claude tab or reload the
window. If you remove the plugin, run `uninstall-hooks` first.

What counts as what:

- **Needs you** (always lights up): a permission prompt, a question
  (`AskUserQuestion` lights it at once), or another prompt that blocks on you.
  Claude Code waits a few seconds before raising a permission notification, so
  prompts you answer straight away never light it.
- **Working** (optional): from your prompt until Claude stops. Pressing Esc
  doesn't tell hooks anything, so "working" with no activity for 30 minutes
  counts as idle.
- **Done** (optional): Claude finished and is waiting for your next prompt.
  Tap the key (with nothing waiting) to clear finished sessions; they stay
  cleared until they do something new.

### Status line (optional second usage source)

Claude Code passes your 5 h / 7 d usage to its
[status line](https://docs.claude.com/en/docs/claude-code/statusline) command
after each response. The plugin can use that too: click **Use Claude Code's
status line** in a usage key's settings, or run

```sh
claude-opendeck-<platform> install-statusline   # or uninstall-statusline
```

If you already have a status line command, it's wrapped, not replaced: it
still runs with the same input and its output is what you see (uninstalling
restores it exactly). Otherwise you get a short line like `Fable · 5h 12% · 7d 4%`.

Each usage key's **Data source** is then either:

- **Usage API + status line** (default): status line figures win when they're
  newer than the last API fetch, and fill in if the API is failing.
- **Status line only:** never reads your Claude Code login at all. If every
  usage key uses it, the plugin makes no API calls.

Trade-offs: the status line only carries the 5 h and 7 d windows (per-model
limits, the breakdown and extra usage need the API and show "needs API"), and
it only updates while a Claude Code session is running and getting responses.

## Platform support

| | Linux | Windows | macOS |
|---|---|---|---|
| Usage keys, alerts, status line, theming | ✅ tested | builds in CI, untested | builds in CI, untested |
| Session status (lights up), sessions, activity | ✅ tested | builds in CI, untested | builds in CI, untested |
| Session status (jump to window) | ✅ KDE Plasma (KWin) | ❌ not yet | ❌ not yet |
| Dials (Stream Deck+) | tested against a simulated OpenDeck only | | |
| Launch Claude default | `konsole` | `wt` (Windows Terminal) | Terminal via `osascript` |

Credentials are read from `~/.claude/.credentials.json` (honouring
`CLAUDE_CONFIG_DIR`); on macOS the plugin first asks the login Keychain for
Claude Code's `Claude Code-credentials` item, then falls back to the file.
Elsewhere than KDE Plasma the Session Status key still lights up, but jumping to
a window shows an alert. If `wt` isn't available on Windows, set the Launch
key's command to e.g. `cmd /c start "" /D {dir} cmd /k claude`. On Windows a
wrapped status line command runs through `cmd /C`.

## Command line

```sh
claude-opendeck-<platform> usage          # Session (5h)  12%  resets ...
claude-opendeck-<platform> usage --json
```

## How it works, and caveats

- **Undocumented endpoint.** Usage comes from `https://api.anthropic.com/api/oauth/usage`,
  the endpoint Claude Code's `/usage` command uses. It isn't a public API and
  may change or break without notice. (The status line source doesn't use it.)
- **The login token is never refreshed.** Refresh tokens rotate, so refreshing
  here could log Claude Code out. Claude Code renews the token whenever it
  runs; if it has expired, the keys show "open Claude Code" until you do.
- **Privacy.** The plugin reads Claude Code's local files (login, session list,
  transcripts for token counts) and writes its own files to your cache
  directory (`~/.cache/claude-opendeck` on Linux): hook status, the last usage
  response, status line figures and fired alerts. The login token is sent only
  to `api.anthropic.com`. Nothing else leaves your machine.
- Set `CLAUDE_OPENDECK_HOOK_DEBUG=/path/to/log` in Claude Code's environment to
  log raw hook payloads.

## Building from source

```sh
cargo build --release
scripts/install.sh                         # build, package, copy into OpenDeck's plugins folder (Linux)
cargo run --features icons --bin build-icons   # regenerate assets/icons with resvg
```

Copy the plugin rather than symlinking it: OpenDeck's file server refuses files
under a symlinked plugin folder. Releases are built by GitHub Actions for
Windows (x86_64), macOS (x86_64, Apple Silicon) and Linux (x86_64, arm64) when a
`v*` tag matching the version in `Cargo.toml` and `assets/manifest.json` is pushed.

For testing, `CLAUDE_OPENDECK_USAGE_URL` points the plugin at a stub instead of
the real usage endpoint.

## Disclosure

This plugin is AI-assisted: it was built with [Claude Code](https://claude.com/claude-code).
It is an independent project, not affiliated with or endorsed by Anthropic.
"Claude" is a trademark of Anthropic.

## License

[MIT](LICENSE)
