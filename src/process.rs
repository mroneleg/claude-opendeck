//! Process ancestry and liveness, used to tie hook events to the Claude Code
//! process (and the terminal or editor window around it).
//!
//! Linux reads `/proc` directly. Elsewhere `sysinfo` provides the same facts.

/// An opaque process start marker that guards against pid reuse. On Linux it
/// is field 22 of `/proc/<pid>/stat` (the same value Claude Code writes as
/// `procStart` in `~/.claude/sessions/<pid>.json`); elsewhere it is the start
/// time in seconds as reported by `sysinfo`.
pub fn start_marker(pid: u32) -> Option<String> {
    imp::start_marker(pid)
}

/// Whether `pid` is running and, if `marker` is given, is the same process.
pub fn is_alive(pid: u32, marker: Option<&str>) -> bool {
    match start_marker(pid) {
        None => false,
        Some(actual) => marker.is_none_or(|expected| expected.is_empty() || expected == actual),
    }
}

/// Whether `pid` is running, ignoring start markers. For markers written by
/// another program whose format is only known on Linux.
pub fn is_running(pid: u32) -> bool {
    start_marker(pid).is_some()
}

/// `pid`'s ancestors from its parent outwards, stopping before pid 1.
pub fn ancestors(pid: u32) -> Vec<u32> {
    imp::ancestors(pid)
}

#[cfg(target_os = "linux")]
mod imp {
    /// Fields after the `(comm)` part of /proc/<pid>/stat, which may contain spaces.
    fn stat_fields(pid: u32) -> Option<Vec<String>> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = stat.get(stat.rfind(')')? + 2..)?;
        Some(after.split(' ').map(str::to_owned).collect())
    }

    pub fn start_marker(pid: u32) -> Option<String> {
        // Field 22 overall; index 19 after pid and comm.
        stat_fields(pid)?.get(19).cloned()
    }

    fn parent_of(pid: u32) -> Option<u32> {
        stat_fields(pid)?.get(1)?.parse().ok()
    }

    pub fn ancestors(pid: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut current = parent_of(pid);
        while let Some(p) = current {
            if p <= 1 || chain.contains(&p) {
                break;
            }
            chain.push(p);
            current = parent_of(p);
        }
        chain
    }
}

#[cfg(any(not(target_os = "linux"), test))]
mod portable {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    fn system_with(pids: Option<&[Pid]>) -> System {
        let mut system = System::new();
        let which = match pids {
            Some(pids) => ProcessesToUpdate::Some(pids),
            None => ProcessesToUpdate::All,
        };
        system.refresh_processes_specifics(which, true, ProcessRefreshKind::nothing());
        system
    }

    pub fn start_marker(pid: u32) -> Option<String> {
        let pid = Pid::from_u32(pid);
        let system = system_with(Some(&[pid]));
        system.process(pid).map(|p| p.start_time().to_string())
    }

    pub fn ancestors(pid: u32) -> Vec<u32> {
        let system = system_with(None);
        let mut chain = Vec::new();
        let mut current = system.process(Pid::from_u32(pid)).and_then(|p| p.parent());
        while let Some(parent) = current {
            let raw = parent.as_u32();
            if raw <= 1 || chain.contains(&raw) {
                break;
            }
            chain.push(raw);
            current = system.process(parent).and_then(|p| p.parent());
        }
        chain
    }
}

#[cfg(not(target_os = "linux"))]
use portable as imp;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_with_matching_marker() {
        let pid = std::process::id();
        let marker = start_marker(pid).expect("own start marker");
        assert!(is_alive(pid, Some(&marker)));
        assert!(is_alive(pid, None));
        assert!(!is_alive(pid, Some("not-the-start-time")));
        assert!(is_running(pid));
    }

    #[test]
    fn missing_process_is_dead() {
        // Above the default pid_max, so never a real process.
        assert!(!is_alive(4_000_000_000, None));
    }

    #[test]
    fn ancestry_includes_parent() {
        let chain = ancestors(std::process::id());
        assert!(!chain.is_empty());
        assert!(!chain.contains(&1));
    }

    #[test]
    fn portable_implementation_agrees_on_ancestry() {
        // The sysinfo code path used on macOS/Windows, exercised on Linux too.
        let pid = std::process::id();
        assert!(portable::start_marker(pid).is_some());
        assert_eq!(portable::ancestors(pid).first(), ancestors(pid).first());
    }
}
