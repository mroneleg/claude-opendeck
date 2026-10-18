# Claude Usage for OpenDeck

An [OpenAction](https://openaction.amankhanna.me) plugin for [OpenDeck](https://github.com/nekename/OpenDeck)
(and other Stream Deck–compatible software) that shows your Claude plan usage on
your deck, with pace and threshold alerts, plus the status of your Claude Code
sessions.

## Actions

| Action | Key shows | Press |
|---|---|---|
| Claude Usage | Session (5 h) and weekly (7 d) usage, or one of them, or the highest per-model weekly limit. A white tick marks where even spending would be, with "on pace" / "+N% pace" / "−N% pace" | Refresh now |
| Usage Breakdown | This week's usage by surface (Claude Code, Chats, …) | Refresh now |
| Extra Usage | Spending on usage credits beyond your plan, against its limit | Refresh now |
| Claude Needs You | Lights up orange with the project name when a Claude Code session (terminal or VS Code) is waiting for permission or an answer | Jump to that session's window |
| Active Sessions | Running Claude Code sessions, and how many are busy | Refresh |
| Today's Activity | Tokens used today across your Claude Code sessions (no cost estimates) | Refresh |
| Launch Claude | A project folder | Open a terminal running `claude` there |

**Alerts:** each Claude Usage key can alert at two thresholds (default 80% and
95%; 0 turns one off). Crossing one flashes the key and sends a desktop
notification, once per window until it resets.

## Requirements

- [Claude Code](https://claude.com/claude-code), logged in with a Claude account
  (the plugin reads Claude Code's stored login; it has no login of its own).
- OpenDeck, or other software that runs Stream Deck SDK 2 plugins.

## Install

Download `com.mroneleg.claudeusage.streamDeckPlugin` from the
[latest release](https://github.com/mroneleg/claude-opendeck/releases/latest),
install it in OpenDeck (Plugins → Install from file), then restart OpenDeck.

### "Claude Needs You" hooks

The Needs You key learns about sessions from Claude Code
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

Notifications map by type: permission prompts and questions light the key; the
60 s "waiting for your input" reminder only does if **Also light up for idle
sessions** is ticked. Claude Code waits a few seconds before raising a
permission notification, so prompts you answer straight away never light it.

## Platform support

| | Linux | Windows | macOS |
|---|---|---|---|
| Usage, breakdown, extra usage, alerts | ✅ tested | builds in CI, untested | builds in CI, untested |
| Needs You (lights up), sessions, activity | ✅ tested | builds in CI, untested | builds in CI, untested |
| Needs You (jump to window) | ✅ KDE Plasma (KWin) | ❌ not yet | ❌ not yet |
| Launch Claude default | `konsole` | `wt` (Windows Terminal) | Terminal via `osascript` |

Credentials are read from `~/.claude/.credentials.json` (honouring
`CLAUDE_CONFIG_DIR`); on macOS the plugin first asks the login Keychain for
Claude Code's `Claude Code-credentials` item, then falls back to the file.
Elsewhere than KDE Plasma the Needs You key still lights up, but pressing it
shows an alert. If `wt` isn't available on Windows, set the Launch key's
command to e.g. `cmd /c start "" /D {dir} cmd /k claude`.

## Command line

```sh
claude-opendeck-<platform> usage          # Session (5h)  12%  resets ...
claude-opendeck-<platform> usage --json
```

## How it works, and caveats

- **Undocumented endpoint.** Usage comes from `https://api.anthropic.com/api/oauth/usage`,
  the endpoint Claude Code's `/usage` command uses. It isn't a public API and
  may change or break without notice.
- **The login token is never refreshed.** Refresh tokens rotate, so refreshing
  here could log Claude Code out. Claude Code renews the token whenever it
  runs; if it has expired, the keys show "open Claude Code" until you do.
- **Privacy.** The plugin reads Claude Code's local files (login, session list,
  transcripts for token counts) and writes its own status files to your cache
  directory (`~/.cache/claude-opendeck` on Linux). The login token is sent only
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

## Disclosure

This plugin is AI-assisted: it was built with [Claude Code](https://claude.com/claude-code).
It is an independent project, not affiliated with or endorsed by Anthropic.
"Claude" is a trademark of Anthropic.

## License

[MIT](LICENSE)
