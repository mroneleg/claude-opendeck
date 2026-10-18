//! `install-hooks` / `uninstall-hooks`: add or remove the Claude Code hooks
//! that feed the "Claude Needs You" key, in `settings.json` under Claude Code's
//! config directory. Other settings and hooks are preserved, running it twice
//! changes nothing, and the previous file is kept as `settings.json.bak`.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::paths;

/// (event, async). Tool and prompt hooks run async so they never slow Claude
/// Code down; the rest are quick and ordering matters (SessionEnd must finish
/// before exit). A failed or denied tool never fires PostToolUse, so
/// PostToolUseFailure and PermissionDenied clear "waiting" then.
pub const EVENTS: [(&str, bool); 8] = [
    ("Notification", false),
    ("Stop", false),
    ("SessionEnd", false),
    ("UserPromptSubmit", true),
    ("PreToolUse", true),
    ("PostToolUse", true),
    ("PostToolUseFailure", true),
    ("PermissionDenied", true),
];

/// Quotes the binary path for the shell Claude Code runs hook commands in:
/// POSIX single quotes on Linux/macOS; on Windows double quotes and forward
/// slashes, which both cmd.exe and Git Bash accept.
pub fn quote_path(path: &str, windows: bool) -> String {
    if windows {
        format!("\"{}\"", path.replace('\\', "/"))
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

pub fn hook_command(binary: &Path) -> String {
    format!("{} hook", quote_path(&binary.to_string_lossy(), cfg!(windows)))
}

/// Hooks this plugin owns: the current command, an older install's command
/// (e.g. from a plugin folder that moved), or the original Node.js hook script.
fn is_ours(hook: &Value, command: &str) -> bool {
    if hook.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    let Some(existing) = hook.get("command").and_then(Value::as_str) else {
        return false;
    };
    existing == command
        || (existing.contains("claude-opendeck")
            && (existing.ends_with(" hook") || existing.ends_with("claude-status-hook")))
}

/// Returns the updated settings and how many events changed.
pub fn apply(settings: &Value, command: &str, uninstall: bool) -> (Value, usize) {
    let mut settings = settings.clone();
    if !settings.is_object() {
        settings = json!({});
    }
    let root = settings.as_object_mut().expect("object");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");

    let mut changes = 0;
    for (event, is_async) in EVENTS {
        let groups: Vec<Value> = hooks.get(event).and_then(Value::as_array).cloned().unwrap_or_default();
        // Drop our hooks (current or stale), keeping everything else...
        let mut kept: Vec<Value> = groups
            .iter()
            .cloned()
            .filter_map(|mut group| {
                if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                    list.retain(|h| !is_ours(h, command));
                    if list.is_empty() {
                        return None;
                    }
                }
                Some(group)
            })
            .collect();
        // ...then add the current one.
        if !uninstall {
            let mut hook = Map::new();
            hook.insert("type".into(), json!("command"));
            hook.insert("command".into(), json!(command));
            hook.insert("timeout".into(), json!(10));
            if is_async {
                hook.insert("async".into(), json!(true));
            }
            kept.push(json!({"hooks": [Value::Object(hook)]}));
        }
        if kept == groups {
            continue;
        }
        if kept.is_empty() {
            hooks.remove(event);
        } else {
            hooks.insert(event.to_owned(), Value::Array(kept));
        }
        changes += 1;
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    (settings, changes)
}

/// Whether every hook event already points at `binary`.
pub fn is_installed(binary: &Path) -> bool {
    let file = paths::claude_dir().join("settings.json");
    let Some(settings) = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return false;
    };
    apply(&settings, &hook_command(binary), false).1 == 0
}

/// Installs (or removes) the hooks for `binary`. Returns a human-readable summary.
pub fn run(binary: &Path, uninstall: bool) -> anyhow::Result<String> {
    let file = paths::claude_dir().join("settings.json");
    let original = std::fs::read_to_string(&file).unwrap_or_else(|_| "{}".into());
    let settings: Value = serde_json::from_str(&original)
        .map_err(|error| anyhow::anyhow!("{} is not valid JSON ({error}); not touching it", file.display()))?;
    let command = hook_command(binary);
    let (updated, changes) = apply(&settings, &command, uninstall);
    if changes == 0 {
        return Ok(format!(
            "{}: already {}, nothing to do",
            file.display(),
            if uninstall { "uninstalled" } else { "installed" }
        ));
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file.with_extension("json.bak"), &original)?;
    std::fs::write(&file, format!("{}\n", serde_json::to_string_pretty(&updated)?))?;
    Ok(format!(
        "{}: {} {changes} hook event(s); previous version saved as settings.json.bak. \
         Running Claude Code sessions pick this up after /hooks or a restart.",
        file.display(),
        if uninstall { "removed" } else { "updated" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "'/opt/plugins/claude-opendeck-x86_64-unknown-linux-gnu' hook";

    #[test]
    fn quotes_per_platform() {
        assert_eq!(quote_path("/a b/c'd", false), r"'/a b/c'\''d'");
        assert_eq!(
            quote_path(r"C:\Users\Mr One\AppData\claude-opendeck.exe", true),
            "\"C:/Users/Mr One/AppData/claude-opendeck.exe\""
        );
    }

    fn our_commands(settings: &Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn install_is_idempotent_and_preserves_other_settings() {
        let original = json!({
            "theme": "dark",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}
        });
        let (installed, changes) = apply(&original, CMD, false);
        assert_eq!(changes, EVENTS.len());
        assert_eq!(installed["theme"], "dark");
        assert_eq!(our_commands(&installed, "Stop"), ["say done", CMD]);
        assert_eq!(installed["hooks"]["PostToolUse"][0]["hooks"][0]["async"], true);
        assert!(installed["hooks"]["Notification"][0]["hooks"][0].get("async").is_none());

        let (again, changes) = apply(&installed, CMD, false);
        assert_eq!(changes, 0);
        assert_eq!(again, installed);

        let (removed, changes) = apply(&installed, CMD, true);
        assert_eq!(changes, EVENTS.len());
        assert_eq!(removed, original);
        assert_eq!(apply(&removed, CMD, true).1, 0);
    }

    #[test]
    fn install_replaces_stale_and_legacy_hooks() {
        let legacy = "/home/u/WebstormProjects/claude-opendeck/bin/claude-status-hook";
        let moved = "'/old/place/claude-opendeck-x86_64-unknown-linux-gnu' hook";
        let settings = json!({"hooks": {
            "Notification": [{"hooks": [{"type": "command", "command": legacy, "timeout": 10}]}],
            "Stop": [{"hooks": [{"type": "command", "command": moved}]}]
        }});
        let (updated, _) = apply(&settings, CMD, false);
        assert_eq!(our_commands(&updated, "Notification"), [CMD]);
        assert_eq!(our_commands(&updated, "Stop"), [CMD]);
        // Uninstall also clears legacy entries entirely.
        let (cleared, _) = apply(&settings, CMD, true);
        assert_eq!(cleared, json!({}));
    }
}
