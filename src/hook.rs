//! `claude-opendeck hook`: the Claude Code hook that records each session's
//! state for the Session Status key. Reads the hook payload on stdin and
//! writes `<cache>/claude-opendeck/sessions/<session_id>.json`.
//!
//! ```text
//! Notification                    -> waiting (permission prompt, question), or
//!                                    idle (no prompt for 60 s after finishing)
//! PreToolUse for AskUserQuestion  -> waiting (the tool always asks you, and
//!                                    its prompt's Notification comes ~6 s later)
//! UserPromptSubmit, PreToolUse,
//! PostToolUse(Failure),
//! PermissionDenied                -> working
//! Stop                            -> done
//! SessionEnd                      -> file removed
//! ```
//!
//! Never fails the hook: every error is swallowed so Claude Code is unaffected.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{paths, process};

/// Tool hooks run async, so one can land just after a Notification for the next
/// permission prompt and wrongly flip the session back to working. Answering a
/// prompt takes longer than this, so a later tool event still clears it.
pub const WAITING_GRACE_MS: i64 = 1500;

const ASYNC_TOOL_EVENTS: [&str; 4] = ["PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionDenied"];

/// The status a hook event moves the session to, or None to leave it alone.
/// Only prompts that block on you count as waiting; the 60 s idle reminder is
/// its own state so the key can ignore finished sessions; auth_success is noise.
pub fn state_for(event: &Value) -> Option<&'static str> {
    let name = event.get("hook_event_name").and_then(Value::as_str)?;
    match name {
        "Notification" => {
            let message = event.get("message").and_then(Value::as_str).unwrap_or("");
            match event.get("notification_type").and_then(Value::as_str) {
                Some("auth_success") => None,
                Some("idle_prompt") => Some("idle"),
                None if message.to_lowercase().contains("waiting for your input") => Some("idle"),
                _ => Some("waiting"),
            }
        }
        "PreToolUse" if event.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") => Some("waiting"),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
            Some("working")
        }
        "Stop" => Some("done"),
        _ => None,
    }
}

/// True if `current` (the existing status file) went to waiting moments ago.
pub fn just_started_waiting(current: &Value, now: i64) -> bool {
    current.get("state").and_then(Value::as_str) == Some("waiting")
        && current
            .get("updatedAt")
            .and_then(Value::as_i64)
            .is_some_and(|at| now - at < WAITING_GRACE_MS)
}

/// Session ids become file names, so keep only safe characters.
pub fn sanitize_session_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

fn session_file(claude_dir: &Path, pid: u32) -> PathBuf {
    claude_dir.join("sessions").join(format!("{pid}.json"))
}

/// The session's own directory from Claude Code's session file. The hook's
/// `cwd` follows the session's shell around; the session file has the
/// directory Claude Code was started in, which is what window titles show.
fn session_cwd(claude_dir: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(session_file(claude_dir, pid)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("cwd").and_then(Value::as_str).map(str::to_owned)
}

/// Builds the status record for an event, or None if the file should not change.
pub fn handle(event: &Value, status_dir: &Path, claude_dir: &Path, chain: &[u32], now: i64) -> std::io::Result<()> {
    let session_id = sanitize_session_id(event.get("session_id").and_then(Value::as_str).unwrap_or(""));
    if session_id.is_empty() {
        return Ok(());
    }
    let file = status_dir.join(format!("{session_id}.json"));
    let name = event.get("hook_event_name").and_then(Value::as_str).unwrap_or("");

    if name == "SessionEnd" {
        let _ = std::fs::remove_file(&file);
        return Ok(());
    }
    let Some(state) = state_for(event) else {
        return Ok(());
    };
    let current = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if let Some(current) = &current {
        if ASYNC_TOOL_EVENTS.contains(&name) && just_started_waiting(current, now) {
            return Ok(());
        }
        // A finished session the user dismissed stays dismissed through the
        // 60 s idle reminder; anything new (a prompt, a tool) brings it back.
        if state == "idle" && current.get("state").and_then(Value::as_str) == Some("cleared") {
            return Ok(());
        }
    }

    // Claude Code's own pid is the nearest ancestor with a sessions entry;
    // record it and everything outwards (its terminal, editor, ...).
    let owner_index = chain.iter().position(|pid| session_file(claude_dir, *pid).exists());
    let pids: Vec<u32> = owner_index.map_or_else(|| chain.to_vec(), |i| chain[i..].to_vec());
    let claude_pid = owner_index.map(|i| chain[i]);

    let cwd = claude_pid
        .and_then(|pid| session_cwd(claude_dir, pid))
        .or_else(|| event.get("cwd").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let record = json!({
        "sessionId": session_id,
        "state": state,
        "event": name,
        "cwd": cwd,
        "project": paths::basename(Path::new(&cwd)),
        "message": if name == "Notification" { event.get("message").and_then(Value::as_str).unwrap_or("") } else { "" },
        "updatedAt": now,
        "claudePid": claude_pid,
        "claudePidStart": claude_pid.and_then(process::start_marker),
        "pids": pids,
    });
    paths::write_atomic(&file, record.to_string().as_bytes())
}

/// Entry point for `claude-opendeck hook`. Always succeeds.
pub fn run() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    // Troubleshooting: set CLAUDE_OPENDECK_HOOK_DEBUG to a file path to log raw payloads.
    if let Some(debug) = std::env::var_os("CLAUDE_OPENDECK_HOOK_DEBUG") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(debug) {
            let _ = writeln!(file, "{}", input.trim());
        }
    }
    let Ok(event) = serde_json::from_str::<Value>(if input.trim().is_empty() { "{}" } else { &input }) else {
        return;
    };
    let chain = process::ancestors(std::process::id());
    let _ = handle(
        &event,
        &paths::status_dir(),
        &paths::claude_dir(),
        &chain,
        paths::now_ms(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(kind: Option<&str>, message: &str) -> Value {
        let mut event = json!({"hook_event_name": "Notification", "message": message});
        if let Some(kind) = kind {
            event["notification_type"] = json!(kind);
        }
        event
    }

    #[test]
    fn maps_events_to_states() {
        assert_eq!(state_for(&notification(Some("permission_prompt"), "")), Some("waiting"));
        assert_eq!(
            state_for(&notification(Some("elicitation_dialog"), "")),
            Some("waiting")
        );
        assert_eq!(state_for(&notification(Some("idle_prompt"), "")), Some("idle"));
        assert_eq!(state_for(&notification(Some("auth_success"), "")), None);
        assert_eq!(
            state_for(&notification(None, "Claude is waiting for your input")),
            Some("idle")
        );
        assert_eq!(
            state_for(&notification(None, "Claude needs your permission")),
            Some("waiting")
        );
        for working in [
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionDenied",
        ] {
            assert_eq!(
                state_for(&json!({"hook_event_name": working})),
                Some("working"),
                "{working}"
            );
        }
        assert_eq!(
            state_for(&json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion"})),
            Some("waiting")
        );
        assert_eq!(
            state_for(&json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion"})),
            Some("working")
        );
        assert_eq!(state_for(&json!({"hook_event_name": "Stop"})), Some("done"));
        assert_eq!(state_for(&json!({"hook_event_name": "SubagentStop"})), None);
        assert_eq!(state_for(&json!({})), None);
    }

    #[test]
    fn grace_window_only_for_fresh_waiting() {
        let waiting = |at| json!({"state": "waiting", "updatedAt": at});
        assert!(just_started_waiting(&waiting(10_000), 10_000 + WAITING_GRACE_MS - 1));
        assert!(!just_started_waiting(&waiting(10_000), 10_000 + WAITING_GRACE_MS));
        assert!(!just_started_waiting(
            &json!({"state": "working", "updatedAt": 10_000}),
            10_001
        ));
    }

    #[test]
    fn sanitizes_ids() {
        assert_eq!(sanitize_session_id("abc-123_X/../../etc"), "abc-123_Xetc");
    }

    fn temp_dirs(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("claude-opendeck-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let status = root.join("status");
        let claude = root.join("claude");
        std::fs::create_dir_all(claude.join("sessions")).unwrap();
        (root, status, claude)
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn writes_record_with_owner_and_project() {
        let (root, status, claude) = temp_dirs("record");
        // Pretend pid 222 is Claude Code, started in /work/my-app.
        std::fs::write(claude.join("sessions/222.json"), r#"{"cwd":"/work/my-app"}"#).unwrap();
        let event = json!({"session_id": "s1", "hook_event_name": "Notification",
                           "notification_type": "permission_prompt", "message": "needs Bash", "cwd": "/elsewhere"});
        handle(&event, &status, &claude, &[111, 222, 333], 5000).unwrap();
        let record = read(&status.join("s1.json"));
        assert_eq!(record["state"], "waiting");
        assert_eq!(record["project"], "my-app");
        assert_eq!(record["cwd"], "/work/my-app");
        assert_eq!(record["claudePid"], 222);
        assert_eq!(record["pids"], json!([222, 333]));
        assert_eq!(record["message"], "needs Bash");
        assert_eq!(record["updatedAt"], 5000);

        // A late async tool event inside the grace window doesn't clear it...
        handle(
            &json!({"session_id": "s1", "hook_event_name": "PostToolUse"}),
            &status,
            &claude,
            &[111, 222],
            5500,
        )
        .unwrap();
        assert_eq!(read(&status.join("s1.json"))["state"], "waiting");
        // ...but one after it does, and Stop/SessionEnd work as before.
        handle(
            &json!({"session_id": "s1", "hook_event_name": "PostToolUseFailure"}),
            &status,
            &claude,
            &[111, 222],
            9000,
        )
        .unwrap();
        assert_eq!(read(&status.join("s1.json"))["state"], "working");
        assert_eq!(read(&status.join("s1.json"))["message"], "");
        handle(
            &json!({"session_id": "s1", "hook_event_name": "Stop"}),
            &status,
            &claude,
            &[222],
            9100,
        )
        .unwrap();
        assert_eq!(read(&status.join("s1.json"))["state"], "done");
        handle(
            &json!({"session_id": "s1", "hook_event_name": "SessionEnd"}),
            &status,
            &claude,
            &[222],
            9200,
        )
        .unwrap();
        assert!(!status.join("s1.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn falls_back_to_event_cwd_without_owner() {
        let (root, status, claude) = temp_dirs("fallback");
        let event = json!({"session_id": "s2", "hook_event_name": "UserPromptSubmit", "cwd": "/srv/thing"});
        handle(&event, &status, &claude, &[7, 8], 1).unwrap();
        let record = read(&status.join("s2.json"));
        assert_eq!(record["project"], "thing");
        assert_eq!(record["claudePid"], Value::Null);
        assert_eq!(record["pids"], json!([7, 8]));
        // A dismissed session ignores the idle reminder but not new work.
        std::fs::write(status.join("s2.json"), r#"{"state":"cleared"}"#).unwrap();
        handle(
            &json!({"session_id": "s2", "hook_event_name": "Notification", "notification_type": "idle_prompt"}),
            &status,
            &claude,
            &[7],
            2,
        )
        .unwrap();
        assert_eq!(read(&status.join("s2.json"))["state"], "cleared");
        handle(&event, &status, &claude, &[7, 8], 3).unwrap();
        // auth_success leaves the file alone.
        handle(
            &json!({"session_id": "s2", "hook_event_name": "Notification", "notification_type": "auth_success"}),
            &status,
            &claude,
            &[7],
            2,
        )
        .unwrap();
        assert_eq!(read(&status.join("s2.json"))["state"], "working");
        let _ = std::fs::remove_dir_all(root);
    }
}
