use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::{Value, json};

const CODEX_COMMAND: &str = "codex";
const APP_SERVER_ARG: &str = "app-server";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const FIVE_HOURS_MINS: i64 = 300;
const WEEKLY_MINS: i64 = 10_080;

#[derive(Clone, Debug)]
pub struct CodexUsageState {
    pub status: CodexUsageStatus,
    pub content: Option<CodexUsageContent>,
}

impl CodexUsageState {
    pub fn loading() -> Self {
        Self {
            status: CodexUsageStatus::Loading,
            content: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Loading,
    Ready,
    Stale,
    Unavailable,
}

impl CodexUsageStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loading => "LOADING",
            Self::Ready => "READY",
            Self::Stale => "STALE",
            Self::Unavailable => "OFFLINE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexUsageContent {
    pub codex: QuotaBucket,
    pub spark: Option<QuotaBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaBucket {
    pub title: String,
    pub five_hour: QuotaWindow,
    pub weekly: QuotaWindow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaWindow {
    pub label: &'static str,
    pub remaining_percent: Option<u8>,
    pub reset_text: String,
}

impl QuotaWindow {
    pub fn remaining_text(&self) -> String {
        self.remaining_percent
            .map(|percent| format!("{percent}%"))
            .unwrap_or_else(|| "--".to_owned())
    }
}

#[derive(Debug)]
pub struct CodexUsagePoller {
    failures: u32,
    last_good: Option<CodexUsageContent>,
}

impl CodexUsagePoller {
    pub fn new() -> Self {
        Self {
            failures: 0,
            last_good: None,
        }
    }

    pub fn refresh(&mut self) -> CodexUsageState {
        let fetched_at = SystemTime::now();
        let now_secs = unix_seconds(fetched_at);

        match fetch_usage(now_secs) {
            Ok(content) => {
                self.failures = 0;
                self.last_good = Some(content.clone());
                CodexUsageState {
                    status: CodexUsageStatus::Ready,
                    content: Some(content),
                }
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                let status = if error.is_auth_related() {
                    CodexUsageStatus::Unavailable
                } else {
                    CodexUsageStatus::Stale
                };

                if let Some(content) = self.last_good.clone() {
                    CodexUsageState {
                        status,
                        content: Some(content),
                    }
                } else {
                    CodexUsageState {
                        status: CodexUsageStatus::Unavailable,
                        content: None,
                    }
                }
            }
        }
    }

    pub fn next_delay(&self) -> Duration {
        if self.failures == 0 {
            return REFRESH_INTERVAL;
        }

        let backoff_power = self.failures.saturating_sub(1).min(3);
        let multiplier = 1_u64 << backoff_power;
        let seconds = REFRESH_INTERVAL.as_secs().saturating_mul(multiplier);
        Duration::from_secs(seconds.min(MAX_BACKOFF.as_secs()))
    }
}

impl Default for CodexUsagePoller {
    fn default() -> Self {
        Self::new()
    }
}

fn fetch_usage(now_secs: i64) -> Result<CodexUsageContent, FetchError> {
    let mut client = JsonRpcClient::spawn()?;
    client.request(
        "initialize",
        Some(json!({
            "clientInfo": {
                "name": "mini_system_monitor_rs",
                "title": "mini-system-monitor-rs",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        REQUEST_TIMEOUT,
    )?;
    client.notify("initialized")?;

    let result = client.request("account/rateLimits/read", None, REQUEST_TIMEOUT)?;
    parse_rate_limits_response(result, now_secs)
}

struct JsonRpcClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Result<Value, FetchError>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl JsonRpcClient {
    fn spawn() -> Result<Self, FetchError> {
        let mut child = Command::new(CODEX_COMMAND)
            .arg(APP_SERVER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(FetchError::Spawn)?;

        let stdin = child
            .stdin
            .take()
            .ok_or(FetchError::MissingStdio("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(FetchError::MissingStdio("stdout"))?;
        let (tx, rx) = mpsc::channel();

        let reader = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        let parsed = serde_json::from_str::<Value>(&line).map_err(FetchError::Json);
                        if tx.send(parsed).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(FetchError::Io(error)));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            rx,
            reader: Some(reader),
            next_id: 1,
        })
    }

    fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, FetchError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);

        let mut message = json!({
            "id": id,
            "method": method,
        });
        if let Some(params) = params {
            message["params"] = params;
        }

        self.write_message(&message)?;
        self.read_response(id, timeout)
    }

    fn notify(&mut self, method: &str) -> Result<(), FetchError> {
        self.write_message(&json!({ "method": method }))
    }

    fn write_message(&mut self, message: &Value) -> Result<(), FetchError> {
        serde_json::to_writer(&mut self.stdin, message).map_err(FetchError::Json)?;
        self.stdin.write_all(b"\n").map_err(FetchError::Io)?;
        self.stdin.flush().map_err(FetchError::Io)
    }

    fn read_response(&mut self, id: u64, timeout: Duration) -> Result<Value, FetchError> {
        let deadline = Instant::now() + timeout;

        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(FetchError::Timeout)?;
            let message = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| FetchError::Timeout)??;

            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }

            if let Some(error) = message.get("error") {
                let error = serde_json::from_value::<JsonRpcError>(error.clone())
                    .map_err(FetchError::Json)?;
                return Err(FetchError::Rpc {
                    code: error.code,
                    message: error.message,
                });
            }

            return message
                .get("result")
                .cloned()
                .ok_or(FetchError::BadResponse("missing result"));
        }
    }
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug)]
enum FetchError {
    Spawn(io::Error),
    MissingStdio(&'static str),
    Io(io::Error),
    Json(serde_json::Error),
    Rpc { code: i64, message: String },
    Timeout,
    BadResponse(&'static str),
}

impl FetchError {
    fn is_auth_related(&self) -> bool {
        match self {
            Self::Rpc { code, message } => {
                let _ = *code;
                let lower = message.to_ascii_lowercase();
                lower.contains("auth") || lower.contains("login") || lower.contains("token")
            }
            Self::Spawn(error) => {
                let _ = error.kind();
                false
            }
            Self::MissingStdio(stream) | Self::BadResponse(stream) => {
                let _ = stream.len();
                false
            }
            Self::Io(error) => {
                let _ = error.kind();
                false
            }
            Self::Json(error) => {
                let _ = error.classify();
                false
            }
            Self::Timeout => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResponse {
    rate_limits: RateLimitSnapshot,
    rate_limits_by_limit_id: Option<BTreeMap<String, RateLimitSnapshot>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitSnapshot {
    limit_id: Option<String>,
    limit_name: Option<String>,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    used_percent: i32,
    resets_at: Option<i64>,
    window_duration_mins: Option<i64>,
}

fn parse_rate_limits_response(
    result: Value,
    now_secs: i64,
) -> Result<CodexUsageContent, FetchError> {
    let response =
        serde_json::from_value::<RateLimitsResponse>(result).map_err(FetchError::Json)?;

    let (codex_snapshot, spark_snapshot) = match response.rate_limits_by_limit_id {
        Some(buckets) if !buckets.is_empty() => {
            let codex = buckets
                .iter()
                .find(|(limit_id, bucket)| {
                    limit_id.as_str() == "codex" || bucket.limit_id.as_deref() == Some("codex")
                })
                .map(|(_, bucket)| bucket.clone())
                .unwrap_or_else(|| response.rate_limits.clone());
            let spark = buckets
                .values()
                .find(|bucket| {
                    bucket
                        .limit_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("spark"))
                })
                .cloned();
            (codex, spark)
        }
        _ => (response.rate_limits, None),
    };

    Ok(CodexUsageContent {
        codex: quota_bucket("Codex", &codex_snapshot, now_secs),
        spark: spark_snapshot
            .as_ref()
            .map(|snapshot| quota_bucket("Spark", snapshot, now_secs)),
    })
}

fn quota_bucket(title: &str, snapshot: &RateLimitSnapshot, now_secs: i64) -> QuotaBucket {
    QuotaBucket {
        title: title.to_owned(),
        five_hour: quota_window(
            "5h",
            select_window(snapshot, FIVE_HOURS_MINS, WindowFallback::Primary),
            now_secs,
        ),
        weekly: quota_window(
            "週",
            select_window(snapshot, WEEKLY_MINS, WindowFallback::Secondary),
            now_secs,
        ),
    }
}

#[derive(Clone, Copy)]
enum WindowFallback {
    Primary,
    Secondary,
}

fn select_window(
    snapshot: &RateLimitSnapshot,
    duration_mins: i64,
    fallback: WindowFallback,
) -> Option<&RateLimitWindow> {
    [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
        .find(|window| window.window_duration_mins == Some(duration_mins))
        .or(match fallback {
            WindowFallback::Primary => snapshot.primary.as_ref(),
            WindowFallback::Secondary => snapshot.secondary.as_ref(),
        })
}

fn quota_window(
    label: &'static str,
    window: Option<&RateLimitWindow>,
    now_secs: i64,
) -> QuotaWindow {
    QuotaWindow {
        label,
        remaining_percent: window.map(|window| remaining_percent(window.used_percent)),
        reset_text: window
            .map(|window| format_reset_countdown(window.resets_at, now_secs))
            .unwrap_or_else(|| "--".to_owned()),
    }
}

fn remaining_percent(used_percent: i32) -> u8 {
    (100 - used_percent).clamp(0, 100) as u8
}

fn format_reset_countdown(resets_at: Option<i64>, now_secs: i64) -> String {
    match resets_at {
        Some(resets_at) if resets_at <= now_secs => "まもなく".to_owned(),
        Some(resets_at) => {
            let seconds = resets_at - now_secs;
            format!("あと{}", format_seconds(seconds))
        }
        None => "--".to_owned(),
    }
}

fn format_seconds(seconds: i64) -> String {
    if seconds < 60 {
        return "1分未満".to_owned();
    }

    let minutes = seconds / 60;
    let days = minutes / (24 * 60);
    let hours = (minutes % (24 * 60)) / 60;
    let mins = minutes % 60;

    if days > 0 {
        if hours > 0 {
            format!("{days}日{hours}時間")
        } else {
            format!("{days}日")
        }
    } else if hours > 0 {
        if mins > 0 {
            format!("{hours}時間{mins}分")
        } else {
            format!("{hours}時間")
        }
    } else {
        format!("{mins}分")
    }
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_and_spark_buckets_from_limit_id_map() {
        let result = json!({
            "rateLimits": {
                "limitId": "legacy",
                "limitName": "Legacy",
                "primary": { "usedPercent": 90, "windowDurationMins": 300, "resetsAt": 11_000 },
                "secondary": { "usedPercent": 91, "windowDurationMins": 10080, "resetsAt": 20_000 }
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "limitName": "Codex",
                    "primary": { "usedPercent": 25, "windowDurationMins": 300, "resetsAt": 13_600 },
                    "secondary": { "usedPercent": 10, "windowDurationMins": 10080, "resetsAt": 96_400 }
                },
                "codex_bengalfox": {
                    "limitId": "codex_bengalfox",
                    "limitName": "GPT-5.3-Codex-Spark",
                    "primary": { "usedPercent": 40, "windowDurationMins": 300, "resetsAt": 10_030 },
                    "secondary": { "usedPercent": 70, "windowDurationMins": 10080, "resetsAt": 182_800 }
                }
            }
        });

        let parsed = parse_rate_limits_response(result, 10_000).expect("rate limits parse");

        assert_eq!(parsed.codex.five_hour.remaining_percent, Some(75));
        assert_eq!(parsed.codex.weekly.remaining_percent, Some(90));
        assert_eq!(
            parsed.spark.as_ref().map(|bucket| bucket.title.as_str()),
            Some("Spark")
        );
        assert_eq!(
            parsed
                .spark
                .as_ref()
                .map(|bucket| bucket.five_hour.remaining_percent),
            Some(Some(60))
        );
    }

    #[test]
    fn falls_back_to_single_rate_limits_without_inventing_spark() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "limitName": "Codex",
                "primary": { "usedPercent": 12, "windowDurationMins": 300, "resetsAt": 13_600 },
                "secondary": { "usedPercent": 34, "windowDurationMins": 10080, "resetsAt": 96_400 }
            }
        });

        let parsed = parse_rate_limits_response(result, 10_000).expect("rate limits parse");

        assert_eq!(parsed.codex.five_hour.remaining_percent, Some(88));
        assert_eq!(parsed.codex.weekly.remaining_percent, Some(66));
        assert!(parsed.spark.is_none());
    }

    #[test]
    fn formats_remaining_percent_and_reset_countdown() {
        assert_eq!(remaining_percent(0), 100);
        assert_eq!(remaining_percent(37), 63);
        assert_eq!(remaining_percent(150), 0);
        assert_eq!(remaining_percent(-10), 100);

        assert_eq!(format_reset_countdown(Some(10_030), 10_000), "あと1分未満");
        assert_eq!(format_reset_countdown(Some(13_661), 10_000), "あと1時間1分");
        assert_eq!(format_reset_countdown(Some(182_800), 10_000), "あと2日");
        assert_eq!(format_reset_countdown(Some(9_999), 10_000), "まもなく");
        assert_eq!(format_reset_countdown(None, 10_000), "--");
    }
}
