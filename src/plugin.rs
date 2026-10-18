//! The OpenDeck / Stream Deck plugin: seven actions sharing one usage fetch,
//! one session scan and one transcript scan.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use openaction::{Action, ActionUuid, Instance, OpenActionResult, async_trait, get_instance, register_action};
use serde_json::{Value, json};

use crate::activity::{Activity, Totals};
use crate::alerts::{self, AlertState};
use crate::sessions::{self, ActiveSession, HookSession, LightUp, Status};
use crate::theme::{self, Badge, Theme};
use crate::usage::{self, Bridge, Usage, UsageError};
use crate::{focus, hooks_install, launch, notify, paths, render, statusline};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Usage,
    Breakdown,
    ExtraUsage,
    NeedsYou,
    Sessions,
    Activity,
    Launch,
}

impl Kind {
    fn uses_usage(self) -> bool {
        matches!(self, Kind::Usage | Kind::Breakdown | Kind::ExtraUsage)
    }

    fn uses_sessions(self) -> bool {
        matches!(self, Kind::NeedsYou | Kind::Sessions)
    }
}

// The usage endpoint is undocumented; don't hammer it.
const MIN_REFRESH_MS: i64 = 60 * 1000;
const MANUAL_REFRESH_MS: i64 = 10 * 1000;
// Countdowns are minute-granular, so redraw from cache once a minute.
const REDRAW: Duration = Duration::from_secs(60);
// Due fetches (including backoff retries) are checked this often.
const SCHEDULE: Duration = Duration::from_secs(5);
const SESSIONS_POLL: Duration = Duration::from_secs(5);
// Hook status files change via write-then-rename, which updates the
// directory's mtime; checking it this often makes the key feel instant.
const STATUS_DIR_POLL: Duration = Duration::from_millis(250);
const STATUSLINE_POLL: Duration = Duration::from_secs(2);
const ACTIVITY_REFRESH_MS: i64 = 60 * 1000;
const FLASH_MS: i64 = 6000;
const FLASH_FRAME_MS: i64 = 500;
/// Held at least this long, a key press is a "hold" (refresh / open) rather than a tap.
const HOLD_MS: u64 = 600;
/// Numbers older than this get a "Xm ago" badge.
pub const STALE_MS: i64 = 15 * 60 * 1000;
const MODES: [&str; 4] = ["both", "session", "weekly", "model"];

/// Where a usage key's 5h/7d numbers come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataSource {
    /// The usage endpoint, overridden by fresher status line figures.
    Api,
    /// Only Claude Code's status line: never reads the login token.
    StatusLine,
}

#[derive(Debug, Clone, PartialEq)]
struct Settings {
    mode: String,
    refresh_minutes: f64,
    alert_low: f64,
    alert_high: f64,
    project_dir: String,
    terminal_command: String,
    data_source: DataSource,
    light: LightUp,
    theme: Theme,
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl Settings {
    /// Lenient: missing or malformed fields fall back to defaults.
    fn from_value(value: &Value) -> Self {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let flag = |key: &str| value.get(key).and_then(Value::as_bool);
        Settings {
            mode: text("mode")
                .filter(|m| MODES.contains(&m.as_str()))
                .unwrap_or_else(|| "both".into()),
            refresh_minutes: number(value.get("refreshMinutes")).filter(|n| *n > 0.0).unwrap_or(5.0),
            alert_low: number(value.get("alertLow")).unwrap_or(80.0),
            alert_high: number(value.get("alertHigh")).unwrap_or(95.0),
            project_dir: text("projectDir").unwrap_or_default(),
            terminal_command: text("terminalCommand")
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| launch::default_command().into()),
            data_source: if text("dataSource").as_deref() == Some("statusline") {
                DataSource::StatusLine
            } else {
                DataSource::Api
            },
            light: LightUp {
                working: flag("lightWorking").unwrap_or(false),
                // `includeIdle` is the 1.0 name for lighting up finished sessions.
                done: flag("lightDone").or_else(|| flag("includeIdle")).unwrap_or(false),
            },
            theme: Theme::from_settings(
                &text("background").unwrap_or_default(),
                &text("accent").unwrap_or_default(),
            ),
        }
    }
}

/// The display mode after `mode`, stepping `steps` (negative = backwards).
fn cycle_mode(mode: &str, steps: i64) -> &'static str {
    let index = MODES.iter().position(|m| *m == mode).unwrap_or(0) as i64;
    MODES[(index + steps).rem_euclid(MODES.len() as i64) as usize]
}

#[derive(Debug, Clone)]
struct Flash {
    until: i64,
    label: String,
    percent: f64,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    seq: u64,
    held: bool,
}

struct Entry {
    kind: Kind,
    settings: Settings,
    /// The settings as OpenDeck sent them, so a mode change keeps unknown keys.
    raw: Value,
    encoder: bool,
    in_multi_action: bool,
    last_image: Option<String>,
    flash: Option<Flash>,
    press: Option<Press>,
    /// Session a dial is pointing at (index into `sessions::by_urgency`).
    selected: usize,
}

struct State {
    entries: HashMap<String, Entry>,
    /// Last successful API response (possibly from the cache on disk).
    usage: Option<Usage>,
    last_error: Option<UsageError>,
    last_fetch_at: i64,
    failures: u32,
    next_attempt_at: i64,
    bridge: Option<Bridge>,
    active: Vec<ActiveSession>,
    hooks: Vec<HookSession>,
    activity: Option<Totals>,
    activity_fetched_at: i64,
    alerts: AlertState,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        entries: HashMap::new(),
        usage: None,
        last_error: None,
        last_fetch_at: 0,
        failures: 0,
        next_attempt_at: 0,
        bridge: None,
        active: Vec::new(),
        hooks: Vec::new(),
        activity: None,
        activity_fetched_at: 0,
        alerts: AlertState::new(Some(paths::alerts_file())),
    })
});

static ACTIVITY: LazyLock<Mutex<Activity>> = LazyLock::new(|| Mutex::new(Activity::default()));
static PRESS_SEQ: AtomicU64 = AtomicU64::new(0);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn has_kind(state: &State, test: impl Fn(Kind) -> bool) -> bool {
    state.entries.values().any(|e| test(e.kind))
}

/// Whether any key needs the usage endpoint (status-line-only keys don't).
fn wants_api(state: &State) -> bool {
    state
        .entries
        .values()
        .any(|e| e.kind.uses_usage() && e.settings.data_source == DataSource::Api)
}

// --- what a usage key shows ------------------------------------------------------------

/// The usage numbers a key shows: API data with fresher status line figures
/// on top, or status line figures alone.
fn shown_usage(state: &State, source: DataSource) -> Option<Usage> {
    match (source, &state.usage, &state.bridge) {
        (DataSource::StatusLine, _, Some(bridge)) => Some(usage::usage_from_bridge(bridge)),
        (DataSource::StatusLine, _, None) => None,
        (DataSource::Api, Some(api), Some(bridge)) => Some(usage::merge_bridge(api, bridge)),
        (DataSource::Api, Some(api), None) => Some(api.clone()),
        // The API has nothing (yet, or failing): the status line still helps.
        (DataSource::Api, None, Some(bridge)) => Some(usage::usage_from_bridge(bridge)),
        (DataSource::Api, None, None) => None,
    }
}

/// Short label for a transient fetch problem, shown while older numbers stay up.
fn trouble_label(error: &UsageError) -> &'static str {
    match error {
        UsageError::NoLogin => "no login",
        UsageError::Expired(_) => "log in",
        UsageError::RateLimited(_) => "rate limited",
        UsageError::Http(_) => "server error",
        UsageError::Network(_) => "offline",
    }
}

/// Red while fetches are failing, amber once the numbers are getting old.
fn badge_for(state: &State, usage: &Usage, source: DataSource, now: i64) -> Option<Badge> {
    let age = now - usage.fetched_at;
    let failing = source == DataSource::Api && usage.fetched_at <= state.last_fetch_at;
    match &state.last_error {
        Some(error) if failing => Some(Badge {
            color: render::DANGER,
            text: trouble_label(error).into(),
        }),
        _ if age > STALE_MS => Some(Badge {
            color: render::WARN,
            text: render::format_age(age),
        }),
        _ => None,
    }
}

/// (line1, line2, colour) when a usage key has no numbers to show.
fn no_data_message(state: &State, source: DataSource, now: i64) -> Option<(String, String, &'static str)> {
    if source == DataSource::StatusLine {
        return Some(("Status line".into(), "no data yet".into(), render::MUTED));
    }
    let error = state.last_error.as_ref()?;
    Some(match error {
        UsageError::NoLogin => ("Claude".into(), "not logged in".into(), render::DANGER),
        UsageError::Expired(_) => ("Claude".into(), "open Claude Code".into(), render::CLAUDE),
        UsageError::RateLimited(_) => (
            "Rate limited".into(),
            format!(
                "retry in {}",
                render::format_countdown(Some(state.next_attempt_at), now)
            ),
            render::WARN,
        ),
        UsageError::Http(status) => ("Claude".into(), format!("server error {status}"), render::DANGER),
        UsageError::Network(_) => ("Claude".into(), "offline".into(), render::DANGER),
    })
}

fn usage_key_image(state: &State, entry: &Entry, now: i64) -> Option<String> {
    let source = entry.settings.data_source;
    let theme = &entry.settings.theme;
    let Some(usage) = shown_usage(state, source) else {
        let (line1, line2, color) = no_data_message(state, source, now)?;
        let image = if entry.encoder {
            render::message_strip(&line1, &line2, color)
        } else {
            render::message_image(&line1, &line2, color)
        };
        return Some(theme::finish(&image, theme, None));
    };
    // Per-model, breakdown and spend only come from the API.
    let api_only = entry.kind != Kind::Usage || entry.settings.mode == "model";
    if api_only && state.usage.is_none() {
        let title = match entry.kind {
            Kind::Breakdown => "Breakdown",
            Kind::ExtraUsage => "Extra usage",
            _ => "Per-model",
        };
        let image = if entry.encoder {
            render::message_strip(title, "needs API", render::MUTED)
        } else {
            render::message_image(title, "needs API", render::MUTED)
        };
        return Some(theme::finish(&image, theme, None));
    }
    let image = match entry.kind {
        Kind::Breakdown => render::breakdown_image(&usage.breakdown),
        Kind::ExtraUsage => render::spend_image(usage.spend.as_ref()),
        _ if entry.encoder => render::usage_strip(&usage, &entry.settings.mode, now),
        _ => render::usage_image(&usage, &entry.settings.mode, now),
    };
    Some(theme::finish(
        &image,
        theme,
        badge_for(state, &usage, source, now).as_ref(),
    ))
}

fn status_key_image(state: &State, entry: &Entry, now: i64) -> String {
    let theme = &entry.settings.theme;
    if entry.encoder {
        let ordered = sessions::by_urgency(&state.hooks, now);
        let image = match ordered.get(entry.selected.min(ordered.len().saturating_sub(1))) {
            Some(session) => render::session_strip(
                sessions::status_of(session, now),
                &session.project,
                entry.selected.min(ordered.len() - 1),
                ordered.len(),
            ),
            None => render::message_strip("Claude", "no sessions", render::MUTED),
        };
        return theme::finish(&image, theme, None);
    }
    let summary = sessions::summarize(&state.hooks, entry.settings.light, now);
    let project = summary.lit.first().map(|s| s.project.as_str());
    let image = match summary.status {
        Status::Waiting => render::needs_you_image(summary.lit.len(), project, summary.working),
        Status::Working | Status::Done => render::status_image(summary.status, summary.lit.len(), project),
        Status::Idle => render::all_clear_image(summary.working, summary.done),
    };
    theme::finish(&image, theme, None)
}

fn launch_dir(settings: &Settings) -> std::path::PathBuf {
    if settings.project_dir.trim().is_empty() {
        dirs::home_dir().unwrap_or_default()
    } else {
        paths::expand_home(&settings.project_dir)
    }
}

fn compute_image(state: &State, entry: &mut Entry, now: i64) -> Option<String> {
    if let Some(flash) = &entry.flash {
        if now >= flash.until {
            entry.flash = None;
        } else if (now / FLASH_FRAME_MS) % 2 == 0 {
            return Some(render::alert_image(&flash.label, flash.percent));
        }
    }
    let theme = &entry.settings.theme;
    match entry.kind {
        Kind::NeedsYou => Some(status_key_image(state, entry, now)),
        Kind::Sessions => Some(theme::finish(
            &render::sessions_image(
                state.active.len(),
                state.active.iter().filter(|s| s.status == "busy").count(),
            ),
            theme,
            None,
        )),
        Kind::Activity => state
            .activity
            .as_ref()
            .map(|t| theme::finish(&render::activity_image(t), theme, None)),
        Kind::Launch => Some(theme::finish(
            &render::launch_image(&paths::basename(&launch_dir(&entry.settings))),
            theme,
            None,
        )),
        _ => usage_key_image(state, entry, now),
    }
}

/// Re-renders the given instances (all if None), sending only changed images.
async fn render(ids: Option<Vec<String>>) {
    let updates: Vec<(String, String, bool)> = {
        let mut guard = state();
        let state = &mut *guard;
        let now = paths::now_ms();
        let ids = ids.unwrap_or_else(|| state.entries.keys().cloned().collect());
        let mut entries = std::mem::take(&mut state.entries);
        let mut updates = Vec::new();
        for id in ids {
            let Some(entry) = entries.get_mut(&id) else { continue };
            if let Some(image) = compute_image(state, entry, now)
                && entry.last_image.as_deref() != Some(image.as_str())
            {
                entry.last_image = Some(image.clone());
                updates.push((id, image, entry.encoder));
            }
        }
        state.entries = entries;
        updates
    };
    for (id, image, encoder) in updates {
        if let Some(instance) = get_instance(id).await {
            if encoder {
                // "$A0" layout: a full-canvas image, with its title blanked.
                let _ = instance.set_feedback(&json!({"full-canvas": image, "title": ""})).await;
            } else {
                let _ = instance.set_image(Some(image), None).await;
            }
        }
    }
}

async fn render_all() {
    render(None).await;
}

// --- data refresh ---------------------------------------------------------------

static USAGE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static SESSIONS_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ACTIVITY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn refresh_interval_ms(state: &State) -> i64 {
    // All usage keys share one fetch, so honour the most eager key's setting.
    let minutes = state
        .entries
        .values()
        .filter(|e| e.kind.uses_usage())
        .map(|e| e.settings.refresh_minutes)
        .fold(f64::INFINITY, f64::min);
    let minutes = if minutes.is_finite() { minutes } else { 5.0 };
    ((minutes * 60_000.0) as i64).max(MIN_REFRESH_MS)
}

/// When to try next after a fetch: the normal interval after a success or a
/// local login problem (retrying sooner won't help), backing off otherwise.
fn schedule_after(state: &mut State, result: &Result<(), UsageError>, now: i64) {
    let interval = refresh_interval_ms(state);
    match result {
        Ok(()) => {
            state.failures = 0;
            state.next_attempt_at = now + interval;
        }
        Err(error) if error.is_login_problem() => {
            state.failures = 0;
            state.next_attempt_at = now + interval;
        }
        Err(error) => {
            state.failures = state.failures.saturating_add(1);
            let retry_after = match error {
                UsageError::RateLimited(retry) => *retry,
                _ => None,
            };
            let wait = usage::backoff_ms(interval, state.failures, retry_after);
            state.next_attempt_at = now + wait;
            log::info!(
                "retrying usage in {}s ({} failure(s) in a row)",
                wait / 1000,
                state.failures
            );
        }
    }
}

async fn refresh_usage() {
    if USAGE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let result = tokio::task::spawn_blocking(usage::fetch_usage_body)
        .await
        .unwrap_or_else(|error| Err(UsageError::Network(error.to_string())));
    let mut notifications = Vec::new();
    {
        let mut guard = state();
        let state = &mut *guard;
        let now = paths::now_ms();
        let outcome = match result {
            Ok(body) => {
                usage::save_cached(&body, now);
                state.usage = Some(usage::parse_usage(&body, now));
                state.last_error = None;
                Ok(())
            }
            Err(error) => {
                log::warn!("usage fetch failed: {error}");
                state.last_error = Some(error.clone());
                Err(error)
            }
        };
        state.last_fetch_at = now;
        schedule_after(state, &outcome, now);
        if outcome.is_ok() {
            notifications = check_thresholds(state);
        }
    }
    USAGE_IN_FLIGHT.store(false, Ordering::SeqCst);
    send_notifications(notifications).await;
    render_all().await;
}

async fn send_notifications(notifications: Vec<(String, String)>) {
    for (title, body) in notifications {
        notify::send(&title, &body).await;
    }
}

/// Flashes usage keys whose shown windows crossed a threshold; returns notifications to send.
fn check_thresholds(state: &mut State) -> Vec<(String, String)> {
    let now = paths::now_ms();
    let shown: Vec<(String, alerts::Shown)> = state
        .entries
        .iter()
        .filter(|(_, e)| e.kind == Kind::Usage)
        .filter_map(|(id, e)| {
            let usage = shown_usage(state, e.settings.data_source)?;
            Some((
                id.clone(),
                (
                    alerts::windows_for(&usage, &e.settings.mode),
                    vec![e.settings.alert_low, e.settings.alert_high],
                ),
            ))
        })
        .collect();
    let (ids, shown): (Vec<String>, Vec<alerts::Shown>) = shown.into_iter().unzip();
    let crossings = state.alerts.evaluate(&shown, now);
    let mut notifications = Vec::new();
    for crossing in crossings {
        if let Some(entry) = state.entries.get_mut(&ids[crossing.entry]) {
            entry.flash = Some(Flash {
                until: now + FLASH_MS,
                label: crossing.label.clone(),
                percent: crossing.percent,
            });
        }
        if crossing.notify {
            notifications.push((
                format!(
                    "Claude {} usage at {}%",
                    crossing.label.to_lowercase(),
                    crossing.percent.round() as i64
                ),
                format!(
                    "Crossed {}%. Resets in {}.",
                    crossing.threshold,
                    render::format_countdown(Some(crossing.resets_at), now)
                ),
            ));
        }
    }
    notifications
}

/// Reloads the status line figures; true if they changed.
async fn refresh_bridge() -> bool {
    let bridge = tokio::task::spawn_blocking(|| statusline::load_bridge(paths::now_ms()))
        .await
        .unwrap_or(None);
    let notifications = {
        let mut state = state();
        if state.bridge == bridge {
            return false;
        }
        state.bridge = bridge;
        check_thresholds(&mut state)
    };
    send_notifications(notifications).await;
    true
}

async fn refresh_sessions() {
    if SESSIONS_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let result = tokio::task::spawn_blocking(|| {
        (
            sessions::list_active_sessions(),
            sessions::list_hook_sessions(paths::now_ms()),
        )
    })
    .await;
    if let Ok((active, hooks)) = result {
        let mut state = state();
        state.active = active;
        state.hooks = hooks;
    }
    SESSIONS_IN_FLIGHT.store(false, Ordering::SeqCst);
    render_all().await;
}

async fn refresh_activity() {
    if ACTIVITY_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let result = tokio::task::spawn_blocking(|| {
        let mut activity = ACTIVITY.lock().unwrap_or_else(|p| p.into_inner());
        activity.today(&paths::claude_dir().join("projects"), chrono::Local::now().date_naive())
    })
    .await;
    {
        let mut state = state();
        if let Ok(totals) = result {
            state.activity = Some(totals);
        }
        state.activity_fetched_at = paths::now_ms();
    }
    ACTIVITY_IN_FLIGHT.store(false, Ordering::SeqCst);
    render_all().await;
}

async fn refresh_for(kind: Kind) {
    if kind.uses_sessions() {
        refresh_sessions().await;
    } else if kind == Kind::Activity && state().activity.is_none() {
        refresh_activity().await;
    } else if kind.uses_usage() {
        refresh_bridge().await;
        // Fetches only if one is due (a fresh cached response defers it).
        schedule_tick().await;
    }
}

/// Starts a usage fetch if one is due.
async fn schedule_tick() {
    let due = {
        let state = state();
        wants_api(&state) && paths::now_ms() >= state.next_attempt_at
    };
    if due {
        refresh_usage().await;
    } else {
        render_all().await;
    }
}

async fn tick() {
    let activity_due = {
        let state = state();
        has_kind(&state, |k| k == Kind::Activity) && paths::now_ms() - state.activity_fetched_at >= ACTIVITY_REFRESH_MS
    };
    if activity_due {
        tokio::spawn(refresh_activity());
    }
    render_all().await;
}

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Shows the cached response straight away, and defers the first fetch if
/// it's recent enough, so restarts don't cost an API call.
fn load_cache() {
    let Some(cached) = usage::load_cached() else {
        return;
    };
    let mut state = state();
    let interval = refresh_interval_ms(&state);
    state.next_attempt_at = cached.fetched_at + interval;
    state.last_fetch_at = cached.fetched_at;
    log::info!(
        "showing cached usage from {}",
        render::format_age(paths::now_ms() - cached.fetched_at)
    );
    state.usage = Some(cached);
}

/// Starts the background refresh loops.
fn spawn_loops() {
    tokio::spawn(async {
        let mut interval = tokio::time::interval(REDRAW);
        loop {
            interval.tick().await;
            tick().await;
        }
    });
    tokio::spawn(async {
        let mut interval = tokio::time::interval(SCHEDULE);
        loop {
            interval.tick().await;
            let due = {
                let state = state();
                wants_api(&state) && paths::now_ms() >= state.next_attempt_at
            };
            if due {
                refresh_usage().await;
            }
        }
    });
    tokio::spawn(async {
        let file = paths::statusline_file();
        let mut last = None;
        let mut interval = tokio::time::interval(STATUSLINE_POLL);
        loop {
            interval.tick().await;
            let current = mtime(&file);
            if current != last && has_kind(&state(), Kind::uses_usage) {
                last = current;
                if refresh_bridge().await {
                    render_all().await;
                }
            }
        }
    });
    tokio::spawn(async {
        let mut interval = tokio::time::interval(SESSIONS_POLL);
        loop {
            interval.tick().await;
            if has_kind(&state(), Kind::uses_sessions) {
                refresh_sessions().await;
            }
        }
    });
    tokio::spawn(async {
        let _ = std::fs::create_dir_all(paths::status_dir());
        let mut last = mtime(&paths::status_dir());
        let mut interval = tokio::time::interval(STATUS_DIR_POLL);
        loop {
            interval.tick().await;
            let current = mtime(&paths::status_dir());
            if current != last {
                last = current;
                if has_kind(&state(), Kind::uses_sessions) {
                    refresh_sessions().await;
                }
            }
        }
    });
    tokio::spawn(async {
        let mut interval = tokio::time::interval(Duration::from_millis(FLASH_FRAME_MS as u64));
        loop {
            interval.tick().await;
            let flashing = state().entries.values().any(|e| e.flash.is_some());
            if flashing {
                render_all().await;
            }
        }
    });
}

// --- events ---------------------------------------------------------------------

async fn on_appear(instance: &Instance, kind: Kind, settings: &Value) {
    let id = instance.instance_id.clone();
    let encoder = instance.controller == "Encoder";
    state().entries.insert(
        id.clone(),
        Entry {
            kind,
            settings: Settings::from_value(settings),
            raw: settings.clone(),
            encoder,
            in_multi_action: instance.is_in_multi_action,
            last_image: None,
            flash: None,
            press: None,
            selected: 0,
        },
    );
    if encoder {
        let _ = instance.set_feedback_layout("$A0".into()).await;
    }
    render(Some(vec![id])).await;
    tokio::spawn(refresh_for(kind));
}

fn on_disappear(instance: &Instance) {
    state().entries.remove(&instance.instance_id);
}

async fn on_settings(instance: &Instance, settings: &Value) {
    let id = instance.instance_id.clone();
    if let Some(entry) = state().entries.get_mut(&id) {
        entry.settings = Settings::from_value(settings);
        entry.raw = settings.clone();
        entry.last_image = None;
    }
    render(Some(vec![id])).await;
}

/// Brings `session`'s window to the front.
async fn focus(session: Option<HookSession>) -> anyhow::Result<()> {
    let session = session
        .filter(|s| !s.pids.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no session to focus (are the Claude Code hooks installed?)"))?;
    focus::focus_window(&session.pids, &session.project).await
}

/// Session-status key press: jump to a waiting session, else dismiss finished ones.
async fn status_press(settings: &Settings) -> anyhow::Result<()> {
    refresh_sessions().await;
    let (waiting, cleared) = {
        let state = state();
        let now = paths::now_ms();
        let summary = sessions::summarize(&state.hooks, settings.light, now);
        if summary.status == Status::Waiting {
            (summary.lit.first().map(|s| (*s).clone()), 0)
        } else {
            (None, sessions::clear_done(&paths::status_dir(), &state.hooks, now))
        }
    };
    if waiting.is_some() {
        return focus(waiting).await;
    }
    if cleared > 0 {
        refresh_sessions().await;
    }
    Ok(())
}

/// Session-status key hold: jump to the most relevant session, whatever its state.
async fn status_hold() -> anyhow::Result<()> {
    refresh_sessions().await;
    let target = {
        let state = state();
        sessions::by_urgency(&state.hooks, paths::now_ms())
            .first()
            .map(|s| (*s).clone())
    };
    focus(target).await
}

/// Dial press on a session-status key: jump to the session it points at.
async fn status_dial_press(id: &str) -> anyhow::Result<()> {
    let target = {
        let state = state();
        let selected = state.entries.get(id).map_or(0, |e| e.selected);
        let ordered = sessions::by_urgency(&state.hooks, paths::now_ms());
        ordered
            .get(selected.min(ordered.len().saturating_sub(1)))
            .map(|s| (*s).clone())
    };
    focus(target).await
}

/// Refreshes usage on request, still rate-limited so a stuck key can't spam
/// the API, and never ahead of a server's Retry-After. Returns whether it ran.
async fn manual_usage_refresh() -> Result<bool, UsageError> {
    refresh_bridge().await;
    let allowed = {
        let state = state();
        let now = paths::now_ms();
        let backing_off = matches!(state.last_error, Some(UsageError::RateLimited(_))) && now < state.next_attempt_at;
        wants_api(&state) && now - state.last_fetch_at >= MANUAL_REFRESH_MS && !backing_off
    };
    if !allowed {
        render_all().await;
        return Ok(false);
    }
    refresh_usage().await;
    match &state().last_error {
        Some(error) => Err(error.clone()),
        None => Ok(true),
    }
}

/// Cycles a usage key's display mode and saves it to the key's settings.
async fn cycle(instance: &Instance, steps: i64) {
    let id = instance.instance_id.clone();
    let raw = {
        let mut state = state();
        let Some(entry) = state.entries.get_mut(&id) else {
            return;
        };
        let mode = cycle_mode(&entry.settings.mode, steps);
        entry.raw = if entry.raw.is_object() {
            entry.raw.clone()
        } else {
            json!({})
        };
        entry.raw["mode"] = json!(mode);
        entry.settings = Settings::from_value(&entry.raw);
        entry.raw.clone()
    };
    let _ = instance.set_settings(&raw).await;
    render(Some(vec![id])).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Tap,
    Hold,
}

/// Runs a tap or hold on a key and shows OK / an alert where it makes sense.
async fn gesture(id: String, gesture: Gesture) {
    let Some(instance) = get_instance(id.clone()).await else {
        return;
    };
    let (kind, settings, acknowledged) = {
        let mut state = state();
        let Some(entry) = state.entries.get_mut(&id) else {
            return;
        };
        // While flashing, any press just acknowledges the alert.
        let acknowledged = entry.flash.take().is_some();
        if acknowledged {
            entry.last_image = None;
        }
        (entry.kind, entry.settings.clone(), acknowledged)
    };
    if acknowledged {
        render(Some(vec![id])).await;
        return;
    }
    let result: anyhow::Result<bool> = match (kind, gesture) {
        (Kind::Launch, _) => launch::launch(&settings.terminal_command, &launch_dir(&settings)).map(|_| true),
        (Kind::NeedsYou, Gesture::Tap) => status_press(&settings).await.map(|_| false),
        (Kind::NeedsYou, Gesture::Hold) => status_hold().await.map(|_| false),
        (Kind::Sessions, _) => {
            refresh_sessions().await;
            Ok(false)
        }
        (Kind::Activity, _) => {
            refresh_activity().await;
            Ok(false)
        }
        (Kind::Usage, Gesture::Tap) => {
            cycle(&instance, 1).await;
            Ok(false)
        }
        // Usage hold, and any press on the breakdown / extra usage keys: refresh.
        _ => manual_usage_refresh().await.map_err(|e| anyhow::anyhow!("{e}")),
    };
    match result {
        Ok(true) => {
            let _ = instance.show_ok().await;
        }
        Ok(false) => {}
        Err(error) => {
            log::warn!("{kind:?} key failed: {error}");
            let _ = instance.show_alert().await;
        }
    }
}

// Key down/up only record the press: the SDK handles events one at a time, so
// the work runs in its own task and a hold is timed independently of key up.
async fn on_key_down(instance: &Instance) {
    let id = instance.instance_id.clone();
    let seq = PRESS_SEQ.fetch_add(1, Ordering::SeqCst);
    let immediate = {
        let mut state = state();
        let Some(entry) = state.entries.get_mut(&id) else {
            return;
        };
        // Multi-actions and launch keys act on press; there's nothing to hold for.
        let immediate = entry.in_multi_action || entry.kind == Kind::Launch;
        if !immediate {
            entry.press = Some(Press { seq, held: false });
        }
        immediate
    };
    if immediate {
        tokio::spawn(gesture(id, Gesture::Tap));
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(HOLD_MS)).await;
        let still_down = {
            let mut state = state();
            match state.entries.get_mut(&id).and_then(|e| e.press.as_mut()) {
                Some(press) if press.seq == seq => {
                    press.held = true;
                    true
                }
                _ => false,
            }
        };
        if still_down {
            gesture(id, Gesture::Hold).await;
        }
    });
}

async fn on_key_up(instance: &Instance) {
    let id = instance.instance_id.clone();
    let press = state().entries.get_mut(&id).and_then(|e| e.press.take());
    if let Some(press) = press
        && !press.held
    {
        tokio::spawn(gesture(id, Gesture::Tap));
    }
}

/// Dial turned: usage keys change mode, session-status keys step through sessions.
async fn on_dial_rotate(instance: &Instance, ticks: i16) {
    let id = instance.instance_id.clone();
    let kind = state().entries.get(&id).map(|e| e.kind);
    match kind {
        Some(Kind::Usage) => cycle(instance, i64::from(ticks.signum())).await,
        Some(Kind::NeedsYou) => {
            {
                let mut guard = state();
                let state = &mut *guard;
                let count = state.hooks.len().max(1) as i64;
                if let Some(entry) = state.entries.get_mut(&id) {
                    entry.selected = (entry.selected as i64 + i64::from(ticks)).rem_euclid(count) as usize;
                }
            }
            render(Some(vec![id])).await;
        }
        _ => {}
    }
}

/// Dial pushed (or its strip tapped): refresh usage, or jump to the selected session.
async fn on_dial_press(instance: &Instance, hold: bool) {
    let id = instance.instance_id.clone();
    let kind = state().entries.get(&id).map(|e| e.kind);
    match kind {
        Some(Kind::NeedsYou) if !hold => {
            if let Err(error) = status_dial_press(&id).await {
                log::warn!("session dial press failed: {error}");
                let _ = instance.show_alert().await;
            }
        }
        Some(_) => {
            tokio::spawn(gesture(id, Gesture::Hold));
        }
        None => {}
    }
}

fn own_binary() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_default()
}

fn install_status() -> Value {
    let binary = own_binary();
    json!({
        "hooksInstalled": hooks_install::is_installed(&binary),
        "statuslineInstalled": statusline::is_installed(&binary),
    })
}

/// Tells the property inspector the platform defaults and what's installed.
async fn on_inspector_appear(instance: &Instance) {
    let mut payload = json!({
        "type": "status",
        "terminalCommand": launch::default_command(),
        "focusSupported": cfg!(target_os = "linux"),
    });
    if let (Some(target), Value::Object(extra)) = (payload.as_object_mut(), install_status()) {
        target.extend(extra);
    }
    let _ = instance.send_to_property_inspector(payload).await;
}

async fn on_message(instance: &Instance, payload: &Value) {
    let action = payload.get("type").and_then(Value::as_str).unwrap_or("").to_owned();
    let binary = own_binary();
    let task: Box<dyn FnOnce() -> anyhow::Result<String> + Send> = match action.as_str() {
        "installHooks" => Box::new(move || hooks_install::run(&binary, false)),
        "uninstallHooks" => Box::new(move || hooks_install::run(&binary, true)),
        "installStatusline" => Box::new(move || statusline::install(&binary, false)),
        "uninstallStatusline" => Box::new(move || statusline::install(&binary, true)),
        _ => return,
    };
    let (ok, message) = match tokio::task::spawn_blocking(task).await {
        Ok(Ok(message)) => (true, message),
        Ok(Err(error)) => (false, error.to_string()),
        Err(error) => (false, error.to_string()),
    };
    log::info!("{action}: {message}");
    let mut reply = json!({"type": "installResult", "action": action, "ok": ok, "message": message});
    if let (Some(target), Value::Object(extra)) = (reply.as_object_mut(), install_status()) {
        target.extend(extra);
    }
    let _ = instance.send_to_property_inspector(reply).await;
}

macro_rules! actions {
    ($($name:ident => $kind:expr, $uuid:literal;)*) => {
        $(
            struct $name;

            #[async_trait]
            impl Action for $name {
                const UUID: ActionUuid = $uuid;
                type Settings = Value;

                async fn will_appear(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
                    on_appear(instance, $kind, settings).await;
                    Ok(())
                }

                async fn will_disappear(&self, instance: &Instance, _settings: &Value) -> OpenActionResult<()> {
                    on_disappear(instance);
                    Ok(())
                }

                async fn did_receive_settings(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
                    on_settings(instance, settings).await;
                    Ok(())
                }

                async fn key_down(&self, instance: &Instance, _settings: &Value) -> OpenActionResult<()> {
                    on_key_down(instance).await;
                    Ok(())
                }

                async fn key_up(&self, instance: &Instance, _settings: &Value) -> OpenActionResult<()> {
                    on_key_up(instance).await;
                    Ok(())
                }

                async fn dial_rotate(&self, instance: &Instance, _settings: &Value, ticks: i16, _pressed: bool) -> OpenActionResult<()> {
                    on_dial_rotate(instance, ticks).await;
                    Ok(())
                }

                async fn dial_down(&self, instance: &Instance, _settings: &Value) -> OpenActionResult<()> {
                    on_dial_press(instance, false).await;
                    Ok(())
                }

                async fn touch_tap(&self, instance: &Instance, _settings: &Value, _position: (u16, u16), hold: bool) -> OpenActionResult<()> {
                    on_dial_press(instance, hold).await;
                    Ok(())
                }

                async fn property_inspector_did_appear(&self, instance: &Instance, _settings: &Value) -> OpenActionResult<()> {
                    on_inspector_appear(instance).await;
                    Ok(())
                }

                async fn send_to_plugin(&self, instance: &Instance, _settings: &Value, payload: &Value) -> OpenActionResult<()> {
                    on_message(instance, payload).await;
                    Ok(())
                }
            }
        )*

        async fn register_all() {
            $( register_action($name).await; )*
        }
    };
}

actions! {
    UsageAction => Kind::Usage, "com.mroneleg.claudeusage.usage";
    BreakdownAction => Kind::Breakdown, "com.mroneleg.claudeusage.breakdown";
    ExtraUsageAction => Kind::ExtraUsage, "com.mroneleg.claudeusage.extrausage";
    NeedsYouAction => Kind::NeedsYou, "com.mroneleg.claudeusage.needsyou";
    SessionsAction => Kind::Sessions, "com.mroneleg.claudeusage.sessions";
    ActivityAction => Kind::Activity, "com.mroneleg.claudeusage.activity";
    LaunchAction => Kind::Launch, "com.mroneleg.claudeusage.launch";
}

/// Runs the plugin until OpenDeck closes the connection.
pub async fn run(args: Vec<String>) -> OpenActionResult<()> {
    load_cache();
    register_all().await;
    spawn_loops();
    openaction::run(args).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_lenient() {
        let defaults = Settings::from_value(&Value::Null);
        assert_eq!(defaults.mode, "both");
        assert_eq!(
            (defaults.refresh_minutes, defaults.alert_low, defaults.alert_high),
            (5.0, 80.0, 95.0)
        );
        assert_eq!(defaults.terminal_command, launch::default_command());
        assert_eq!(defaults.data_source, DataSource::Api);
        assert_eq!(defaults.light, LightUp::default());
        assert_eq!(defaults.theme, Theme::default());
        let parsed = Settings::from_value(&json!({
            "mode": "model", "refreshMinutes": "2", "alertLow": 0, "alertHigh": "90",
            "projectDir": "~/x", "terminalCommand": "  ", "includeIdle": true,
            "dataSource": "statusline", "lightWorking": true,
            "background": "#000000", "accent": "<script>"
        }));
        assert_eq!(parsed.mode, "model");
        assert_eq!(
            (parsed.refresh_minutes, parsed.alert_low, parsed.alert_high),
            (2.0, 0.0, 90.0)
        );
        assert_eq!(parsed.terminal_command, launch::default_command());
        assert_eq!(parsed.data_source, DataSource::StatusLine);
        // includeIdle (1.0) means "light up for finished sessions".
        assert_eq!(
            parsed.light,
            LightUp {
                working: true,
                done: true
            }
        );
        assert_eq!(parsed.theme.background, "#000000");
        assert_eq!(parsed.theme.accent, render::CLAUDE);
        // lightDone wins over the old name; unknown modes fall back.
        let explicit = Settings::from_value(&json!({"includeIdle": true, "lightDone": false, "mode": "bogus"}));
        assert!(!explicit.light.done);
        assert_eq!(explicit.mode, "both");
    }

    #[test]
    fn modes_cycle_both_ways() {
        assert_eq!(cycle_mode("both", 1), "session");
        assert_eq!(cycle_mode("model", 1), "both");
        assert_eq!(cycle_mode("both", -1), "model");
        assert_eq!(cycle_mode("weekly", 6), "both");
        assert_eq!(cycle_mode("unknown", 1), "session");
    }

    fn blank_state() -> State {
        State {
            entries: HashMap::new(),
            usage: None,
            last_error: None,
            last_fetch_at: 0,
            failures: 0,
            next_attempt_at: 0,
            bridge: None,
            active: Vec::new(),
            hooks: Vec::new(),
            activity: None,
            activity_fetched_at: 0,
            alerts: AlertState::new(None),
        }
    }

    #[test]
    fn schedule_backs_off_only_for_transient_errors() {
        let mut state = blank_state();
        let interval = refresh_interval_ms(&state);
        schedule_after(&mut state, &Err(UsageError::Http(503)), 0);
        assert_eq!((state.failures, state.next_attempt_at), (1, 2 * interval));
        schedule_after(&mut state, &Err(UsageError::Network("x".into())), 0);
        assert_eq!((state.failures, state.next_attempt_at), (2, 4 * interval));
        schedule_after(&mut state, &Err(UsageError::RateLimited(Some(3_600_000))), 0);
        assert_eq!(state.next_attempt_at, 3_600_000);
        // A login problem resets the backoff: only Claude Code can fix it.
        schedule_after(&mut state, &Err(UsageError::NoLogin), 0);
        assert_eq!((state.failures, state.next_attempt_at), (0, interval));
        schedule_after(&mut state, &Err(UsageError::Http(500)), 0);
        schedule_after(&mut state, &Ok(()), 100);
        assert_eq!((state.failures, state.next_attempt_at), (0, 100 + interval));
    }

    #[test]
    fn badge_marks_failing_and_stale_numbers() {
        let mut state = blank_state();
        let usage = |fetched_at| Usage {
            session: None,
            weekly: None,
            scoped: vec![],
            breakdown: vec![],
            spend: None,
            extra_usage: None,
            fetched_at,
        };
        let now = 100 * 60_000;
        assert_eq!(badge_for(&state, &usage(now - 60_000), DataSource::Api, now), None);
        let stale = badge_for(&state, &usage(now - STALE_MS - 60_000), DataSource::Api, now).unwrap();
        assert_eq!((stale.color, stale.text.as_str()), (render::WARN, "16m ago"));
        // A failed fetch after the shown numbers: red, with the reason.
        state.last_error = Some(UsageError::RateLimited(None));
        state.last_fetch_at = now - 1000;
        let failing = badge_for(&state, &usage(now - 60_000), DataSource::Api, now).unwrap();
        assert_eq!((failing.color, failing.text.as_str()), (render::DANGER, "rate limited"));
        // Newer status line numbers aren't "failing", and status-line-only keys
        // ignore API errors entirely.
        assert_eq!(badge_for(&state, &usage(now), DataSource::Api, now), None);
        assert_eq!(
            badge_for(&state, &usage(now - 60_000), DataSource::StatusLine, now),
            None
        );
    }

    #[test]
    fn shown_usage_prefers_fresher_status_line() {
        let mut state = blank_state();
        assert!(shown_usage(&state, DataSource::Api).is_none());
        let bridge = Bridge {
            five_hour: Some(usage::BridgeWindow {
                percent: 40.0,
                resets_at: None,
            }),
            seven_day: None,
            updated_at: 50,
        };
        state.bridge = Some(bridge);
        // No API data: the status line fills in, for either source.
        assert_eq!(
            shown_usage(&state, DataSource::Api).unwrap().session.unwrap().percent,
            40.0
        );
        state.usage = Some(usage::parse_usage(
            &json!({"five_hour": {"utilization": 10.0}, "seven_day": {"utilization": 3.0}}),
            10,
        ));
        let merged = shown_usage(&state, DataSource::Api).unwrap();
        assert_eq!(
            (merged.session.unwrap().percent, merged.weekly.unwrap().percent),
            (40.0, 3.0)
        );
        let only = shown_usage(&state, DataSource::StatusLine).unwrap();
        assert!(only.weekly.is_none());
    }
}
