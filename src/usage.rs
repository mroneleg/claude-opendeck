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
#[cfg(feature = "http")]
const OAUTH_BETA: &str = "oauth-2025-04-20";

const HOUR_MS: i64 = 60 * 60 * 1000;
pub const SESSION_WINDOW_MS: i64 = 5 * HOUR_MS;
pub const WEEKLY_WINDOW_MS: i64 = 7 * 24 * HOUR_MS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    /// No Claude Code login on this machine.
    NoLogin,
    /// The stored token expired or was rejected; running Claude Code renews it.
    Expired(String),
    Http(u16),
    Network(String),
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLogin => write!(f, "No Claude Code login found"),
            Self::Expired(message) => write!(f, "{message}"),
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

#[cfg(not(feature = "http"))]
pub fn fetch_usage() -> Result<Usage, UsageError> {
    Err(UsageError::Network("built without the `http` feature".into()))
}

/// Fetches and parses current usage. Blocking; run it off the async runtime.
#[cfg(feature = "http")]
pub fn fetch_usage() -> Result<Usage, UsageError> {
    let token = read_access_token()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(USAGE_URL)
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
    if !(200..300).contains(&status) {
        return Err(UsageError::Http(status));
    }
    let body: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| UsageError::Network(error.to_string()))?;
    Ok(parse_usage(&body, paths::now_ms()))
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
