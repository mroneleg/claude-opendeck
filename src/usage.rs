//! Plan usage from the endpoint Claude Code's `/usage` command uses,
//! authenticated with Claude Code's own stored OAuth login.
//!
//! The token is only read, never refreshed: refresh tokens rotate, and
//! refreshing behind Claude Code's back could log it out. Claude Code renews
//! the token whenever it runs, so an expired token just means "open Claude Code".

use serde::Serialize;
use serde_json::Value;

use crate::paths;

#[cfg(feature = "http")]
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Test seam: point the plugin at a local stub instead of the real endpoint.
#[cfg(feature = "http")]
const USAGE_URL_ENV: &str = "CLAUDE_OPENDECK_USAGE_URL";
#[cfg(feature = "http")]
const OAUTH_BETA: &str = "oauth-2025-04-20";

const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const SESSION_WINDOW_MS: i64 = 5 * HOUR_MS;
pub const WEEKLY_WINDOW_MS: i64 = 7 * 24 * HOUR_MS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    /// No Claude Code login on this machine.
    NoLogin,
    /// The stored token expired or was rejected; running Claude Code renews it.
    Expired(String),
    /// HTTP 429, with the server's Retry-After (ms) when it sent one.
    RateLimited(Option<i64>),
    Http(u16),
    Network(String),
}

impl UsageError {
    /// Local login problems: retrying sooner or later won't help until Claude
    /// Code runs again, so they never back off.
    pub fn is_login_problem(&self) -> bool {
        matches!(self, Self::NoLogin | Self::Expired(_))
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLogin => write!(f, "No Claude Code login found"),
            Self::Expired(message) => write!(f, "{message}"),
            Self::RateLimited(Some(ms)) => write!(f, "rate limited (retry after {}s)", ms / 1000),
            Self::RateLimited(None) => write!(f, "rate limited"),
            Self::Http(status) => write!(f, "HTTP {status}"),
            Self::Network(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for UsageError {}

/// One usage window. `window_ms` lets callers work out pace.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub label: String,
    pub percent: f64,
    pub resets_at: Option<i64>,
    pub window_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BreakdownRow {
    pub name: String,
    pub percent: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    pub enabled: bool,
    pub used: f64,
    pub limit: Option<f64>,
    pub currency: String,
    pub percent: Option<f64>,
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtraUsage {
    pub used: f64,
    pub limit: Option<f64>,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    /// Per-model weekly limits, highest first.
    pub scoped: Vec<Window>,
    pub breakdown: Vec<BreakdownRow>,
    pub spend: Option<Spend>,
    pub extra_usage: Option<ExtraUsage>,
    pub fetched_at: i64,
}

fn parse_time(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn usage_window(raw: &Value, label: &str, window_ms: i64) -> Option<Window> {
    let percent = raw.get("utilization")?.as_f64()?;
    Some(Window {
        label: label.to_owned(),
        percent,
        resets_at: raw.get("resets_at").and_then(parse_time),
        window_ms,
    })
}

fn scoped_windows(limits: &Value) -> Vec<Window> {
    let mut windows: Vec<Window> = limits
        .as_array()
        .into_iter()
        .flatten()
        .filter(|limit| limit.get("kind").and_then(Value::as_str) == Some("weekly_scoped"))
        .filter_map(|limit| {
            let percent = limit.get("percent")?.as_f64()?;
            let scope = limit.get("scope");
            let name = |key: &str| {
                scope
                    .and_then(|s| s.get(key))
                    .and_then(|s| s.get("display_name"))
                    .and_then(Value::as_str)
            };
            Some(Window {
                label: name("model").or_else(|| name("surface")).unwrap_or("Scoped").to_owned(),
                percent,
                resets_at: limit.get("resets_at").and_then(parse_time),
                window_ms: WEEKLY_WINDOW_MS,
            })
        })
        .collect();
    windows.sort_by(|a, b| b.percent.total_cmp(&a.percent));
    windows
}

fn money(amount: &Value) -> Option<f64> {
    let minor = amount.get("amount_minor")?.as_f64()?;
    let exponent = amount.get("exponent").and_then(Value::as_i64).unwrap_or(2);
    Some(minor / 10f64.powi(exponent as i32))
}

/// Parses the usage endpoint's JSON body.
pub fn parse_usage(body: &Value, now: i64) -> Usage {
    let breakdown = body
        .pointer("/seven_day_breakdown/rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| BreakdownRow {
            name: row
                .get("display_name")
                .or_else(|| row.get("key"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            percent: row.get("percent").and_then(Value::as_f64).unwrap_or(0.0),
        })
        .collect();

    let spend = body.get("spend").filter(|s| s.is_object()).map(|spend| Spend {
        enabled: spend.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        used: spend.get("used").and_then(money).unwrap_or(0.0),
        limit: spend.get("limit").and_then(money),
        currency: spend
            .pointer("/used/currency")
            .and_then(Value::as_str)
            .unwrap_or("USD")
            .to_owned(),
        percent: spend.get("percent").and_then(Value::as_f64),
        disabled_reason: spend
            .get("disabled_reason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    });

    let extra_usage = body
        .get("extra_usage")
        .filter(|e| e.get("is_enabled").and_then(Value::as_bool) == Some(true))
        .map(|extra| ExtraUsage {
            used: extra.get("used_credits").and_then(Value::as_f64).unwrap_or(0.0),
            limit: extra.get("monthly_limit").and_then(Value::as_f64),
            currency: extra
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("USD")
                .to_owned(),
        });

    Usage {
        session: body
            .get("five_hour")
            .and_then(|raw| usage_window(raw, "5h", SESSION_WINDOW_MS)),
        weekly: body
            .get("seven_day")
            .and_then(|raw| usage_window(raw, "7d", WEEKLY_WINDOW_MS)),
        scoped: body.get("limits").map(scoped_windows).unwrap_or_default(),
        breakdown,
        spend,
        extra_usage,
        fetched_at: now,
    }
}

/// Longest wait between attempts while the endpoint keeps failing.
pub const MAX_BACKOFF_MS: i64 = 30 * MINUTE_MS;

/// How long to wait before the next fetch after `failures` consecutive
/// transient failures (429, 5xx, network), starting from the normal refresh
/// interval and doubling up to [`MAX_BACKOFF_MS`]. A server-sent Retry-After
/// wins when it is longer than the computed wait, and is capped at an hour.
pub fn backoff_ms(interval_ms: i64, failures: u32, retry_after_ms: Option<i64>) -> i64 {
    let doubled = interval_ms.saturating_mul(1i64 << failures.min(16));
    let computed = doubled.min(MAX_BACKOFF_MS).max(interval_ms);
    match retry_after_ms {
        Some(retry) => computed.max(retry.min(HOUR_MS)),
        None => computed,
    }
}

/// Parses a Retry-After header: delta-seconds or an HTTP date. Returns ms from `now`.
pub fn parse_retry_after(value: &str, now: i64) -> Option<i64> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<i64>() {
        return Some(seconds.max(0) * 1000);
    }
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|time| (time.timestamp_millis() - now).max(0))
}

/// The last successful response, kept so restarts show numbers without an API call.
pub fn save_cached(body: &Value, fetched_at: i64) {
    let record = serde_json::json!({"fetchedAt": fetched_at, "body": body});
    let _ = paths::write_atomic(&paths::usage_cache_file(), record.to_string().as_bytes());
}

/// Loads the cached response, if any, as parsed usage.
pub fn load_cached() -> Option<Usage> {
    let text = std::fs::read_to_string(paths::usage_cache_file()).ok()?;
    parse_cached(&text)
}

pub fn parse_cached(text: &str) -> Option<Usage> {
    let record: Value = serde_json::from_str(text).ok()?;
    let fetched_at = record.get("fetchedAt")?.as_i64()?;
    Some(parse_usage(record.get("body")?, fetched_at))
}

/// 5h/7d figures reported by Claude Code's status line (see `statusline`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BridgeWindow {
    pub percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bridge {
    pub five_hour: Option<BridgeWindow>,
    pub seven_day: Option<BridgeWindow>,
    pub updated_at: i64,
}

fn bridge_window(window: Option<BridgeWindow>, label: &str, window_ms: i64) -> Option<Window> {
    window.map(|w| Window {
        label: label.to_owned(),
        percent: w.percent,
        resets_at: w.resets_at,
        window_ms,
    })
}

/// Usage from the status line alone ("Status line only"): just 5h and 7d.
pub fn usage_from_bridge(bridge: &Bridge) -> Usage {
    Usage {
        session: bridge_window(bridge.five_hour, "5h", SESSION_WINDOW_MS),
        weekly: bridge_window(bridge.seven_day, "7d", WEEKLY_WINDOW_MS),
        scoped: Vec::new(),
        breakdown: Vec::new(),
        spend: None,
        extra_usage: None,
        fetched_at: bridge.updated_at,
    }
}

/// API usage with the 5h/7d windows replaced by newer status-line figures.
/// Everything else (per-model, breakdown, spend) only the API provides.
pub fn merge_bridge(api: &Usage, bridge: &Bridge) -> Usage {
    if bridge.updated_at <= api.fetched_at {
        return api.clone();
    }
    let mut merged = api.clone();
    if let Some(session) = bridge_window(bridge.five_hour, "5h", SESSION_WINDOW_MS) {
        merged.session = Some(session);
    }
    if let Some(weekly) = bridge_window(bridge.seven_day, "7d", WEEKLY_WINDOW_MS) {
        merged.weekly = Some(weekly);
    }
    merged.fetched_at = bridge.updated_at;
    merged
}

/// Expected usage % if spending evenly across the window, or None if unknown.
pub fn expected_percent(window: &Window, now: i64) -> Option<f64> {
    let resets_at = window.resets_at?;
    if window.window_ms <= 0 {
        return None;
    }
    let elapsed = window.window_ms - (resets_at - now);
    Some((elapsed as f64 / window.window_ms as f64 * 100.0).clamp(0.0, 100.0))
}

/// The `claudeAiOauth` access token from Claude Code's stored credentials JSON.
pub fn token_from_credentials(json: &str, now: i64) -> Result<String, UsageError> {
    let parsed: Value = serde_json::from_str(json).map_err(|_| UsageError::NoLogin)?;
    let oauth = parsed.get("claudeAiOauth").ok_or(UsageError::NoLogin)?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or(UsageError::NoLogin)?;
    if let Some(expires_at) = oauth.get("expiresAt").and_then(Value::as_i64)
        && expires_at <= now
    {
        return Err(UsageError::Expired(
            "Login token expired; run Claude Code to renew it".into(),
        ));
    }
    Ok(token.to_owned())
}

/// Claude Code keeps its OAuth login in the macOS login Keychain.
#[cfg(target_os = "macos")]
fn keychain_credentials() -> Option<String> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok().map(|s| s.trim().to_owned())
}

fn stored_credentials() -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some(json) = keychain_credentials() {
        return Some(json);
    }
    std::fs::read_to_string(paths::claude_dir().join(".credentials.json")).ok()
}

pub fn read_access_token() -> Result<String, UsageError> {
    let json = stored_credentials().ok_or(UsageError::NoLogin)?;
    token_from_credentials(&json, paths::now_ms())
}

/// Fetches and parses current usage. Blocking; run it off the async runtime.
pub fn fetch_usage() -> Result<Usage, UsageError> {
    fetch_usage_body().map(|body| parse_usage(&body, paths::now_ms()))
}

#[cfg(not(feature = "http"))]
pub fn fetch_usage_body() -> Result<Value, UsageError> {
    Err(UsageError::Network("built without the `http` feature".into()))
}

/// Fetches the endpoint's raw JSON body. Blocking.
#[cfg(feature = "http")]
pub fn fetch_usage_body() -> Result<Value, UsageError> {
    let token = read_access_token()?;
    let url = std::env::var(USAGE_URL_ENV).unwrap_or_else(|_| USAGE_URL.to_owned());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .call()
        .map_err(|error| UsageError::Network(error.to_string()))?;
    let status = response.status().as_u16();
    if status == 401 {
        return Err(UsageError::Expired(
            "Login rejected; run Claude Code to renew it".into(),
        ));
    }
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, paths::now_ms()));
        return Err(UsageError::RateLimited(retry_after));
    }
    if !(200..300).contains(&status) {
        return Err(UsageError::Http(status));
    }
    response
        .body_mut()
        .read_json()
        .map_err(|error| UsageError::Network(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Trimmed copy of a real response from the endpoint.
    fn sample() -> Value {
        json!({
            "five_hour": {"utilization": 0.0, "resets_at": "2026-10-18T03:20:00.185773+00:00"},
            "seven_day": {"utilization": 2.0, "resets_at": "2026-10-19T16:00:00.185798+00:00"},
            "seven_day_opus": null,
            "extra_usage": {"is_enabled": true, "monthly_limit": null, "used_credits": 0.0, "currency": "USD"},
            "limits": [
                {"kind": "session", "percent": 0, "resets_at": "2026-10-18T03:20:00.185773+00:00", "scope": null},
                {"kind": "weekly_all", "percent": 2, "resets_at": "2026-10-19T16:00:00.185798+00:00", "scope": null},
                {"kind": "weekly_scoped", "percent": 0, "resets_at": "2026-10-19T16:00:00+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}},
                {"kind": "weekly_scoped", "percent": 12, "resets_at": "2026-10-19T16:00:00+00:00",
                 "scope": {"model": {"display_name": "Opus"}}}
            ],
            "spend": {"used": {"amount_minor": 1250, "currency": "USD", "exponent": 2}, "limit": null,
                      "percent": 0, "enabled": true, "disabled_reason": null},
            "seven_day_breakdown": {"rows": [
                {"key": "claude_code", "display_name": "Claude Code", "percent": 100},
                {"key": "chat", "display_name": "Chats", "percent": 0}
            ]}
        })
    }

    #[test]
    fn parses_windows_and_extras() {
        let usage = parse_usage(&sample(), 7);
        let session = usage.session.unwrap();
        assert_eq!(session.label, "5h");
        assert_eq!(session.percent, 0.0);
        assert_eq!(session.window_ms, SESSION_WINDOW_MS);
        assert_eq!(
            session.resets_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-10-18T03:20:00.185773+00:00")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        assert_eq!(usage.weekly.unwrap().percent, 2.0);
        // weekly_scoped only, highest first.
        assert_eq!(
            usage.scoped.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            ["Opus", "Fable"]
        );
        assert_eq!(
            usage.breakdown[0],
            BreakdownRow {
                name: "Claude Code".into(),
                percent: 100.0
            }
        );
        let spend = usage.spend.unwrap();
        assert!(spend.enabled);
        assert_eq!(spend.used, 12.5);
        assert_eq!(spend.limit, None);
        assert_eq!(usage.extra_usage.unwrap().currency, "USD");
        assert_eq!(usage.fetched_at, 7);
    }

    #[test]
    fn missing_fields_are_none() {
        let usage = parse_usage(&json!({"five_hour": null, "extra_usage": {"is_enabled": false}}), 0);
        assert!(usage.session.is_none() && usage.weekly.is_none() && usage.spend.is_none());
        assert!(usage.extra_usage.is_none());
        assert!(usage.scoped.is_empty() && usage.breakdown.is_empty());
    }

    #[test]
    fn pace_is_elapsed_fraction_of_window() {
        let window = |resets_in: i64| Window {
            label: "7d".into(),
            percent: 10.0,
            resets_at: Some(1_000_000 + resets_in),
            window_ms: WEEKLY_WINDOW_MS,
        };
        assert_eq!(expected_percent(&window(WEEKLY_WINDOW_MS), 1_000_000), Some(0.0));
        assert_eq!(expected_percent(&window(WEEKLY_WINDOW_MS / 2), 1_000_000), Some(50.0));
        assert_eq!(expected_percent(&window(0), 1_000_000), Some(100.0));
        // Clamped if the clock is past the reset or the reset is further out than a window.
        assert_eq!(expected_percent(&window(-5), 1_000_000), Some(100.0));
        assert_eq!(expected_percent(&window(WEEKLY_WINDOW_MS * 2), 1_000_000), Some(0.0));
        let mut unknown = window(0);
        unknown.resets_at = None;
        assert_eq!(expected_percent(&unknown, 0), None);
    }

    #[test]
    fn backoff_doubles_to_cap_and_honours_retry_after() {
        let interval = 5 * MINUTE_MS;
        assert_eq!(backoff_ms(interval, 0, None), interval);
        assert_eq!(backoff_ms(interval, 1, None), 10 * MINUTE_MS);
        assert_eq!(backoff_ms(interval, 2, None), 20 * MINUTE_MS);
        assert_eq!(backoff_ms(interval, 3, None), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(interval, 40, None), MAX_BACKOFF_MS);
        // Retry-After longer than the schedule wins (capped at an hour)...
        assert_eq!(backoff_ms(interval, 1, Some(15 * MINUTE_MS)), 15 * MINUTE_MS);
        assert_eq!(backoff_ms(interval, 1, Some(5 * HOUR_MS)), HOUR_MS);
        // ...but never shortens it.
        assert_eq!(backoff_ms(interval, 2, Some(1000)), 20 * MINUTE_MS);
        // A long refresh interval is never shortened by the cap.
        assert_eq!(backoff_ms(HOUR_MS, 3, None), HOUR_MS);
    }

    #[test]
    fn parses_retry_after_forms() {
        assert_eq!(parse_retry_after("120", 0), Some(120_000));
        assert_eq!(parse_retry_after(" -3 ", 0), Some(0));
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-18T03:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(parse_retry_after("Sun, 18 Oct 2026 03:02:00 GMT", now), Some(120_000));
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn cached_response_round_trips() {
        let text = json!({"fetchedAt": 1234, "body": sample()}).to_string();
        let usage = parse_cached(&text).unwrap();
        assert_eq!(usage.fetched_at, 1234);
        assert_eq!(usage.weekly.unwrap().percent, 2.0);
        assert!(parse_cached("{}").is_none());
        assert!(parse_cached("not json").is_none());
    }

    #[test]
    fn bridge_overrides_only_when_newer() {
        let api = parse_usage(&sample(), 1000);
        let bridge = Bridge {
            five_hour: Some(BridgeWindow {
                percent: 33.0,
                resets_at: Some(9_000),
            }),
            seven_day: None,
            updated_at: 2000,
        };
        let merged = merge_bridge(&api, &bridge);
        assert_eq!(merged.session.as_ref().unwrap().percent, 33.0);
        assert_eq!(merged.session.as_ref().unwrap().resets_at, Some(9_000));
        // Windows the bridge doesn't know keep the API values; extras stay.
        assert_eq!(merged.weekly.as_ref().unwrap().percent, 2.0);
        assert_eq!(merged.scoped.len(), 2);
        assert_eq!(merged.fetched_at, 2000);
        // Older bridge data is ignored.
        let old = Bridge {
            updated_at: 500,
            ..bridge
        };
        assert_eq!(merge_bridge(&api, &old), api);
        // Status line only: just the two windows.
        let only = usage_from_bridge(&bridge);
        assert!(only.weekly.is_none() && only.scoped.is_empty() && only.spend.is_none());
        assert_eq!(only.session.unwrap().window_ms, SESSION_WINDOW_MS);
    }

    #[test]
    fn credentials_token_and_expiry() {
        let creds = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":2000}}"#;
        assert_eq!(token_from_credentials(creds, 1000), Ok("tok".into()));
        assert!(matches!(
            token_from_credentials(creds, 2000),
            Err(UsageError::Expired(_))
        ));
        assert_eq!(token_from_credentials("{}", 0), Err(UsageError::NoLogin));
        assert_eq!(token_from_credentials("not json", 0), Err(UsageError::NoLogin));
        assert_eq!(
            token_from_credentials(r#"{"claudeAiOauth":{"accessToken":"t"}}"#, 99),
            Ok("t".into())
        );
    }
}
