//! Claude Code session state from two local sources:
//! - `~/.claude/sessions/<pid>.json`, which Claude Code itself maintains for
//!   every running session (pid, cwd, busy/idle status).
//! - `<cache>/claude-opendeck/sessions/<session_id>.json`, written by the
//!   `hook` subcommand so we know when a session is waiting on the user and
//!   which processes it runs under.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{paths, process};

/// A hook status file this old belongs to a session that died without SessionEnd.
pub const STALE_MS: i64 = 12 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveSession {
    pub pid: u32,
    pub project: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookSession {
    pub session_id: String,
    pub state: String,
    pub project: String,
    pub updated_at: i64,
    pub pids: Vec<u32>,
}

fn read_json_files(dir: &Path) -> Vec<(PathBuf, i64, Value)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let path = entry.path();
            // Mid-write or just removed: skip this round.
            let text = std::fs::read_to_string(&path).ok()?;
            let data: Value = serde_json::from_str(&text).ok()?;
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            Some((path, mtime, data))
        })
        .collect()
}

fn as_u32(value: Option<&Value>) -> Option<u32> {
    value.and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}

/// Claude Code writes `procStart` in the Linux `/proc` format; elsewhere we
/// can only check that the pid is running.
fn claude_session_alive(pid: u32, proc_start: Option<&str>) -> bool {
    if cfg!(target_os = "linux") {
        process::is_alive(pid, proc_start)
    } else {
        process::is_running(pid)
    }
}

/// Running Claude Code sessions.
pub fn list_active_sessions() -> Vec<ActiveSession> {
    read_json_files(&paths::claude_dir().join("sessions"))
        .into_iter()
        .filter_map(|(_, _, data)| {
            let pid = as_u32(data.get("pid"))?;
            let proc_start = data.get("procStart").and_then(Value::as_str);
            if !claude_session_alive(pid, proc_start) {
                return None;
            }
            let cwd = data.get("cwd").and_then(Value::as_str).unwrap_or("");
            Some(ActiveSession {
                pid,
                project: paths::basename(Path::new(cwd)),
                status: data
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
            })
        })
        .collect()
}

/// Parses one hook status file; None if it is stale or its Claude Code process
/// is gone (the caller deletes those).
pub fn parse_hook_session(
    data: &Value,
    mtime: i64,
    now: i64,
    alive: impl Fn(u32, Option<&str>) -> bool,
) -> Option<HookSession> {
    let updated_at = data.get("updatedAt").and_then(Value::as_i64).unwrap_or(mtime);
    if now - updated_at > STALE_MS {
        return None;
    }
    if let Some(pid) = as_u32(data.get("claudePid"))
        && !alive(pid, data.get("claudePidStart").and_then(Value::as_str))
    {
        return None;
    }
    let pids = data
        .get("pids")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(|p| as_u32(Some(p))).collect())
        .unwrap_or_default();
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    Some(HookSession {
        session_id: text("sessionId"),
        state: text("state"),
        project: text("project"),
        updated_at,
        pids,
    })
}

/// Hook-reported sessions, newest first. Removes stale and dead entries.
pub fn list_hook_sessions(now: i64) -> Vec<HookSession> {
    let mut sessions: Vec<HookSession> = read_json_files(&paths::status_dir())
        .into_iter()
        .filter_map(|(path, mtime, data)| {
            let session = parse_hook_session(&data, mtime, now, process::is_alive);
            if session.is_none() {
                let _ = std::fs::remove_file(&path);
            }
            session
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    sessions
}

/// Sessions a needs-you key lights up for, newest first.
pub fn attention(sessions: &[HookSession], include_idle: bool) -> Vec<&HookSession> {
    sessions
        .iter()
        .filter(|s| s.state == "waiting" || (include_idle && s.state == "idle"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_stale_and_dead_sessions() {
        let record = json!({"sessionId": "a", "state": "waiting", "project": "p", "updatedAt": 1000,
                            "claudePid": 42, "claudePidStart": "99", "pids": [42, 7]});
        let alive = |pid: u32, start: Option<&str>| pid == 42 && start == Some("99");
        let session = parse_hook_session(&record, 0, 2000, alive).unwrap();
        assert_eq!(session.pids, [42, 7]);
        assert_eq!(session.state, "waiting");
        assert!(parse_hook_session(&record, 0, 1000 + STALE_MS + 1, alive).is_none());
        assert!(parse_hook_session(&record, 0, 2000, |_, _| false).is_none());
        // No owner recorded: kept until stale; mtime stands in for updatedAt.
        let ownerless = json!({"sessionId": "b", "state": "idle"});
        assert_eq!(
            parse_hook_session(&ownerless, 500, 600, |_, _| false)
                .unwrap()
                .updated_at,
            500
        );
    }

    #[test]
    fn attention_respects_idle_setting() {
        let s = |state: &str| HookSession {
            session_id: state.into(),
            state: state.into(),
            project: String::new(),
            updated_at: 0,
            pids: vec![],
        };
        let all = [s("waiting"), s("idle"), s("working"), s("done")];
        assert_eq!(attention(&all, false).len(), 1);
        assert_eq!(attention(&all, true).len(), 2);
    }
}
