//! Where Claude Code keeps its files, and where this plugin keeps its own.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Claude Code's config directory (`$CLAUDE_CONFIG_DIR`, default `~/.claude`).
pub fn claude_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".claude"),
    }
}

/// This plugin's cache directory: `~/.cache/claude-opendeck` on Linux
/// (honouring `$XDG_CACHE_HOME`), `~/Library/Caches/claude-opendeck` on macOS,
/// `%LOCALAPPDATA%\claude-opendeck` on Windows.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| home().join(".cache"))
        .join("claude-opendeck")
}

/// Per-session status files written by the `hook` subcommand.
pub fn status_dir() -> PathBuf {
    cache_dir().join("sessions")
}

/// Threshold alerts already fired, so restarts don't repeat them.
pub fn alerts_file() -> PathBuf {
    cache_dir().join("alerts.json")
}

/// Expands a leading `~` to the home directory.
pub fn expand_home(dir: &str) -> PathBuf {
    let dir = dir.trim();
    if dir == "~" {
        return home();
    }
    if let Some(rest) = dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")) {
        return home().join(rest);
    }
    PathBuf::from(dir)
}

/// Last path component as a display name ("" for a bare root).
pub fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Writes via a temp file and rename so readers never see a partial file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}
