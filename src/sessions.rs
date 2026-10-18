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
/// "Working" with no hook event for this long counts as idle: pressing Esc to
/// interrupt Claude doesn't fire Stop, so nothing else would clear it.
pub const WORKING_EXPIRY_MS: i64 = 30 * 60 * 1000;

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

/// What a session-status key shows for one session, least urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Idle,
    Done,
    Working,
    Waiting,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Done => "done",
            Status::Working => "working",
            Status::Waiting => "needs you",
        }
    }
}

/// Which statuses light the key up; waiting always does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LightUp {
    pub working: bool,
    pub done: bool,
}

/// A session's status as shown. The hook's "idle" (the 60 s reminder after a
/// session finishes) is just a finished session, so it shows as done;
/// "cleared" (done, then dismissed with the key) and expired "working" are idle.
pub fn status_of(session: &HookSession, now: i64) -> Status {
    match session.state.as_str() {
        "waiting" => Status::Waiting,
        "working" if now - session.updated_at < WORKING_EXPIRY_MS => Status::Working,
        "done" | "idle" => Status::Done,
        _ => Status::Idle,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary<'a> {
    /// The most urgent lit status, or Idle if nothing lights the key.
    pub status: Status,
    /// Sessions in `status`, newest first (empty when Idle).
    pub lit: Vec<&'a HookSession>,
    pub waiting: usize,
    pub working: usize,
    pub done: usize,
}

/// Summarises sessions (newest first) for a session-status key.
pub fn summarize<'a>(sessions: &'a [HookSession], light: LightUp, now: i64) -> Summary<'a> {
    let lights = |status: Status| match status {
        Status::Waiting => true,
        Status::Working => light.working,
        Status::Done => light.done,
        Status::Idle => false,
    };
    let count = |status: Status| sessions.iter().filter(|s| status_of(s, now) == status).count();
    let status = sessions
        .iter()
        .map(|s| status_of(s, now))
        .filter(|status| lights(*status))
        .max()
        .unwrap_or(Status::Idle);
    let lit = if status == Status::Idle {
        Vec::new()
    } else {
        sessions.iter().filter(|s| status_of(s, now) == status).collect()
    };
    Summary {
        status,
        lit,
        waiting: count(Status::Waiting),
        working: count(Status::Working),
        done: count(Status::Done),
    }
}

/// Sessions ordered for a dial to step through: most urgent first, then newest.
pub fn by_urgency(sessions: &[HookSession], now: i64) -> Vec<&HookSession> {
    let mut ordered: Vec<&HookSession> = sessions.iter().collect();
    ordered.sort_by_key(|s| (std::cmp::Reverse(status_of(s, now)), std::cmp::Reverse(s.updated_at)));
    ordered
}

/// Dismisses finished sessions: rewrites their status files as "cleared"
/// (the hook keeps them that way until the session does something new).
/// Returns how many were cleared.
pub fn clear_done(dir: &Path, sessions: &[HookSession], now: i64) -> usize {
    let mut cleared = 0;
    for session in sessions.iter().filter(|s| status_of(s, now) == Status::Done) {
        let file = dir.join(format!(
            "{}.json",
            crate::hook::sanitize_session_id(&session.session_id)
        ));
        let Some(mut record) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        record["state"] = Value::from("cleared");
        record["event"] = Value::from("Cleared");
        if paths::write_atomic(&file, record.to_string().as_bytes()).is_ok() {
            cleared += 1;
        }
    }
    cleared
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

    fn session(id: &str, state: &str, updated_at: i64) -> HookSession {
        HookSession {
            session_id: id.into(),
            state: state.into(),
            project: id.into(),
            updated_at,
            pids: vec![],
        }
    }

    #[test]
    fn status_mapping_and_working_expiry() {
        let now = 10 * WORKING_EXPIRY_MS;
        assert_eq!(status_of(&session("a", "waiting", 0), now), Status::Waiting);
        assert_eq!(status_of(&session("a", "working", now - 1000), now), Status::Working);
        assert_eq!(
            status_of(&session("a", "working", now - WORKING_EXPIRY_MS), now),
            Status::Idle
        );
        assert_eq!(status_of(&session("a", "done", 0), now), Status::Done);
        assert_eq!(status_of(&session("a", "idle", 0), now), Status::Done);
        assert_eq!(status_of(&session("a", "cleared", 0), now), Status::Idle);
    }

    #[test]
    fn summary_precedence_respects_light_up() {
        let now = 1_000_000;
        // Newest first, as list_hook_sessions returns them.
        let all = [
            session("done-new", "done", now - 10),
            session("working", "working", now - 20),
            session("waiting-new", "waiting", now - 30),
            session("waiting-old", "waiting", now - 40),
            session("cleared", "cleared", now - 50),
        ];
        let off = summarize(&all, LightUp::default(), now);
        assert_eq!(off.status, Status::Waiting);
        assert_eq!(
            off.lit.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(),
            ["waiting-new", "waiting-old"]
        );
        assert_eq!((off.waiting, off.working, off.done), (2, 1, 1));

        let rest = &all[..2];
        assert_eq!(summarize(rest, LightUp::default(), now).status, Status::Idle);
        assert!(summarize(rest, LightUp::default(), now).lit.is_empty());
        let both = LightUp {
            working: true,
            done: true,
        };
        assert_eq!(summarize(rest, both, now).status, Status::Working);
        assert_eq!(summarize(&all[..1], both, now).status, Status::Done);
        assert_eq!(
            summarize(
                &all[..1],
                LightUp {
                    working: true,
                    done: false
                },
                now
            )
            .status,
            Status::Idle
        );

        let order: Vec<_> = by_urgency(&all, now).iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(order, ["waiting-new", "waiting-old", "working", "done-new", "cleared"]);
    }

    #[test]
    fn clearing_done_sessions_rewrites_their_files() {
        let dir = std::env::temp_dir().join(format!("claude-opendeck-clear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (id, state) in [("a", "done"), ("b", "idle"), ("c", "waiting")] {
            std::fs::write(
                dir.join(format!("{id}.json")),
                json!({"sessionId": id, "state": state}).to_string(),
            )
            .unwrap();
        }
        let sessions = [
            session("a", "done", 0),
            session("b", "idle", 0),
            session("c", "waiting", 0),
        ];
        assert_eq!(clear_done(&dir, &sessions, 0), 2);
        let state = |id: &str| -> String {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{id}.json"))).unwrap()).unwrap();
            v["state"].as_str().unwrap().to_owned()
        };
        assert_eq!(
            (state("a"), state("b"), state("c")),
            ("cleared".into(), "cleared".into(), "waiting".into())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
