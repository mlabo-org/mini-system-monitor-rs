use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const CODEX_COMMAND: &str = "codex";
const APP_SERVER_ARG: &str = "app-server";
const CHATGPT_APP_BUNDLED_CLI: &str = "/Applications/ChatGPT.app/Contents/Resources/codex";
const CODEX_APP_BUNDLED_CLI: &str = "/Applications/Codex.app/Contents/Resources/codex";
const CODEX_VIABILITY_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_VIABILITY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const FIVE_HOURS_MINS: i64 = 300;
const WEEKLY_MINS: i64 = 10_080;
const AUTO_RESET_JOURNAL_DIRECTORY: &str = "mini-system-monitor-rs";
const AUTO_RESET_JOURNAL_FILE: &str = "codex-auto-reset.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexServiceTier {
    Standard,
    Fast,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexActionKind {
    Refresh,
    ServiceTier,
    AutoReset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexActivity {
    Working(CodexActionKind),
    ServiceTierSaved(CodexServiceTier),
    ResetConsumed,
    ResetAlreadyApplied,
    ResetSkippedNoEligibleWindow,
    ResetSkippedNoCredit,
    Error {
        action: CodexActionKind,
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexControl {
    Refresh,
    SetAutoReset(bool),
    SetServiceTier(CodexServiceTier),
}

#[derive(Clone, Debug)]
pub struct CodexUsageState {
    pub status: CodexUsageStatus,
    pub content: Option<CodexUsageContent>,
    pub activity: Option<CodexActivity>,
    pub error: Option<String>,
}

impl CodexUsageState {
    pub fn loading() -> Self {
        Self {
            status: CodexUsageStatus::Loading,
            content: None,
            activity: None,
            error: None,
        }
    }

    pub fn begin(&mut self, action: CodexActionKind) {
        self.activity = Some(CodexActivity::Working(action));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Loading,
    Ready,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexUsageContent {
    pub codex: QuotaBucket,
    pub spark: Option<QuotaBucket>,
    pub reset_credits: ResetCreditInventory,
    pub service_tier: CodexServiceTier,
    pub fetched_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetCreditInventory {
    pub available_count: Option<u32>,
    pub credits: Vec<ResetCredit>,
    pub details_complete: bool,
}

impl ResetCreditInventory {
    pub fn nearest_expiry(&self) -> Option<i64> {
        self.credits
            .iter()
            .filter_map(|credit| credit.expires_at)
            .min()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetCredit {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub expires_at: Option<i64>,
    pub granted_at: i64,
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
    pub resets_at: Option<i64>,
    pub window_duration_mins: Option<i64>,
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
    auto_reset_enabled: bool,
    auto_reset_guard: AutoResetGuard,
}

impl CodexUsagePoller {
    pub fn new(auto_reset_enabled: bool) -> Self {
        Self {
            failures: 0,
            last_good: None,
            auto_reset_enabled,
            auto_reset_guard: AutoResetGuard::load(),
        }
    }

    pub fn refresh(&mut self) -> CodexUsageState {
        let fetched_at = SystemTime::now();
        let now_secs = unix_seconds(fetched_at);

        match fetch_usage(
            now_secs,
            self.auto_reset_enabled,
            &mut self.auto_reset_guard,
        ) {
            Ok(result) => {
                self.failures = 0;
                self.last_good = Some(result.content.clone());
                CodexUsageState {
                    status: CodexUsageStatus::Ready,
                    content: Some(result.content),
                    activity: result.activity,
                    error: None,
                }
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                let error_summary = error.summary();
                let status = if error.is_auth_related() {
                    CodexUsageStatus::Unavailable
                } else {
                    CodexUsageStatus::Stale
                };

                if let Some(content) = self.last_good.clone() {
                    CodexUsageState {
                        status,
                        content: Some(content),
                        activity: None,
                        error: Some(error_summary),
                    }
                } else {
                    CodexUsageState {
                        status: CodexUsageStatus::Unavailable,
                        content: None,
                        activity: None,
                        error: Some(error_summary),
                    }
                }
            }
        }
    }

    pub fn set_auto_reset_enabled(&mut self, enabled: bool) -> CodexUsageState {
        self.auto_reset_enabled = enabled;
        self.refresh()
    }

    pub fn set_service_tier(&mut self, service_tier: CodexServiceTier) -> CodexUsageState {
        if service_tier == CodexServiceTier::Unknown {
            return self.state_with_activity(CodexActivity::Error {
                action: CodexActionKind::ServiceTier,
                detail: "unsupported service tier".to_owned(),
            });
        }

        if let Err(error) = write_service_tier(service_tier) {
            return self.state_with_activity(CodexActivity::Error {
                action: CodexActionKind::ServiceTier,
                detail: error.summary(),
            });
        }

        let mut state = self.refresh();
        let verified = state
            .content
            .as_ref()
            .is_some_and(|content| content.service_tier == service_tier);
        state.activity = Some(if verified {
            CodexActivity::ServiceTierSaved(service_tier)
        } else {
            CodexActivity::Error {
                action: CodexActionKind::ServiceTier,
                detail: "setting was overridden by another config layer".to_owned(),
            }
        });
        state
    }

    fn state_with_activity(&self, activity: CodexActivity) -> CodexUsageState {
        CodexUsageState {
            status: if self.last_good.is_some() {
                CodexUsageStatus::Ready
            } else {
                CodexUsageStatus::Unavailable
            },
            content: self.last_good.clone(),
            activity: Some(activity),
            error: None,
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
        Self::new(false)
    }
}

struct FetchResult {
    content: CodexUsageContent,
    activity: Option<CodexActivity>,
}

fn fetch_usage(
    now_secs: i64,
    auto_reset_enabled: bool,
    auto_reset_guard: &mut AutoResetGuard,
) -> Result<FetchResult, FetchError> {
    let mut client = initialized_client()?;
    let rate_limits = client.request("account/rateLimits/read", None, REQUEST_TIMEOUT)?;
    let config = client.request(
        "config/read",
        Some(json!({ "includeLayers": false })),
        REQUEST_TIMEOUT,
    )?;
    let service_tier = parse_service_tier(&config);
    let mut content = parse_rate_limits_response(rate_limits, now_secs, service_tier)?;
    let mut activity = None;

    if auto_reset_enabled {
        let reset_result = try_auto_reset(&mut client, &content, now_secs, auto_reset_guard);
        activity = reset_result.activity;

        if reset_result.refetch {
            let refreshed = client.request("account/rateLimits/read", None, REQUEST_TIMEOUT)?;
            content = parse_rate_limits_response(refreshed, now_secs, service_tier)?;
        }
    }

    Ok(FetchResult { content, activity })
}

fn initialized_client() -> Result<JsonRpcClient, FetchError> {
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
    Ok(client)
}

fn write_service_tier(service_tier: CodexServiceTier) -> Result<(), FetchError> {
    let config_value = match service_tier {
        CodexServiceTier::Standard => "default",
        CodexServiceTier::Fast => "fast",
        CodexServiceTier::Unknown => {
            return Err(FetchError::BadResponse("unsupported service tier"));
        }
    };
    let mut client = initialized_client()?;
    client.request(
        "config/batchWrite",
        Some(json!({
            "edits": [
                {
                    "keyPath": "features.fast_mode",
                    "value": true,
                    "mergeStrategy": "upsert"
                },
                {
                    "keyPath": "service_tier",
                    "value": config_value,
                    "mergeStrategy": "replace"
                }
            ],
            "reloadUserConfig": true
        })),
        REQUEST_TIMEOUT,
    )?;
    Ok(())
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
        let codex_executable = resolve_codex_executable();
        let mut child = Command::new(codex_executable)
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

fn resolve_codex_executable() -> PathBuf {
    resolve_codex_executable_from(
        env::var_os("PATH").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

fn resolve_codex_executable_from(path_env: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    resolve_codex_executable_with_fixed_candidates(
        path_env,
        home,
        [
            Path::new(CHATGPT_APP_BUNDLED_CLI),
            Path::new(CODEX_APP_BUNDLED_CLI),
        ],
    )
}

fn resolve_codex_executable_with_fixed_candidates<'a>(
    path_env: Option<&OsStr>,
    home: Option<&OsStr>,
    fixed_candidates: impl IntoIterator<Item = &'a Path>,
) -> PathBuf {
    codex_executable_candidates(path_env, home, fixed_candidates)
        .into_iter()
        .find(|candidate| is_executable_file(candidate) && is_viable_codex_executable(candidate))
        .unwrap_or_else(|| PathBuf::from(CODEX_COMMAND))
}

fn codex_executable_candidates<'a>(
    path_env: Option<&OsStr>,
    home: Option<&OsStr>,
    fixed_candidates: impl IntoIterator<Item = &'a Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(path_env) = path_env {
        for candidate in env::split_paths(path_env)
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.join(CODEX_COMMAND))
        {
            push_unique_path(&mut candidates, candidate);
        }
    }

    if let Some(home) = home
        && !home.is_empty()
    {
        push_unique_path(&mut candidates, Path::new(home).join(".local/bin/codex"));
    }

    for candidate in fixed_candidates {
        push_unique_path(&mut candidates, candidate.to_path_buf());
    }

    candidates
}

fn push_unique_path(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };

    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn is_viable_codex_executable(path: &Path) -> bool {
    is_viable_codex_executable_with_timeout(path, CODEX_VIABILITY_TIMEOUT)
}

fn is_viable_codex_executable_with_timeout(path: &Path, timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(CODEX_VIABILITY_POLL_INTERVAL.min(timeout));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
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

    fn summary(&self) -> String {
        match self {
            Self::Spawn(_) => "Codex CLI could not be started".to_owned(),
            Self::MissingStdio(stream) => format!("Codex app-server {stream} is unavailable"),
            Self::Io(_) => "Codex app-server connection failed".to_owned(),
            Self::Json(_) => "Codex app-server returned invalid data".to_owned(),
            Self::Rpc { code, message } if self.is_auth_related() => {
                let _ = message;
                "Codex sign-in is required".to_owned()
            }
            Self::Rpc { code, message } => {
                let _ = message;
                format!("Codex app-server request failed ({code})")
            }
            Self::Timeout => "Codex app-server request timed out".to_owned(),
            Self::BadResponse(reason) => format!("Codex app-server response: {reason}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResponse {
    rate_limits: RateLimitSnapshot,
    rate_limits_by_limit_id: Option<BTreeMap<String, RateLimitSnapshot>>,
    rate_limit_reset_credits: Option<RateLimitResetCreditsSummary>,
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitResetCreditsSummary {
    available_count: i64,
    credits: Option<Vec<RateLimitResetCredit>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitResetCredit {
    id: String,
    title: Option<String>,
    description: Option<String>,
    expires_at: Option<i64>,
    granted_at: i64,
    reset_type: String,
    status: String,
}

fn parse_rate_limits_response(
    result: Value,
    now_secs: i64,
    service_tier: CodexServiceTier,
) -> Result<CodexUsageContent, FetchError> {
    let response =
        serde_json::from_value::<RateLimitsResponse>(result).map_err(FetchError::Json)?;

    let (codex_snapshot, spark_snapshot) = match response.rate_limits_by_limit_id.as_ref() {
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
        _ => (response.rate_limits.clone(), None),
    };

    Ok(CodexUsageContent {
        codex: quota_bucket("Codex", &codex_snapshot, now_secs),
        spark: spark_snapshot
            .as_ref()
            .map(|snapshot| quota_bucket("Spark", snapshot, now_secs)),
        reset_credits: reset_credit_inventory(response.rate_limit_reset_credits, now_secs),
        service_tier,
        fetched_at: now_secs,
    })
}

fn parse_service_tier(config_result: &Value) -> CodexServiceTier {
    match config_result
        .pointer("/config/service_tier")
        .and_then(Value::as_str)
    {
        Some("fast" | "priority") => CodexServiceTier::Fast,
        Some("default") | None => CodexServiceTier::Standard,
        Some(_) => CodexServiceTier::Unknown,
    }
}

fn reset_credit_inventory(
    summary: Option<RateLimitResetCreditsSummary>,
    now_secs: i64,
) -> ResetCreditInventory {
    let Some(summary) = summary else {
        return ResetCreditInventory {
            available_count: None,
            credits: Vec::new(),
            details_complete: false,
        };
    };

    let available_count = u32::try_from(summary.available_count.max(0)).unwrap_or(u32::MAX);
    let details_known = summary.credits.is_some();
    let mut credits = summary
        .credits
        .unwrap_or_default()
        .into_iter()
        .filter(|credit| {
            credit.status == "available"
                && credit.reset_type == "codexRateLimits"
                && credit
                    .expires_at
                    .is_none_or(|expires_at| expires_at > now_secs)
        })
        .map(|credit| ResetCredit {
            id: credit.id,
            title: credit.title,
            description: credit.description,
            expires_at: credit.expires_at,
            granted_at: credit.granted_at,
        })
        .collect::<Vec<_>>();
    credits.sort_by(|left, right| {
        left.expires_at
            .unwrap_or(i64::MAX)
            .cmp(&right.expires_at.unwrap_or(i64::MAX))
            .then_with(|| left.granted_at.cmp(&right.granted_at))
            .then_with(|| left.id.cmp(&right.id))
    });

    ResetCreditInventory {
        available_count: Some(available_count),
        details_complete: details_known && credits.len() == available_count as usize,
        credits,
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AutoResetJournal {
    handled_weekly_resets_at: Option<i64>,
    pending: Option<PendingAutoReset>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingAutoReset {
    weekly_resets_at: i64,
    credit_id: String,
    idempotency_key: String,
}

#[derive(Debug)]
struct AutoResetGuard {
    path: Option<PathBuf>,
    journal: AutoResetJournal,
    fault: Option<String>,
}

impl AutoResetGuard {
    fn load() -> Self {
        let Some(path) = auto_reset_journal_path() else {
            return Self {
                path: None,
                journal: AutoResetJournal::default(),
                fault: Some("application support directory is unavailable".to_owned()),
            };
        };

        match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<AutoResetJournal>(&contents) {
                Ok(journal) => Self {
                    path: Some(path),
                    journal,
                    fault: None,
                },
                Err(_) => Self {
                    path: Some(path),
                    journal: AutoResetJournal::default(),
                    fault: Some("auto-reset safety journal is invalid".to_owned()),
                },
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self {
                path: Some(path),
                journal: AutoResetJournal::default(),
                fault: None,
            },
            Err(_) => Self {
                path: Some(path),
                journal: AutoResetJournal::default(),
                fault: Some("auto-reset safety journal could not be read".to_owned()),
            },
        }
    }

    fn prepare(&mut self, content: &CodexUsageContent) -> Result<Option<PendingAutoReset>, String> {
        let weekly = &content.codex.weekly;
        if weekly.window_duration_mins != Some(WEEKLY_MINS) || weekly.remaining_percent != Some(0) {
            return Ok(None);
        }
        let Some(weekly_resets_at) = weekly.resets_at else {
            return Ok(None);
        };
        if self.journal.handled_weekly_resets_at == Some(weekly_resets_at) {
            return Ok(None);
        }
        if let Some(fault) = self.fault.as_ref() {
            return Err(fault.clone());
        }
        if let Some(pending) = self
            .journal
            .pending
            .as_ref()
            .filter(|pending| pending.weekly_resets_at == weekly_resets_at)
        {
            return Ok(Some(pending.clone()));
        }
        if content.reset_credits.available_count == Some(0)
            || !content.reset_credits.details_complete
        {
            return Ok(None);
        }
        let Some(credit) = content.reset_credits.credits.first() else {
            return Ok(None);
        };

        let pending = PendingAutoReset {
            weekly_resets_at,
            credit_id: credit.id.clone(),
            idempotency_key: Uuid::new_v4().to_string(),
        };
        self.journal.pending = Some(pending.clone());
        if let Err(error) = self.persist() {
            self.fault = Some(error.clone());
            return Err(error);
        }
        Ok(Some(pending))
    }

    fn complete(&mut self, weekly_resets_at: i64) -> Result<(), String> {
        self.journal.handled_weekly_resets_at = Some(weekly_resets_at);
        self.journal.pending = None;
        if let Err(error) = self.persist() {
            self.fault = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| "application support directory is unavailable".to_owned())?;
        let directory = path
            .parent()
            .ok_or_else(|| "auto-reset journal path has no parent".to_owned())?;
        fs::create_dir_all(directory)
            .map_err(|_| "auto-reset safety directory could not be created".to_owned())?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .map_err(|_| "auto-reset safety directory could not be secured".to_owned())?;
        }

        let temporary =
            directory.join(format!(".{AUTO_RESET_JOURNAL_FILE}.{}.tmp", Uuid::new_v4()));
        let write_result = (|| -> Result<(), String> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|_| "auto-reset safety journal could not be created".to_owned())?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(|_| "auto-reset safety journal could not be secured".to_owned())?;
            }

            serde_json::to_writer(&mut file, &self.journal)
                .map_err(|_| "auto-reset safety journal could not be encoded".to_owned())?;
            file.write_all(b"\n")
                .map_err(|_| "auto-reset safety journal could not be written".to_owned())?;
            file.sync_all()
                .map_err(|_| "auto-reset safety journal could not be synchronized".to_owned())?;
            fs::rename(&temporary, path)
                .map_err(|_| "auto-reset safety journal could not be replaced".to_owned())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

fn auto_reset_journal_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            Path::new(&home)
                .join("Library/Application Support")
                .join(AUTO_RESET_JOURNAL_DIRECTORY)
                .join(AUTO_RESET_JOURNAL_FILE)
        })
}

#[derive(Default)]
struct AutoResetResult {
    activity: Option<CodexActivity>,
    refetch: bool,
}

fn try_auto_reset(
    client: &mut JsonRpcClient,
    content: &CodexUsageContent,
    _now_secs: i64,
    guard: &mut AutoResetGuard,
) -> AutoResetResult {
    let pending = match guard.prepare(content) {
        Ok(Some(pending)) => pending,
        Ok(None) => return AutoResetResult::default(),
        Err(detail) => {
            return AutoResetResult {
                activity: Some(CodexActivity::Error {
                    action: CodexActionKind::AutoReset,
                    detail,
                }),
                refetch: false,
            };
        }
    };

    let response = match client.request(
        "account/rateLimitResetCredit/consume",
        Some(json!({
            "creditId": pending.credit_id,
            "idempotencyKey": pending.idempotency_key
        })),
        REQUEST_TIMEOUT,
    ) {
        Ok(response) => response,
        Err(error) => {
            return AutoResetResult {
                activity: Some(CodexActivity::Error {
                    action: CodexActionKind::AutoReset,
                    detail: error.summary(),
                }),
                refetch: false,
            };
        }
    };

    let activity = match response.get("outcome").and_then(Value::as_str) {
        Some("reset") => CodexActivity::ResetConsumed,
        Some("alreadyRedeemed") => CodexActivity::ResetAlreadyApplied,
        Some("nothingToReset") => CodexActivity::ResetSkippedNoEligibleWindow,
        Some("noCredit") => CodexActivity::ResetSkippedNoCredit,
        _ => {
            return AutoResetResult {
                activity: Some(CodexActivity::Error {
                    action: CodexActionKind::AutoReset,
                    detail: "reset result was not recognized".to_owned(),
                }),
                refetch: false,
            };
        }
    };

    let activity = match guard.complete(pending.weekly_resets_at) {
        Ok(()) => activity,
        Err(detail) => CodexActivity::Error {
            action: CodexActionKind::AutoReset,
            detail,
        },
    };
    AutoResetResult {
        activity: Some(activity),
        refetch: true,
    }
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
        resets_at: window.and_then(|window| window.resets_at),
        window_duration_mins: window.and_then(|window| window.window_duration_mins),
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
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "mini-system-monitor-rs-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn write_script(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("write executable");

        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(path)
                .expect("executable metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("set executable permissions");
        }
    }

    fn write_executable(path: &Path) {
        write_script(path, b"#!/bin/sh\nexit 0\n");
    }

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

        let parsed = parse_rate_limits_response(result, 10_000, CodexServiceTier::Standard)
            .expect("rate limits parse");

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

        let parsed = parse_rate_limits_response(result, 10_000, CodexServiceTier::Standard)
            .expect("rate limits parse");

        assert_eq!(parsed.codex.five_hour.remaining_percent, Some(88));
        assert_eq!(parsed.codex.weekly.remaining_percent, Some(66));
        assert!(parsed.spark.is_none());
    }

    #[test]
    fn parses_reset_credits_in_nearest_expiry_order() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "limitName": "Codex",
                "primary": { "usedPercent": 5, "windowDurationMins": 300, "resetsAt": 13_600 },
                "secondary": { "usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 96_400 }
            },
            "rateLimitResetCredits": {
                "availableCount": 2,
                "credits": [
                    {
                        "id": "later",
                        "title": "Later",
                        "description": null,
                        "expiresAt": 30_000,
                        "grantedAt": 2_000,
                        "resetType": "codexRateLimits",
                        "status": "available"
                    },
                    {
                        "id": "earlier",
                        "title": "Earlier",
                        "description": "Use first",
                        "expiresAt": 20_000,
                        "grantedAt": 1_000,
                        "resetType": "codexRateLimits",
                        "status": "available"
                    }
                ]
            }
        });

        let parsed = parse_rate_limits_response(result, 10_000, CodexServiceTier::Fast)
            .expect("rate limits parse");

        assert_eq!(parsed.service_tier, CodexServiceTier::Fast);
        assert_eq!(parsed.reset_credits.available_count, Some(2));
        assert!(parsed.reset_credits.details_complete);
        assert_eq!(parsed.reset_credits.credits[0].id, "earlier");
        assert_eq!(parsed.reset_credits.nearest_expiry(), Some(20_000));
        assert_eq!(parsed.codex.weekly.window_duration_mins, Some(WEEKLY_MINS));
        assert_eq!(parsed.codex.weekly.resets_at, Some(96_400));
    }

    #[test]
    fn marks_capped_credit_details_incomplete_and_parses_speed_config() {
        let inventory = reset_credit_inventory(
            Some(RateLimitResetCreditsSummary {
                available_count: 2,
                credits: Some(vec![RateLimitResetCredit {
                    id: "only-visible".to_owned(),
                    title: None,
                    description: None,
                    expires_at: Some(20_000),
                    granted_at: 1_000,
                    reset_type: "codexRateLimits".to_owned(),
                    status: "available".to_owned(),
                }]),
            }),
            10_000,
        );

        assert!(!inventory.details_complete);
        assert_eq!(
            parse_service_tier(&json!({ "config": { "service_tier": "fast" } })),
            CodexServiceTier::Fast
        );
        assert_eq!(
            parse_service_tier(&json!({ "config": { "service_tier": "default" } })),
            CodexServiceTier::Standard
        );
        assert_eq!(
            parse_service_tier(&json!({ "config": {} })),
            CodexServiceTier::Standard
        );
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

    #[test]
    fn resolves_codex_from_path_when_available() {
        let dir = unique_temp_dir("path-codex");
        fs::create_dir_all(&dir).expect("create temp bin");
        let codex = dir.join(CODEX_COMMAND);
        write_executable(&codex);

        let path_env = OsString::from(dir.as_os_str());
        let resolved = resolve_codex_executable_from(Some(path_env.as_os_str()), None);

        assert_eq!(resolved, codex);

        fs::remove_dir_all(dir).expect("remove temp dir");
    }

    #[test]
    fn resolves_codex_from_home_local_bin_with_empty_path() {
        let home = unique_temp_dir("home-codex");
        let local_bin = home.join(".local/bin");
        fs::create_dir_all(&local_bin).expect("create home local bin");
        let codex = local_bin.join(CODEX_COMMAND);
        write_executable(&codex);

        let empty_path = OsString::from("");
        let resolved =
            resolve_codex_executable_from(Some(empty_path.as_os_str()), Some(home.as_os_str()));

        assert_eq!(resolved, codex);

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[test]
    fn skips_broken_path_wrapper_for_viable_bundled_candidate() {
        let root = unique_temp_dir("broken-wrapper");
        let path_dir = root.join("path-bin");
        let bundled_dir = root.join("ChatGPT.app/Contents/Resources");
        fs::create_dir_all(&path_dir).expect("create path bin");
        fs::create_dir_all(&bundled_dir).expect("create bundled dir");

        let broken_wrapper = path_dir.join(CODEX_COMMAND);
        write_script(
            &broken_wrapper,
            b"#!/bin/sh\nexec /definitely/missing/codex \"$@\"\n",
        );
        let viable_candidate = bundled_dir.join(CODEX_COMMAND);
        write_executable(&viable_candidate);

        let path_env = OsString::from(path_dir.as_os_str());
        let resolved = resolve_codex_executable_with_fixed_candidates(
            Some(path_env.as_os_str()),
            None,
            [viable_candidate.as_path()],
        );

        assert_eq!(resolved, viable_candidate);

        fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn candidate_order_is_stable_and_duplicate_paths_are_removed() {
        let root = unique_temp_dir("candidate-order");
        let first_bin = root.join("first-bin");
        let home = root.join("home");
        let home_bin = home.join(".local/bin");
        let fixed = root.join("ChatGPT.app/Contents/Resources/codex");
        let path_env = env::join_paths([first_bin.as_path(), home_bin.as_path()])
            .expect("join candidate paths");
        let home_candidate = home_bin.join(CODEX_COMMAND);

        let candidates = codex_executable_candidates(
            Some(path_env.as_os_str()),
            Some(home.as_os_str()),
            [home_candidate.as_path(), fixed.as_path()],
        );

        assert_eq!(
            candidates,
            vec![first_bin.join(CODEX_COMMAND), home_candidate, fixed]
        );
    }

    #[test]
    fn viability_check_times_out_and_terminates_hung_candidate() {
        let root = unique_temp_dir("hung-candidate");
        fs::create_dir_all(&root).expect("create temp dir");
        let candidate = root.join(CODEX_COMMAND);
        write_script(&candidate, b"#!/bin/sh\nwhile :; do :; done\n");

        assert!(!is_viable_codex_executable_with_timeout(
            &candidate,
            Duration::from_millis(50)
        ));

        fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn falls_back_to_command_name_when_no_candidate_exists() {
        let home = unique_temp_dir("missing-codex");
        let minimal_path = OsString::from("/usr/bin:/bin:/usr/sbin:/sbin");

        let resolved = resolve_codex_executable_with_fixed_candidates(
            Some(minimal_path.as_os_str()),
            Some(home.as_os_str()),
            [],
        );

        assert_eq!(resolved, PathBuf::from(CODEX_COMMAND));
    }
}
