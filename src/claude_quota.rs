//! Claude subscription quota read from the account usage endpoint.
//!
//! Claude Code keeps its OAuth credentials in the macOS login keychain. The
//! poller borrows that access token for each request, keeps it only in memory,
//! and never refreshes or stores it. Claude Code started from the desktop app
//! does not renew that token, so when it has expired the poller hands the
//! renewal to the terminal `claude` CLI (see `claude_renew`).
//! `api/oauth/usage` is undocumented and may change. The token reaches curl
//! through stdin, so it never appears in process arguments.

use crate::claude_renew;
use crate::codex_usage::{CodexUsageStatus, QuotaBucket, QuotaWindow, format_reset_countdown};
use crate::token_usage::timestamp_secs;
use serde::Deserialize;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SECURITY: &str = "/usr/bin/security";
const CURL: &str = "/usr/bin/curl";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
// A token this close to expiry is treated as expired: the endpoint answers an
// expired token with long 429s rather than an auth error.
const EXPIRY_MARGIN_SECS: i64 = 60;
// After a renewal, and the cap for retries after runs that renewed nothing
// (which start at the short cooldown and double), as in CodexBar.
const RENEW_COOLDOWN: Duration = Duration::from_secs(300);
const RENEW_RETRY_COOLDOWN: Duration = Duration::from_secs(20);
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "anthropic-beta: oauth-2025-04-20";
const USER_AGENT: &str = concat!("mini-system-monitor-rs/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT_SECS: &str = "10";
// The endpoint answers 429 to aggressive polling; quota moves slowly anyway.
const REFRESH_INTERVAL: Duration = Duration::from_secs(180);
const MAX_BACKOFF: Duration = Duration::from_secs(900);
// A missing or expired token is detected from the keychain alone, so the
// poller rechecks it often to pick up Claude Code's refresh promptly.
const KEYCHAIN_RECHECK_INTERVAL: Duration = Duration::from_secs(30);
const FIVE_HOURS_MINS: i64 = 300;
const WEEKLY_MINS: i64 = 10_080;

#[derive(Clone, Debug)]
pub struct ClaudeQuotaState {
    pub status: CodexUsageStatus,
    pub content: Option<ClaudeQuota>,
    pub error: Option<ClaudeQuotaError>,
}

impl ClaudeQuotaState {
    pub fn loading() -> Self {
        Self {
            status: CodexUsageStatus::Loading,
            content: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeQuota {
    pub bucket: QuotaBucket,
    pub fetched_at: i64,
}

impl ClaudeQuota {
    /// The bucket with reset countdowns measured from `now_secs`, since the
    /// quota is fetched only every few minutes.
    pub fn bucket_at(&self, now_secs: i64) -> QuotaBucket {
        let recount = |window: &QuotaWindow| QuotaWindow {
            reset_text: format_reset_countdown(window.resets_at, now_secs),
            ..window.clone()
        };
        QuotaBucket {
            title: self.bucket.title.clone(),
            five_hour: recount(&self.bucket.five_hour),
            weekly: recount(&self.bucket.weekly),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeQuotaError {
    /// No Claude Code credentials in the keychain.
    SignedOut,
    /// The stored access token has expired; Claude Code refreshes it on use.
    TokenExpired,
    /// The server refused the token (HTTP 401/403).
    Rejected,
    /// HTTP 429.
    RateLimited,
    Failed(String),
}

impl ClaudeQuotaError {
    fn is_auth_related(&self) -> bool {
        matches!(self, Self::SignedOut | Self::TokenExpired | Self::Rejected)
    }

    /// Found before any request reaches the server.
    fn is_local(&self) -> bool {
        matches!(self, Self::SignedOut | Self::TokenExpired)
    }
}

#[derive(Default)]
pub struct ClaudeQuotaPoller {
    failures: u32,
    awaiting_token: bool,
    last_renew: Option<Instant>,
    renew_cooldown: Duration,
    last_good: Option<ClaudeQuota>,
}

impl ClaudeQuotaPoller {
    pub fn refresh(&mut self) -> ClaudeQuotaState {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or_default();
        match self.fetch_renewing_token(now_secs) {
            Ok(quota) => {
                self.failures = 0;
                self.awaiting_token = false;
                self.last_good = Some(quota.clone());
                ClaudeQuotaState {
                    status: CodexUsageStatus::Ready,
                    content: Some(quota),
                    error: None,
                }
            }
            Err(error) => {
                self.awaiting_token = error.is_local();
                if !self.awaiting_token {
                    self.failures = self.failures.saturating_add(1);
                }
                let status = match (&self.last_good, error.is_auth_related()) {
                    (Some(_), false) => CodexUsageStatus::Stale,
                    _ => CodexUsageStatus::Unavailable,
                };
                ClaudeQuotaState {
                    status,
                    content: self.last_good.clone(),
                    error: Some(error),
                }
            }
        }
    }

    fn fetch_renewing_token(&mut self, now_secs: i64) -> Result<ClaudeQuota, ClaudeQuotaError> {
        let result = fetch_quota(now_secs);
        let renew_due = self
            .last_renew
            .is_none_or(|last| last.elapsed() >= self.renew_cooldown);
        if result != Err(ClaudeQuotaError::TokenExpired) || !renew_due {
            return result;
        }
        self.last_renew = Some(Instant::now());
        if claude_renew::renew(read_keychain_item) {
            self.renew_cooldown = RENEW_COOLDOWN;
            fetch_quota(now_secs)
        } else {
            self.renew_cooldown =
                (self.renew_cooldown * 2).clamp(RENEW_RETRY_COOLDOWN, RENEW_COOLDOWN);
            result
        }
    }

    pub fn next_delay(&self) -> Duration {
        if self.awaiting_token {
            return KEYCHAIN_RECHECK_INTERVAL;
        }
        if self.failures == 0 {
            return REFRESH_INTERVAL;
        }
        let multiplier = 1_u32 << self.failures.saturating_sub(1).min(3);
        (REFRESH_INTERVAL * multiplier).min(MAX_BACKOFF)
    }
}

#[derive(Deserialize)]
struct Keychain {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuth>,
}

#[derive(Deserialize)]
struct OAuth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    /// Unix milliseconds.
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    /// Percent used, 0 to 100.
    utilization: Option<f64>,
    resets_at: Option<String>,
}

fn fetch_quota(now_secs: i64) -> Result<ClaudeQuota, ClaudeQuotaError> {
    let token = read_access_token(now_secs)?;
    let body = request_usage(&token)?;
    let usage: UsageResponse = serde_json::from_str(&body)
        .map_err(|_| ClaudeQuotaError::Failed("unexpected response".to_owned()))?;
    Ok(quota_from_usage(usage, now_secs))
}

/// Claude Code's credentials item, or `None` when it cannot be read.
fn read_keychain_item() -> Option<Vec<u8>> {
    let output = Command::new(SECURITY)
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn read_access_token(now_secs: i64) -> Result<String, ClaudeQuotaError> {
    let item = read_keychain_item().ok_or(ClaudeQuotaError::SignedOut)?;
    let keychain: Keychain =
        serde_json::from_slice(&item).map_err(|_| ClaudeQuotaError::SignedOut)?;
    let oauth = keychain.oauth.ok_or(ClaudeQuotaError::SignedOut)?;
    if oauth.expires_at.is_some_and(|expires_at| {
        expires_at
            <= now_secs
                .saturating_add(EXPIRY_MARGIN_SECS)
                .saturating_mul(1000)
    }) {
        return Err(ClaudeQuotaError::TokenExpired);
    }
    oauth
        .access_token
        .filter(|token| !token.is_empty())
        .ok_or(ClaudeQuotaError::SignedOut)
}

fn request_usage(token: &str) -> Result<String, ClaudeQuotaError> {
    let mut child = Command::new(CURL)
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            REQUEST_TIMEOUT_SECS,
            "--header",
            "@-",
            "--header",
            OAUTH_BETA_HEADER,
            "--header",
            "Accept: application/json",
            "--user-agent",
            USER_AGENT,
            "--write-out",
            "\n%{http_code}",
            USAGE_URL,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ClaudeQuotaError::Failed("curl unavailable".to_owned()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A write error surfaces below as a curl failure.
        let _ = writeln!(stdin, "Authorization: Bearer {token}");
    }
    let output = child
        .wait_with_output()
        .map_err(|_| ClaudeQuotaError::Failed("curl failed".to_owned()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .next()
            .unwrap_or("request failed")
            .trim_start_matches("curl: ")
            .to_owned();
        return Err(ClaudeQuotaError::Failed(reason));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, code) = stdout
        .rsplit_once('\n')
        .ok_or_else(|| ClaudeQuotaError::Failed("unexpected response".to_owned()))?;
    match code.trim() {
        "200" => Ok(body.to_owned()),
        "401" | "403" => Err(ClaudeQuotaError::Rejected),
        "429" => Err(ClaudeQuotaError::RateLimited),
        code => Err(ClaudeQuotaError::Failed(format!("HTTP {code}"))),
    }
}

fn quota_from_usage(usage: UsageResponse, now_secs: i64) -> ClaudeQuota {
    let window = |label, source: Option<UsageWindow>, duration_mins| {
        let resets_at = source
            .as_ref()
            .and_then(|window| window.resets_at.as_deref())
            .and_then(timestamp_secs);
        QuotaWindow {
            label,
            remaining_percent: source
                .and_then(|window| window.utilization)
                .map(|used| (100.0 - used).round().clamp(0.0, 100.0) as u8),
            reset_text: format_reset_countdown(resets_at, now_secs),
            resets_at,
            window_duration_mins: Some(duration_mins),
        }
    };
    ClaudeQuota {
        bucket: QuotaBucket {
            title: "Claude".to_owned(),
            five_hour: window("5h", usage.five_hour, FIVE_HOURS_MINS),
            weekly: window("週", usage.seven_day, WEEKLY_MINS),
        },
        fetched_at: now_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_response_maps_to_remaining_quota_windows() {
        let now = timestamp_secs("2026-09-28T10:00:00Z").unwrap();
        let usage: UsageResponse = serde_json::from_str(
            r#"{"five_hour":{"utilization":3.0,"resets_at":"2026-09-28T14:30:00.240567+00:00"},
                "seven_day":{"utilization":100.4,"resets_at":"2026-09-29T22:00:00+00:00"},
                "seven_day_opus":null,"extra_usage":{"is_enabled":false}}"#,
        )
        .unwrap();
        let quota = quota_from_usage(usage, now);
        assert_eq!(quota.bucket.five_hour.remaining_percent, Some(97));
        assert_eq!(quota.bucket.five_hour.reset_text, "あと4時間30分");
        assert_eq!(quota.bucket.weekly.remaining_percent, Some(0));
        assert_eq!(
            quota.bucket_at(now + 3600).five_hour.reset_text,
            "あと3時間30分"
        );
    }

    #[test]
    fn missing_windows_stay_unknown() {
        let usage: UsageResponse =
            serde_json::from_str(r#"{"five_hour":null,"seven_day":{"utilization":null}}"#).unwrap();
        let quota = quota_from_usage(usage, 0);
        assert_eq!(quota.bucket.five_hour.remaining_percent, None);
        assert_eq!(quota.bucket.weekly.remaining_percent, None);
        assert_eq!(quota.bucket.weekly.reset_text, "--");
    }

    #[test]
    fn failures_back_off_up_to_the_cap() {
        let mut poller = ClaudeQuotaPoller::default();
        assert_eq!(poller.next_delay(), REFRESH_INTERVAL);
        poller.failures = 2;
        assert_eq!(poller.next_delay(), REFRESH_INTERVAL * 2);
        poller.failures = 10;
        assert_eq!(poller.next_delay(), MAX_BACKOFF);
        poller.awaiting_token = true;
        assert_eq!(poller.next_delay(), KEYCHAIN_RECHECK_INTERVAL);
    }
}
