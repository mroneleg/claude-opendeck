//! The OpenDeck / Stream Deck plugin: seven actions sharing one usage fetch,
//! one session scan and one transcript scan.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use openaction::{Action, ActionUuid, Instance, OpenActionResult, async_trait, get_instance, register_action};
use serde_json::{Value, json};

use crate::activity::{Activity, Totals};
use crate::alerts::{self, AlertState};
use crate::sessions::{self, ActiveSession, HookSession};
use crate::usage::{self, Usage, UsageError};
use crate::{focus, hooks_install, launch, notify, paths, render};

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
const SESSIONS_POLL: Duration = Duration::from_secs(5);
// Hook status files change via write-then-rename, which updates the
// directory's mtime; checking it this often makes the key feel instant.
const STATUS_DIR_POLL: Duration = Duration::from_millis(250);
const ACTIVITY_REFRESH_MS: i64 = 60 * 1000;
const FLASH_MS: i64 = 6000;
const FLASH_FRAME_MS: i64 = 500;

#[derive(Debug, Clone, PartialEq)]
struct Settings {
    mode: String,
    refresh_minutes: f64,
    alert_low: f64,
    alert_high: f64,
    project_dir: String,
    terminal_command: String,
    include_idle: bool,
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
        Settings {
            mode: text("mode").filter(|m| !m.is_empty()).unwrap_or_else(|| "both".into()),
            refresh_minutes: number(value.get("refreshMinutes")).filter(|n| *n > 0.0).unwrap_or(5.0),
            alert_low: number(value.get("alertLow")).unwrap_or(80.0),
            alert_high: number(value.get("alertHigh")).unwrap_or(95.0),
            project_dir: text("projectDir").unwrap_or_default(),
            terminal_command: text("terminalCommand")
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| launch::default_command().into()),
            include_idle: value.get("includeIdle").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone)]
struct Flash {
    until: i64,
    label: String,
    percent: f64,
}

struct Entry {
    kind: Kind,
    settings: Settings,
    last_image: Option<String>,
    flash: Option<Flash>,
}

struct State {
    entries: HashMap<String, Entry>,
    usage: Option<Usage>,
    last_error: Option<UsageError>,
    last_fetch_at: i64,
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
        active: Vec::new(),
        hooks: Vec::new(),
        activity: None,
        activity_fetched_at: 0,
        alerts: AlertState::new(Some(paths::alerts_file())),
    })
});

static ACTIVITY: LazyLock<Mutex<Activity>> = LazyLock::new(|| Mutex::new(Activity::default()));

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn has_kind(state: &State, test: impl Fn(Kind) -> bool) -> bool {
    state.entries.values().any(|e| test(e.kind))
}

// --- rendering ----------------------------------------------------------------

fn error_image(error: &UsageError) -> String {
    match error {
        UsageError::NoLogin => render::message_image("Claude", "not logged in", render::DANGER),
        UsageError::Expired(_) => render::message_image("Claude", "open Claude Code", render::CLAUDE),
        _ => render::message_image("Claude", "offline", render::DANGER),
    }
}

fn usage_key_image(state: &State, entry: &Entry, now: i64) -> Option<String> {
    let login_problem = matches!(state.last_error, Some(UsageError::NoLogin | UsageError::Expired(_)));
    // Keep showing the last good numbers through a transient failure.
    if let Some(usage) = state.usage.as_ref().filter(|_| !login_problem) {
        return Some(match entry.kind {
            Kind::Breakdown => render::breakdown_image(&usage.breakdown),
            Kind::ExtraUsage => render::spend_image(usage.spend.as_ref()),
            _ => render::usage_image(usage, &entry.settings.mode, now),
        });
    }
    state.last_error.as_ref().map(error_image)
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
    match entry.kind {
        Kind::NeedsYou => {
            let waiting = sessions::attention(&state.hooks, entry.settings.include_idle);
            let working = state.hooks.iter().filter(|s| s.state == "working").count();
            Some(render::needs_you_image(
                waiting.len(),
                waiting.first().map(|s| s.project.as_str()),
                working,
            ))
        }
        Kind::Sessions => Some(render::sessions_image(
            state.active.len(),
            state.active.iter().filter(|s| s.status == "busy").count(),
        )),
        Kind::Activity => state.activity.as_ref().map(render::activity_image),
        Kind::Launch => Some(render::launch_image(&paths::basename(&launch_dir(&entry.settings)))),
        _ => usage_key_image(state, entry, now),
    }
}

/// Re-renders the given instances (all if None), sending only changed images.
async fn render(ids: Option<Vec<String>>) {
    let updates: Vec<(String, String)> = {
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
                updates.push((id, image));
            }
        }
        state.entries = entries;
        updates
    };
    for (id, image) in updates {
        if let Some(instance) = get_instance(id).await {
            let _ = instance.set_image(Some(image), None).await;
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

async fn refresh_usage() {
    if USAGE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let result = tokio::task::spawn_blocking(usage::fetch_usage)
        .await
        .unwrap_or_else(|error| Err(UsageError::Network(error.to_string())));
    let mut notifications = Vec::new();
    {
        let mut guard = state();
        let state = &mut *guard;
        match result {
            Ok(usage) => {
                state.usage = Some(usage);
                state.last_error = None;
                notifications = check_thresholds(state);
            }
            Err(error) => {
                log::warn!("usage fetch failed: {error}");
                state.last_error = Some(error);
            }
        }
        state.last_fetch_at = paths::now_ms();
    }
    USAGE_IN_FLIGHT.store(false, Ordering::SeqCst);
    for (title, body) in notifications {
        notify::send(&title, &body).await;
    }
    render_all().await;
}

/// Flashes usage keys whose shown windows crossed a threshold; returns notifications to send.
fn check_thresholds(state: &mut State) -> Vec<(String, String)> {
    let Some(usage) = state.usage.as_ref() else {
        return Vec::new();
    };
    let now = paths::now_ms();
    let ids: Vec<String> = state
        .entries
        .iter()
        .filter(|(_, e)| e.kind == Kind::Usage)
        .map(|(id, _)| id.clone())
        .collect();
    let shown: Vec<_> = ids
        .iter()
        .map(|id| {
            let settings = &state.entries[id].settings;
            (
                alerts::windows_for(usage, &settings.mode),
                vec![settings.alert_low, settings.alert_high],
            )
        })
        .collect();
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
    let (have_usage, have_activity) = {
        let state = state();
        (state.usage.is_some(), state.activity.is_some())
    };
    if kind.uses_usage() && !have_usage {
        refresh_usage().await;
    } else if kind.uses_sessions() {
        refresh_sessions().await;
    } else if kind == Kind::Activity && !have_activity {
        refresh_activity().await;
    }
}

async fn tick() {
    let (usage_due, activity_due) = {
        let state = state();
        let now = paths::now_ms();
        (
            has_kind(&state, Kind::uses_usage) && now - state.last_fetch_at >= refresh_interval_ms(&state),
            has_kind(&state, |k| k == Kind::Activity) && now - state.activity_fetched_at >= ACTIVITY_REFRESH_MS,
        )
    };
    if usage_due {
        tokio::spawn(refresh_usage());
    }
    if activity_due {
        tokio::spawn(refresh_activity());
    }
    render_all().await;
}

fn status_dir_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(paths::status_dir()).and_then(|m| m.modified()).ok()
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
        let mut last = status_dir_mtime();
        let mut interval = tokio::time::interval(STATUS_DIR_POLL);
        loop {
            interval.tick().await;
            let current = status_dir_mtime();
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
    state().entries.insert(
        id.clone(),
        Entry {
            kind,
            settings: Settings::from_value(settings),
            last_image: None,
            flash: None,
        },
    );
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
        entry.last_image = None;
    }
    render(Some(vec![id])).await;
}

async fn focus_session(settings: &Settings) -> anyhow::Result<()> {
    refresh_sessions().await;
    let target = {
        let state = state();
        sessions::attention(&state.hooks, settings.include_idle)
            .first()
            .map(|s| (*s).clone())
            .or_else(|| state.hooks.first().cloned())
    };
    let target = target
        .filter(|t| !t.pids.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no session to focus (are the Claude Code hooks installed?)"))?;
    focus::focus_window(&target.pids, &target.project).await
}

async fn on_key_down(instance: &Instance) {
    let id = instance.instance_id.clone();
    let (kind, settings, flashing) = {
        let mut state = state();
        let Some(entry) = state.entries.get_mut(&id) else {
            return;
        };
        let flashing = entry.flash.take().is_some();
        if flashing {
            entry.last_image = None;
        }
        (entry.kind, entry.settings.clone(), flashing)
    };
    if flashing {
        // First press just acknowledges the alert.
        render(Some(vec![id])).await;
        return;
    }
    let result: anyhow::Result<bool> = match kind {
        Kind::Launch => launch::launch(&settings.terminal_command, &launch_dir(&settings)).map(|_| true),
        Kind::NeedsYou => focus_session(&settings).await.map(|_| false),
        Kind::Sessions => {
            refresh_sessions().await;
            Ok(false)
        }
        Kind::Activity => {
            refresh_activity().await;
            Ok(false)
        }
        _ => {
            // Manual refresh, still rate-limited so a stuck key can't spam the API.
            let due = paths::now_ms() - state().last_fetch_at >= MANUAL_REFRESH_MS;
            if due {
                refresh_usage().await;
                match &state().last_error {
                    Some(error) => Err(anyhow::anyhow!("{error}")),
                    None => Ok(true),
                }
            } else {
                Ok(false)
            }
        }
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

fn own_binary() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_default()
}

/// Tells the property inspector the platform defaults and whether hooks are installed.
async fn on_inspector_appear(instance: &Instance) {
    let installed = hooks_install::is_installed(&own_binary());
    let _ = instance
        .send_to_property_inspector(json!({
            "type": "status",
            "terminalCommand": launch::default_command(),
            "hooksInstalled": installed,
            "focusSupported": cfg!(target_os = "linux"),
        }))
        .await;
}

async fn on_message(instance: &Instance, payload: &Value) {
    let action = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if action != "installHooks" && action != "uninstallHooks" {
        return;
    }
    let uninstall = action == "uninstallHooks";
    let binary = own_binary();
    let result = tokio::task::spawn_blocking(move || hooks_install::run(&binary, uninstall)).await;
    let (ok, message) = match result {
        Ok(Ok(message)) => (true, message),
        Ok(Err(error)) => (false, error.to_string()),
        Err(error) => (false, error.to_string()),
    };
    log::info!("{action}: {message}");
    let _ = instance
        .send_to_property_inspector(json!({
            "type": "hooksResult",
            "ok": ok,
            "message": message,
            "hooksInstalled": hooks_install::is_installed(&own_binary()),
        }))
        .await;
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
        let parsed = Settings::from_value(&json!({
            "mode": "model", "refreshMinutes": "2", "alertLow": 0, "alertHigh": "90",
            "projectDir": "~/x", "terminalCommand": "  ", "includeIdle": true
        }));
        assert_eq!(parsed.mode, "model");
        assert_eq!(
            (parsed.refresh_minutes, parsed.alert_low, parsed.alert_high),
            (2.0, 0.0, 90.0)
        );
        assert_eq!(parsed.terminal_command, launch::default_command());
        assert!(parsed.include_idle);
    }
}
