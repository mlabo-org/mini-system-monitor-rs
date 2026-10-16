//! Read-only, incremental accounting of locally recorded Claude Code token usage.
//!
//! Claude Code transcripts (`<config>/projects/**/*.jsonl`) carry one `usage`
//! object per assistant API response. A streamed response is written as several
//! rows that repeat the same `message.id` and usage, so events are keyed by
//! message ID. Unlike Codex, `input_tokens` excludes cache reads and cache
//! writes; the three are disjoint and summed into the displayed input.

use crate::token_usage::{TokenTotals, TokenUsageState, timestamp_secs};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const WINDOW: i64 = 60 * 60;

#[derive(Deserialize)]
struct Row {
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    usage: Option<Usage>,
}

#[derive(Clone, Copy, Deserialize, Default, PartialEq, Eq, Debug)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl Usage {
    fn merge(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .max(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .max(other.cache_read_input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
    }
}

struct Event {
    at: i64,
    usage: Usage,
}

#[derive(Default)]
struct Cursor {
    offset: u64,
    length: u64,
    modified: (i64, i64),
    partial_until: i64,
}

type FileId = (u64, u64);

pub struct ClaudeUsageSampler {
    home: Option<PathBuf>,
    cursors: HashMap<FileId, Cursor>,
    events: HashMap<String, Event>,
}

impl ClaudeUsageSampler {
    pub fn from_env() -> Self {
        let home = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")));
        Self {
            home,
            cursors: HashMap::new(),
            events: HashMap::new(),
        }
    }

    pub fn sample(&mut self, now_secs: i64) -> TokenUsageState {
        let Some(home) = &self.home else {
            return TokenUsageState {
                totals: None,
                partial: true,
                cache_write: None,
            };
        };
        let cutoff = now_secs.saturating_sub(WINDOW);
        self.events.retain(|_, event| event.at > cutoff);

        let mut files = Vec::new();
        let mut partial = false;
        let accessible = match fs::read_dir(home.join("projects")) {
            Ok(entries) => {
                collect_files(entries, &mut files, &mut partial);
                true
            }
            Err(_) => false,
        };

        let mut present = HashSet::new();
        let mut attempted = 0;
        let mut readable = 0;
        for (path, metadata) in files {
            let id = (metadata.dev(), metadata.ino());
            present.insert(id);
            if metadata.mtime() <= cutoff && !self.cursors.contains_key(&id) {
                continue;
            }
            attempted += 1;
            let cursor = self.cursors.entry(id).or_default();
            let modified = (metadata.mtime(), metadata.mtime_nsec());
            if cursor.length == metadata.len() && cursor.modified == modified && cursor.offset > 0 {
                readable += 1;
            } else {
                match read_changes(cursor, &path, &metadata, now_secs, &mut self.events) {
                    Ok(()) => readable += 1,
                    Err(_) => partial = true,
                }
            }
            partial |= cursor.partial_until > now_secs;
        }
        self.cursors.retain(|id, _| present.contains(id));

        let available = accessible && (attempted == 0 || readable > 0 || !self.events.is_empty());
        let mut totals = TokenTotals::default();
        let mut cache_write = 0_u64;
        let mut overflow = false;
        for event in self.events.values() {
            if event.at > cutoff && event.at <= now_secs {
                let usage = event.usage;
                let input = usage
                    .input_tokens
                    .checked_add(usage.cache_read_input_tokens)
                    .and_then(|sum| sum.checked_add(usage.cache_creation_input_tokens));
                overflow |= input.is_none();
                overflow |= !totals.add(TokenTotals {
                    input: input.unwrap_or(u64::MAX),
                    cached_input: usage.cache_read_input_tokens,
                    output: usage.output_tokens,
                });
                cache_write = cache_write.saturating_add(usage.cache_creation_input_tokens);
            }
        }
        TokenUsageState {
            totals: available.then_some(totals),
            partial: partial || overflow || !available,
            cache_write: available.then_some(cache_write),
        }
    }

    /// Totals of one session's whole transcript and its subagent transcripts,
    /// with no time window. `found` is false when no transcript has that ID.
    pub fn session(&self, session_id: &str) -> SessionUsage {
        let mut result = SessionUsage::default();
        let Some(home) = &self.home else {
            result.partial = true;
            return result;
        };
        let Ok(projects) = fs::read_dir(home.join("projects")) else {
            result.partial = true;
            return result;
        };
        let mut files = Vec::new();
        for project in projects {
            let Ok(project) = project else {
                result.partial = true;
                continue;
            };
            let main = project.path().join(format!("{session_id}.jsonl"));
            if let Ok(metadata) = fs::metadata(&main) {
                files.push((main, metadata));
            }
            if let Ok(children) = fs::read_dir(project.path().join(session_id)) {
                collect_files(children, &mut files, &mut result.partial);
            }
        }
        result.found = !files.is_empty();
        let mut usages: HashMap<String, Usage> = HashMap::new();
        for (path, _) in files {
            let Ok(file) = File::open(&path) else {
                result.partial = true;
                continue;
            };
            let mut reader = BufReader::new(file);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => {
                        result.partial = true;
                        break;
                    }
                }
                match parse_line(&line) {
                    Line::Skip => {}
                    Line::Malformed => result.partial |= line.last() == Some(&b'\n'),
                    Line::Usage { id, usage, .. } => {
                        usages
                            .entry(id)
                            .and_modify(|known| known.merge(usage))
                            .or_insert(usage);
                    }
                }
            }
        }
        let mut totals = TokenTotals::default();
        for usage in usages.values() {
            let input = usage
                .input_tokens
                .checked_add(usage.cache_read_input_tokens)
                .and_then(|sum| sum.checked_add(usage.cache_creation_input_tokens));
            result.partial |= input.is_none();
            result.partial |= !totals.add(TokenTotals {
                input: input.unwrap_or(u64::MAX),
                cached_input: usage.cache_read_input_tokens,
                output: usage.output_tokens,
            });
            result.cache_write = result
                .cache_write
                .saturating_add(usage.cache_creation_input_tokens);
        }
        result.totals = totals;
        result
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct SessionUsage {
    pub totals: TokenTotals,
    pub cache_write: u64,
    pub partial: bool,
    pub found: bool,
}

/// `--claude-cache-json [--session ID]`: prints the last-hour totals the
/// Claude view shows, and one session's totals when an ID is given, as one
/// JSON object for other local tools. Returns the process exit code.
pub fn print_cache_json(args: &[String], now_secs: i64) -> i32 {
    let session_id = match args {
        [] => None,
        [flag, id] if flag == "--session" && is_session_id(id) => Some(id.as_str()),
        _ => {
            eprintln!("usage: mini-system-monitor-rs --claude-cache-json [--session UUID]");
            return 2;
        }
    };
    let mut sampler = ClaudeUsageSampler::from_env();
    let window = sampler.sample(now_secs);
    let mut output = serde_json::json!({
        "window_minutes": WINDOW / 60,
        "window": window.totals.map(|totals| serde_json::json!({
            "input": totals.input,
            "cache_read": totals.cached_input,
            "cache_write": window.cache_write.unwrap_or(0),
            "output": totals.output,
            "partial": window.partial,
        })),
    });
    if let Some(id) = session_id {
        let session = sampler.session(id);
        output["session"] = serde_json::json!({
            "found": session.found,
            "input": session.totals.input,
            "cache_read": session.totals.cached_input,
            "cache_write": session.cache_write,
            "output": session.totals.output,
            "partial": session.partial,
        });
    }
    println!("{output}");
    0
}

fn is_session_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && text.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn collect_files(entries: fs::ReadDir, files: &mut Vec<(PathBuf, Metadata)>, partial: &mut bool) {
    for entry in entries {
        let Ok(entry) = entry else {
            *partial = true;
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            *partial = true;
            continue;
        };
        if kind.is_dir() {
            match fs::read_dir(entry.path()) {
                Ok(children) => collect_files(children, files, partial),
                Err(_) => *partial = true,
            }
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            match entry.metadata() {
                Ok(metadata) => files.push((entry.path(), metadata)),
                Err(_) => *partial = true,
            }
        }
    }
}

fn read_changes(
    cursor: &mut Cursor,
    path: &Path,
    metadata: &Metadata,
    now: i64,
    events: &mut HashMap<String, Event>,
) -> std::io::Result<()> {
    // Transcripts are append-only; a shrink means the file was replaced, and
    // re-reading it is harmless because events are keyed by message ID.
    if metadata.len() < cursor.offset {
        *cursor = Cursor::default();
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 || line.last() != Some(&b'\n') {
            break;
        }
        cursor.offset += count as u64;
        let (id, at, usage) = match parse_line(&line) {
            Line::Skip => continue,
            Line::Malformed => {
                cursor.partial_until = now.saturating_add(WINDOW);
                continue;
            }
            Line::Usage { id, at, usage } => (id, at, usage),
        };
        let Some(at) = at else {
            cursor.partial_until = now.saturating_add(WINDOW);
            continue;
        };
        if at <= now.saturating_sub(WINDOW) {
            continue;
        }
        events
            .entry(id)
            .and_modify(|event| {
                event.at = event.at.min(at);
                event.usage.merge(usage);
            })
            .or_insert(Event { at, usage });
    }
    cursor.length = metadata.len();
    cursor.modified = (metadata.mtime(), metadata.mtime_nsec());
    Ok(())
}

enum Line {
    Skip,
    Malformed,
    Usage {
        id: String,
        at: Option<i64>,
        usage: Usage,
    },
}

fn parse_line(line: &[u8]) -> Line {
    // Rows without usage (user turns, summaries, tool results) are skipped
    // cheaply; only a malformed line makes the total partial.
    if !contains(line, b"\"usage\"") {
        return Line::Skip;
    }
    let Ok(row) = serde_json::from_slice::<Row>(line) else {
        return Line::Malformed;
    };
    let Some(Message {
        id: Some(id),
        usage: Some(usage),
    }) = row.message
    else {
        return Line::Skip;
    };
    if usage == Usage::default() {
        return Line::Skip;
    }
    Line::Usage {
        id,
        at: row.timestamp.as_deref().and_then(timestamp_secs),
        usage,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("claude-usage-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("projects/-repo/session/subagents")).unwrap();
            Self(root)
        }

        fn sampler(&self) -> ClaudeUsageSampler {
            ClaudeUsageSampler {
                home: Some(self.0.clone()),
                cursors: HashMap::new(),
                events: HashMap::new(),
            }
        }

        fn append(&self, name: &str, rows: &[serde_json::Value]) {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.0.join("projects/-repo").join(name))
                .unwrap();
            for row in rows {
                writeln!(file, "{row}").unwrap();
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn assistant(id: &str, at: &str, usage: [u64; 4]) -> serde_json::Value {
        json!({"type":"assistant","timestamp":at,"message":{"id":id,"usage":{
            "input_tokens":usage[0],"cache_read_input_tokens":usage[1],
            "cache_creation_input_tokens":usage[2],"output_tokens":usage[3]}}})
    }

    fn now() -> i64 {
        timestamp_secs("2020-09-19T12:00:00Z").unwrap()
    }

    #[test]
    fn streamed_rows_count_once_and_old_rows_are_excluded() {
        let fixture = Fixture::new();
        fixture.append(
            "a.jsonl",
            &[
                json!({"type":"user","timestamp":"2020-09-19T11:50:00Z","message":{"role":"user"}}),
                assistant("m1", "2020-09-19T11:50:01Z", [2, 100, 50, 10]),
                assistant("m1", "2020-09-19T11:50:02Z", [2, 100, 50, 10]),
                assistant("m2", "2020-09-19T09:00:00Z", [1, 1000, 0, 5]),
                assistant("m3", "2020-09-19T06:00:00Z", [9, 9, 9, 9]),
            ],
        );
        fixture.append(
            "session/subagents/agent-x.jsonl",
            &[assistant("s1", "2020-09-19T11:55:00Z", [3, 0, 7, 4])],
        );
        let state = fixture.sampler().sample(now());
        assert!(!state.partial);
        assert_eq!(
            state.totals,
            Some(TokenTotals {
                input: 2 + 100 + 50 + 3 + 7,
                cached_input: 100,
                output: 14,
            })
        );
        assert_eq!(state.cache_write, Some(57));
    }

    #[test]
    fn appends_are_read_incrementally_and_expire_while_idle() {
        let fixture = Fixture::new();
        fixture.append(
            "a.jsonl",
            &[assistant("m1", "2020-09-19T11:30:00Z", [1, 0, 0, 1])],
        );
        let mut sampler = fixture.sampler();
        assert_eq!(sampler.sample(now()).totals.unwrap().output, 1);
        fixture.append(
            "a.jsonl",
            &[
                assistant("m1", "2020-09-19T11:30:00Z", [1, 0, 0, 1]),
                assistant("m2", "2020-09-19T11:40:00Z", [1, 0, 0, 2]),
            ],
        );
        assert_eq!(sampler.sample(now()).totals.unwrap().output, 3);
        let later = sampler.sample(now() + 3600);
        assert_eq!(later.totals, Some(TokenTotals::default()));
    }

    #[test]
    fn missing_directory_and_malformed_rows_are_not_complete() {
        let fixture = Fixture::new();
        let mut missing = fixture.sampler();
        missing.home = Some(fixture.0.join("absent"));
        assert!(missing.sample(now()).totals.is_none());
        fs::write(
            fixture.0.join("projects/-repo/b.jsonl"),
            b"{\"usage\": invalid}\n",
        )
        .unwrap();
        let state = fixture.sampler().sample(now());
        assert!(state.partial);
        assert_eq!(state.totals, Some(TokenTotals::default()));
    }

    #[test]
    fn session_counts_its_own_and_subagent_transcripts_without_a_window() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("projects/-repo/abc/subagents")).unwrap();
        fixture.append(
            "abc.jsonl",
            &[
                assistant("m1", "2020-09-19T11:50:01Z", [2, 100, 50, 10]),
                assistant("m1", "2020-09-19T11:50:02Z", [2, 100, 50, 10]),
                assistant("m2", "2020-09-01T09:00:00Z", [1, 1000, 0, 5]),
            ],
        );
        fixture.append(
            "abc/subagents/agent-x.jsonl",
            &[assistant("s1", "2020-09-19T11:55:00Z", [3, 0, 7, 4])],
        );
        fixture.append(
            "def.jsonl",
            &[assistant("o1", "2020-09-19T11:55:00Z", [9, 9, 9, 9])],
        );
        let sampler = fixture.sampler();
        assert_eq!(
            sampler.session("abc"),
            SessionUsage {
                totals: TokenTotals {
                    input: 2 + 100 + 50 + 1 + 1000 + 3 + 7,
                    cached_input: 1100,
                    output: 19,
                },
                cache_write: 57,
                partial: false,
                found: true,
            }
        );
        assert!(!sampler.session("absent").found);
    }
}
