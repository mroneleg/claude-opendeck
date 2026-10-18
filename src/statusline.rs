//! `claude-opendeck statusline [-- <command>]`: Claude Code's status line,
//! used as a second source of plan usage.
//!
//! Claude Code runs the status line command after each response, with JSON on
//! stdin that includes `rate_limits.five_hour` / `rate_limits.seven_day`
//! (`used_percentage` 0-100, `resets_at` in Unix seconds) for claude.ai
//! subscriptions. We save those to `<cache>/statusline.json`, then print a
//! status line: the output of the user's own command if one is wrapped (same
//! stdin, same exit code), otherwise a compact default.
//!
//! Compared with the usage endpoint this needs no token, but it only covers
//! 5h/7d and only updates while a Claude Code session is producing responses.
//!
//! `install-statusline` / `uninstall-statusline` wrap (and later restore) any
//! existing `statusLine` command in Claude Code's settings.json.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::hooks_install::{quote_path, read_settings, write_settings};
use crate::paths;
use crate::usage::{Bridge, BridgeWindow};

fn parse_reset(value: Option<&Value>) -> Option<i64> {
    match value? {
        // Seconds per Claude Code's schema; tolerate milliseconds too.
        Value::Number(n) => n
            .as_f64()
            .map(|t| if t > 1e12 { t as i64 } else { (t * 1000.0) as i64 }),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis()),
        _ => None,
    }
}

fn parse_window(value: Option<&Value>) -> Option<BridgeWindow> {
    let value = value?;
    Some(BridgeWindow {
        percent: value.get("used_percentage")?.as_f64()?,
        resets_at: parse_reset(value.get("resets_at")),
    })
}

/// The 5h/7d windows in a status line payload; None if it has neither.
pub fn parse_input(input: &Value, now: i64) -> Option<Bridge> {
    let limits = input.get("rate_limits")?;
    let bridge = Bridge {
        five_hour: parse_window(limits.get("five_hour")),
        seven_day: parse_window(limits.get("seven_day")),
        updated_at: now,
    };
    (bridge.five_hour.is_some() || bridge.seven_day.is_some()).then_some(bridge)
}

fn window_json(window: Option<BridgeWindow>) -> Value {
    window.map_or(Value::Null, |w| json!({"percent": w.percent, "resetsAt": w.resets_at}))
}

fn window_from_json(value: Option<&Value>) -> Option<BridgeWindow> {
    let value = value.filter(|v| v.is_object())?;
    Some(BridgeWindow {
        percent: value.get("percent")?.as_f64()?,
        resets_at: value.get("resetsAt").and_then(Value::as_i64),
    })
}

pub fn bridge_to_json(bridge: &Bridge) -> Value {
    json!({
        "fiveHour": window_json(bridge.five_hour),
        "sevenDay": window_json(bridge.seven_day),
        "updatedAt": bridge.updated_at,
    })
}

/// Parses the saved file, dropping windows that have already reset (their
/// percentage no longer applies).
pub fn bridge_from_json(value: &Value, now: i64) -> Option<Bridge> {
    let current = |w: Option<BridgeWindow>| w.filter(|w| w.resets_at.is_none_or(|at| at > now));
    let bridge = Bridge {
        five_hour: current(window_from_json(value.get("fiveHour"))),
        seven_day: current(window_from_json(value.get("sevenDay"))),
        updated_at: value.get("updatedAt")?.as_i64()?,
    };
    (bridge.five_hour.is_some() || bridge.seven_day.is_some()).then_some(bridge)
}

/// The saved status line figures, if any are still current.
pub fn load_bridge(now: i64) -> Option<Bridge> {
    let text = std::fs::read_to_string(paths::statusline_file()).ok()?;
    bridge_from_json(&serde_json::from_str(&text).ok()?, now)
}

/// Saves new figures, keeping a window the payload didn't include.
pub fn save_bridge(file: &Path, update: &Bridge) -> std::io::Result<()> {
    let previous = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let keep = |key: &str| previous.as_ref().and_then(|p| window_from_json(p.get(key)));
    let merged = Bridge {
        five_hour: update.five_hour.or_else(|| keep("fiveHour")),
        seven_day: update.seven_day.or_else(|| keep("sevenDay")),
        updated_at: update.updated_at,
    };
    paths::write_atomic(file, bridge_to_json(&merged).to_string().as_bytes())
}

/// "Fable · 5h 12% · 7d 4%" from the payload, for when no command is wrapped.
pub fn default_line(input: &Value, bridge: Option<&Bridge>) -> String {
    let mut parts = Vec::new();
    if let Some(model) = input.pointer("/model/display_name").and_then(Value::as_str) {
        parts.push(model.to_owned());
    }
    if let Some(bridge) = bridge {
        for (label, window) in [("5h", bridge.five_hour), ("7d", bridge.seven_day)] {
            if let Some(w) = window {
                parts.push(format!("{label} {}%", w.percent.round() as i64));
            }
        }
    }
    parts.join(" · ")
}

/// Runs the wrapped status line command with the same stdin; returns its
/// stdout and exit code. Stderr passes straight through.
pub fn run_wrapped(command: &str, input: &[u8]) -> std::io::Result<(Vec<u8>, i32)> {
    let mut child = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", command])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?
    } else {
        Command::new("sh")
            .args(["-c", command])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A command that ignores stdin may close it early; that's fine.
        let _ = stdin.write_all(input);
    }
    let output = child.wait_with_output()?;
    Ok((output.stdout, output.status.code().unwrap_or(1)))
}

/// Entry point for `claude-opendeck statusline [-- <command...>]`. Returns the
/// exit code: the wrapped command's, or 0. Saving never affects the output.
pub fn run(args: &[String]) -> i32 {
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    let parsed: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let bridge = parse_input(&parsed, paths::now_ms());
    if let Some(bridge) = &bridge {
        let _ = save_bridge(&paths::statusline_file(), bridge);
    }

    let wrapped = args
        .iter()
        .position(|a| a == "--")
        .map(|i| args[i + 1..].join(" "))
        .filter(|c| !c.trim().is_empty());
    let mut stdout = std::io::stdout();
    match wrapped {
        Some(command) => match run_wrapped(&command, &input) {
            Ok((output, code)) => {
                let _ = stdout.write_all(&output);
                code
            }
            Err(error) => {
                eprintln!("claude-opendeck statusline: couldn't run `{command}`: {error}");
                1
            }
        },
        None => {
            let _ = writeln!(stdout, "{}", default_line(&parsed, bridge.as_ref()));
            0
        }
    }
}

// --- install / uninstall -----------------------------------------------------------

/// Quotes one argument for the shell Claude Code runs status line commands in.
pub fn quote_arg(value: &str, windows: bool) -> String {
    if windows {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Inverse of [`quote_arg`] for the strings it produces.
pub fn unquote_arg(quoted: &str, windows: bool) -> Option<String> {
    let quoted = quoted.trim();
    if windows {
        let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
        return Some(inner.replace("\\\"", "\""));
    }
    // POSIX: a run of '...' segments joined by \' escapes.
    let mut out = String::new();
    let mut rest = quoted;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix(r"\'") {
            out.push('\'');
            rest = after;
        } else {
            let after = rest.strip_prefix('\'')?;
            let end = after.find('\'')?;
            out.push_str(&after[..end]);
            rest = &after[end + 1..];
        }
    }
    Some(out)
}

fn our_prefix(binary: &Path, windows: bool) -> String {
    format!("{} statusline", quote_path(&binary.to_string_lossy(), windows))
}

/// If `command` is ours (any install location), the command it wraps ("" if none).
pub fn wrapped_command(command: &str, windows: bool) -> Option<String> {
    let index = command.find(" statusline")?;
    if !command[..index].contains("claude-opendeck") {
        return None;
    }
    let rest = command[index + " statusline".len()..].trim();
    if rest.is_empty() {
        return Some(String::new());
    }
    unquote_arg(rest.strip_prefix("--")?.trim(), windows)
}

/// Returns the updated settings and whether anything changed.
pub fn apply(settings: &Value, binary: &Path, uninstall: bool, windows: bool) -> anyhow::Result<(Value, bool)> {
    let mut settings = if settings.is_object() {
        settings.clone()
    } else {
        json!({})
    };
    let root = settings.as_object_mut().expect("object");
    let current = root.get("statusLine").cloned();
    let current_command = current
        .as_ref()
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(line) = &current
        && line.get("type").and_then(Value::as_str).is_some_and(|t| t != "command")
    {
        anyhow::bail!("statusLine has an unsupported type; not touching it");
    }
    let ours = current_command.as_deref().and_then(|c| wrapped_command(c, windows));

    if uninstall {
        let Some(previous) = ours else {
            return Ok((settings, false));
        };
        if previous.is_empty() {
            root.remove("statusLine");
        } else if let Some(line) = root.get_mut("statusLine").and_then(Value::as_object_mut) {
            line.insert("command".into(), json!(previous));
        }
        return Ok((settings, true));
    }

    let previous = match ours {
        Some(previous) => previous,
        None => current_command.unwrap_or_default(),
    };
    let mut command = our_prefix(binary, windows);
    if !previous.trim().is_empty() {
        command = format!("{command} -- {}", quote_arg(&previous, windows));
    }
    if current_command_is(&current, &command) {
        return Ok((settings, false));
    }
    let line = root.entry("statusLine").or_insert_with(|| json!({"type": "command"}));
    if !line.is_object() {
        *line = json!({"type": "command"});
    }
    let line = line.as_object_mut().expect("object");
    line.insert("type".into(), json!("command"));
    line.insert("command".into(), json!(command));
    Ok((settings, true))
}

fn current_command_is(current: &Option<Value>, command: &str) -> bool {
    current.as_ref().and_then(|s| s.get("command")).and_then(Value::as_str) == Some(command)
}

/// Whether Claude Code's status line already runs this binary.
pub fn is_installed(binary: &Path) -> bool {
    let Ok((_, _, settings)) = read_settings() else {
        return false;
    };
    settings
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.starts_with(&our_prefix(binary, cfg!(windows))))
}

/// Installs (or removes) the status line bridge. Returns a summary.
pub fn install(binary: &Path, uninstall: bool) -> anyhow::Result<String> {
    let (file, original, settings) = read_settings()?;
    let (updated, changed) = apply(&settings, binary, uninstall, cfg!(windows))?;
    if !changed {
        return Ok(format!(
            "{}: status line already {}, nothing to do",
            file.display(),
            if uninstall { "removed" } else { "installed" }
        ));
    }
    write_settings(&file, &original, &updated)?;
    let wrapped = updated
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .and_then(|c| wrapped_command(c, cfg!(windows)))
        .filter(|c| !c.is_empty());
    Ok(format!(
        "{}: status line {}{}; previous version saved as settings.json.bak.",
        file.display(),
        if uninstall { "restored" } else { "installed" },
        match (uninstall, wrapped) {
            (false, Some(c)) => format!(" (still runs your `{c}`)"),
            _ => String::new(),
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Value {
        json!({
            "model": {"display_name": "Fable"},
            "rate_limits": {
                "five_hour": {"used_percentage": 12.3, "resets_at": 1_800_000_000},
                "seven_day": {"used_percentage": 4.0, "resets_at": "2027-01-15T08:00:00Z"}
            }
        })
    }

    #[test]
    fn parses_rate_limits() {
        let bridge = parse_input(&payload(), 77).unwrap();
        assert_eq!(bridge.updated_at, 77);
        let five = bridge.five_hour.unwrap();
        assert_eq!(five.percent, 12.3);
        assert_eq!(five.resets_at, Some(1_800_000_000_000));
        assert_eq!(
            bridge.seven_day.unwrap().resets_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2027-01-15T08:00:00Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        // No rate limits yet (before the first response, or API-key sessions).
        assert!(parse_input(&json!({"model": {}}), 0).is_none());
        assert!(parse_input(&json!({"rate_limits": {"five_hour": null}}), 0).is_none());
        assert_eq!(default_line(&payload(), Some(&bridge)), "Fable · 5h 12% · 7d 4%");
        assert_eq!(default_line(&json!({}), None), "");
    }

    #[test]
    fn saved_file_merges_and_drops_reset_windows() {
        let dir = std::env::temp_dir().join(format!("claude-opendeck-sl-{}", std::process::id()));
        let file = dir.join("statusline.json");
        let _ = std::fs::remove_dir_all(&dir);
        let w = |percent, resets_at| {
            Some(BridgeWindow {
                percent,
                resets_at: Some(resets_at),
            })
        };
        save_bridge(
            &file,
            &Bridge {
                five_hour: w(10.0, 5_000),
                seven_day: w(3.0, 9_000),
                updated_at: 100,
            },
        )
        .unwrap();
        // A later payload with only 5h keeps the saved 7d.
        save_bridge(
            &file,
            &Bridge {
                five_hour: w(20.0, 5_000),
                seven_day: None,
                updated_at: 200,
            },
        )
        .unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let loaded = bridge_from_json(&value, 1_000).unwrap();
        assert_eq!(loaded.five_hour.unwrap().percent, 20.0);
        assert_eq!(loaded.seven_day.unwrap().percent, 3.0);
        assert_eq!(loaded.updated_at, 200);
        // After the 5h reset only 7d remains; after both, nothing.
        let later = bridge_from_json(&value, 6_000).unwrap();
        assert!(later.five_hour.is_none() && later.seven_day.is_some());
        assert!(bridge_from_json(&value, 10_000).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quoting_round_trips() {
        for value in [
            "bash ~/.claude/statusline.sh",
            "printf '%s' \"x\" | head -1",
            "it's",
            "''",
        ] {
            assert_eq!(
                unquote_arg(&quote_arg(value, false), false).as_deref(),
                Some(value),
                "{value}"
            );
            assert_eq!(
                unquote_arg(&quote_arg(value, true), true).as_deref(),
                Some(value),
                "{value}"
            );
        }
        assert_eq!(unquote_arg("noquotes", false), None);
    }

    #[cfg(unix)]
    #[test]
    fn posix_quoting_survives_the_shell() {
        // What sh hands our binary must be exactly the original command.
        let original = r#"printf '%s|%s' "a b" it\'s"#;
        let output = Command::new("sh")
            .args(["-c", &format!("printf '%s' {}", quote_arg(original, false))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn wrapped_command_gets_stdin_and_keeps_exit_code() {
        let (out, code) = run_wrapped("cat; echo done; exit 3", b"hello ").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "hello done\n");
        assert_eq!(code, 3);
        let (out, code) = run_wrapped("echo ignored-stdin", b"x".repeat(1 << 20).as_slice()).unwrap();
        assert_eq!((String::from_utf8(out).unwrap().as_str(), code), ("ignored-stdin\n", 0));
    }

    #[test]
    fn install_wraps_existing_command_and_uninstall_restores_it() {
        let binary = Path::new("/opt/plugin/claude-opendeck-x86_64-unknown-linux-gnu");
        let original = json!({"theme": "dark", "statusLine": {"type": "command", "command": "bash ~/.claude/sl.sh | head -1", "padding": 2}});
        let (installed, changed) = apply(&original, binary, false, false).unwrap();
        assert!(changed);
        let command = installed["statusLine"]["command"].as_str().unwrap();
        assert_eq!(
            command,
            "'/opt/plugin/claude-opendeck-x86_64-unknown-linux-gnu' statusline -- 'bash ~/.claude/sl.sh | head -1'"
        );
        assert_eq!(installed["statusLine"]["padding"], 2);
        assert_eq!(installed["theme"], "dark");
        assert_eq!(
            wrapped_command(command, false).as_deref(),
            Some("bash ~/.claude/sl.sh | head -1")
        );
        // Idempotent; a moved binary is rewritten but keeps the wrapped command.
        assert!(!apply(&installed, binary, false, false).unwrap().1);
        let moved = Path::new("/new/claude-opendeck-x86_64-unknown-linux-gnu");
        let (reinstalled, changed) = apply(&installed, moved, false, false).unwrap();
        assert!(changed);
        assert!(
            reinstalled["statusLine"]["command"]
                .as_str()
                .unwrap()
                .starts_with("'/new/")
        );
        // Uninstall restores the exact original.
        let (restored, changed) = apply(&reinstalled, moved, true, false).unwrap();
        assert!(changed);
        assert_eq!(restored, original);
        assert!(!apply(&restored, moved, true, false).unwrap().1);
    }

    #[test]
    fn install_without_existing_and_refuses_unknown_types() {
        let binary = Path::new("/p/claude-opendeck");
        let (installed, _) = apply(&json!({}), binary, false, false).unwrap();
        assert_eq!(
            installed,
            json!({"statusLine": {"type": "command", "command": "'/p/claude-opendeck' statusline"}})
        );
        let (removed, _) = apply(&installed, binary, true, false).unwrap();
        assert_eq!(removed, json!({}));
        assert!(
            apply(
                &json!({"statusLine": {"type": "static", "text": "x"}}),
                binary,
                false,
                false
            )
            .is_err()
        );
        // Someone else's status line isn't "ours" even if it mentions statusline.
        assert_eq!(wrapped_command("my-tool statusline", false), None);
    }
}
