//! `claude-opendeck usage [--json]`: plan usage in the terminal.

use chrono::TimeZone;

use crate::usage::{self, Window};

fn resets(window: &Window) -> String {
    window
        .resets_at
        .and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single())
        .map_or_else(|| "n/a".into(), |time| time.format("%Y-%m-%d %H:%M").to_string())
}

/// Prints usage; returns the process exit code.
pub fn usage(json: bool) -> i32 {
    let usage = match usage::fetch_usage() {
        Ok(usage) => usage,
        Err(error) => {
            eprintln!("claude-opendeck: {error}");
            return 1;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&usage).unwrap_or_default());
        return 0;
    }
    let rows = [
        ("Session (5h)", usage.session.as_ref()),
        ("Weekly (7d)", usage.weekly.as_ref()),
    ];
    for (name, window) in rows {
        if let Some(window) = window {
            println!(
                "{name:<13} {:>3}%  resets {}",
                window.percent.round() as i64,
                resets(window)
            );
        }
    }
    for window in &usage.scoped {
        println!(
            "{:<13} {:>3}%  resets {}",
            format!("{} (7d)", window.label),
            window.percent.round() as i64,
            resets(window)
        );
    }
    if let Some(spend) = usage.spend.as_ref().filter(|s| s.enabled) {
        let limit = spend
            .limit
            .map_or_else(String::new, |l| format!(" of {l:.2} {}", spend.currency));
        println!("Extra usage   {:.2} {}{limit}", spend.used, spend.currency);
    }
    0
}
