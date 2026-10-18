//! Threshold alerts: flash a usage key (and send a desktop notification) when
//! a window it shows crosses one of its thresholds upward. Each window and
//! threshold alerts once until the window resets, across restarts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::usage::{Usage, Window};

/// `resets_at` jitters by up to a second between responses; bucket it so one
/// window is one alert key.
pub const RESET_BUCKET_MS: i64 = 10 * 60 * 1000;

/// The windows a usage key shows (display label, window), for rendering and alerts.
pub fn windows_for(usage: &Usage, mode: &str) -> Vec<(String, Window)> {
    let named = |label: &str, window: &Option<Window>| window.clone().map(|w| (label.to_owned(), w));
    match mode {
        "session" => named("Session", &usage.session).into_iter().collect(),
        "weekly" => named("Weekly", &usage.weekly).into_iter().collect(),
        "model" => usage
            .scoped
            .first()
            .map(|w| (w.label.clone(), w.clone()))
            .into_iter()
            .collect(),
        _ => named("Session", &usage.session)
            .into_iter()
            .chain(named("Weekly", &usage.weekly))
            .collect(),
    }
}

/// What one usage key shows: (display label, window) pairs, and its thresholds (0 = off).
pub type Shown = (Vec<(String, Window)>, Vec<f64>);

#[derive(Debug, Clone, PartialEq)]
pub struct Crossing {
    /// Index of the key (in the slice passed to `evaluate`) to flash.
    pub entry: usize,
    pub label: String,
    pub percent: f64,
    pub threshold: f64,
    pub resets_at: i64,
    /// First key to see this crossing this round; only it sends a notification.
    pub notify: bool,
}

pub struct AlertState {
    file: Option<PathBuf>,
    /// window key -> last seen percent, to detect upward crossings
    last_percents: HashMap<String, f64>,
    /// alert keys already fired: `label|bucket|threshold`
    alerted: HashSet<String>,
}

fn bucket(resets_at: i64) -> i64 {
    (resets_at as f64 / RESET_BUCKET_MS as f64).round() as i64
}

impl AlertState {
    pub fn new(file: Option<PathBuf>) -> Self {
        let alerted = file
            .as_deref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
            .unwrap_or_default()
            .into_iter()
            .collect();
        Self {
            file,
            last_percents: HashMap::new(),
            alerted,
        }
    }

    /// `entries` are the usage keys' shown windows and thresholds (0 = off).
    pub fn evaluate(&mut self, entries: &[Shown], now: i64) -> Vec<Crossing> {
        let mut crossings = Vec::new();
        let mut fired: HashSet<String> = HashSet::new();
        for (index, (windows, thresholds)) in entries.iter().enumerate() {
            for (label, window) in windows {
                let Some(resets_at) = window.resets_at else { continue };
                let window_key = format!("{label}|{}", bucket(resets_at));
                let previous = self.last_percents.get(&window_key).copied();
                for &threshold in thresholds.iter().filter(|t| **t > 0.0) {
                    let key = format!("{window_key}|{threshold}");
                    if self.alerted.contains(&key) && !fired.contains(&key) {
                        continue;
                    }
                    // Only an observed upward crossing alerts, not starting up already above it.
                    let Some(previous) = previous else { continue };
                    if previous >= threshold || window.percent < threshold {
                        continue;
                    }
                    let notify = fired.insert(key.clone());
                    self.alerted.insert(key);
                    crossings.push(Crossing {
                        entry: index,
                        label: label.clone(),
                        percent: window.percent,
                        threshold,
                        resets_at,
                        notify,
                    });
                }
            }
        }
        for (windows, _) in entries {
            for (label, window) in windows {
                if let Some(resets_at) = window.resets_at {
                    self.last_percents
                        .insert(format!("{label}|{}", bucket(resets_at)), window.percent);
                }
            }
        }
        if !fired.is_empty() {
            self.save(now);
        }
        crossings
    }

    /// Persists fired alerts, forgetting windows that have already reset.
    fn save(&mut self, now: i64) {
        self.alerted.retain(|key| {
            key.split('|')
                .nth(1)
                .and_then(|b| b.parse::<i64>().ok())
                .is_some_and(|b| b * RESET_BUCKET_MS > now - RESET_BUCKET_MS)
        });
        let Some(file) = self.file.as_deref() else { return };
        let mut keys: Vec<&String> = self.alerted.iter().collect();
        keys.sort();
        if let Err(error) = write_keys(file, &keys) {
            log::warn!("could not save alert state: {error}");
        }
    }
}

fn write_keys(file: &Path, keys: &[&String]) -> std::io::Result<()> {
    crate::paths::write_atomic(file, serde_json::to_string(keys).unwrap_or_default().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(percent: f64, resets_at: i64) -> Window {
        Window {
            label: "5h".into(),
            percent,
            resets_at: Some(resets_at),
            window_ms: 1,
        }
    }

    const RESET: i64 = 1_800_000_000_000;

    #[test]
    fn alerts_once_on_upward_crossing() {
        let mut state = AlertState::new(None);
        let at = |p: f64| vec![(vec![("Session".to_string(), window(p, RESET))], vec![80.0, 95.0])];
        // First observation never alerts, even above a threshold.
        assert!(state.evaluate(&at(85.0), 0).is_empty());
        assert!(state.evaluate(&at(70.0), 0).is_empty());
        let crossed = state.evaluate(&at(82.0), 0);
        assert_eq!(crossed.len(), 1);
        assert_eq!((crossed[0].threshold, crossed[0].notify), (80.0, true));
        // Dropping and re-crossing the same window doesn't repeat it.
        assert!(state.evaluate(&at(70.0), 0).is_empty());
        assert!(state.evaluate(&at(90.0), 0).is_empty());
        // Next threshold still fires.
        assert_eq!(state.evaluate(&at(96.0), 0)[0].threshold, 95.0);
    }

    #[test]
    fn reset_jitter_is_one_window_and_new_window_alerts_again() {
        let mut state = AlertState::new(None);
        let at = |p: f64, reset: i64| vec![(vec![("Weekly".to_string(), window(p, reset))], vec![80.0])];
        state.evaluate(&at(70.0, RESET), 0);
        assert_eq!(
            state.evaluate(&at(81.0, RESET + 700), 0).len(),
            1,
            "jitter stays in the bucket"
        );
        state.evaluate(&at(70.0, RESET + 900), 0);
        assert!(state.evaluate(&at(85.0, RESET - 400), 0).is_empty());
        // After the reset, a new window can alert again.
        let next = RESET + 7 * 24 * 3_600_000;
        state.evaluate(&at(10.0, next), 0);
        assert_eq!(state.evaluate(&at(80.0, next), 0).len(), 1);
    }

    #[test]
    fn two_keys_flash_but_notify_once() {
        let mut state = AlertState::new(None);
        let keys = |p: f64| {
            let shown = vec![("Session".to_string(), window(p, RESET))];
            vec![(shown.clone(), vec![80.0]), (shown, vec![80.0])]
        };
        state.evaluate(&keys(50.0), 0);
        let crossed = state.evaluate(&keys(80.0), 0);
        assert_eq!(
            crossed.iter().map(|c| (c.entry, c.notify)).collect::<Vec<_>>(),
            [(0, true), (1, false)]
        );
    }

    #[test]
    fn persists_and_prunes() {
        let file = std::env::temp_dir().join(format!("claude-opendeck-alerts-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let mut state = AlertState::new(Some(file.clone()));
        let at = |p: f64| vec![(vec![("Session".to_string(), window(p, RESET))], vec![80.0])];
        state.evaluate(&at(50.0), 0);
        state.evaluate(&at(90.0), RESET - 1000);
        let reloaded = AlertState::new(Some(file.clone()));
        assert_eq!(reloaded.alerted.len(), 1);
        // A save long after the reset drops the key.
        let mut later = AlertState::new(Some(file.clone()));
        later.alerted.insert("Weekly|1|80".into());
        later.save(RESET + RESET_BUCKET_MS * 3);
        assert!(later.alerted.is_empty());
        let _ = std::fs::remove_file(file);
    }
}
