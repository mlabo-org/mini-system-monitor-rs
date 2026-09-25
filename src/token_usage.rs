//! Read-only, incremental accounting of locally recorded Codex token usage.
//!
//! `token_count.info.total_token_usage` is a cumulative counter; `last_token_usage`
//! describes one response. Cached input and reasoning output are subsets, not
//! additional tokens. A rollout can repeat the same snapshot for a quota update.
//! Legacy forks rewrite copied history's outer timestamps. Their inherited turn
//! IDs must therefore be excluded before applying the rolling time window.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const WINDOW: i64 = 60 * 60;
const ANCHOR_BYTES: u64 = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TokenTotals {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
}

impl TokenTotals {
    fn difference(self, earlier: Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(earlier.input)?,
            cached_input: self.cached_input.checked_sub(earlier.cached_input)?,
            output: self.output.checked_sub(earlier.output)?,
        })
    }

    pub(crate) fn add(&mut self, other: Self) -> bool {
        let sum = (
            self.input.checked_add(other.input),
            self.cached_input.checked_add(other.cached_input),
            self.output.checked_add(other.output),
        );
        self.input = sum.0.unwrap_or(u64::MAX);
        self.cached_input = sum.1.unwrap_or(u64::MAX);
        self.output = sum.2.unwrap_or(u64::MAX);
        sum.0.is_some() && sum.1.is_some() && sum.2.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct TokenUsageState {
    pub totals: Option<TokenTotals>,
    pub partial: bool,
    /// Cache-write input tokens, reported separately only by providers which
    /// bill them as a distinct part of input.
    pub cache_write: Option<u64>,
}

#[derive(Deserialize, Default)]
struct Row {
    timestamp: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    payload: Payload,
}

#[derive(Deserialize, Default)]
struct Payload {
    #[serde(rename = "type", default)]
    kind: String,
    id: Option<String>,
    turn_id: Option<String>,
    forked_from_id: Option<String>,
    info: Option<UsageInfo>,
}

#[derive(Deserialize)]
struct UsageInfo {
    total_token_usage: Usage,
    last_token_usage: Usage,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

impl Usage {
    fn totals(&self) -> Option<TokenTotals> {
        (self.cached_input_tokens <= self.input_tokens).then_some(TokenTotals {
            input: self.input_tokens,
            cached_input: self.cached_input_tokens,
            output: self.output_tokens,
        })
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct EventKey {
    thread: String,
    turn: Option<String>,
    epoch: u64,
    cumulative: TokenTotals,
}

struct Event {
    at: i64,
    usage: TokenTotals,
}

#[derive(Default)]
struct Cursor {
    offset: u64,
    length: u64,
    modified: (i64, i64),
    anchor: Option<u64>,
    thread: Option<String>,
    fork: bool,
    inherited_turns: Option<HashSet<String>>,
    turn: Option<String>,
    previous: Option<TokenTotals>,
    epoch: u64,
    partial_until: i64,
}

type FileId = (u64, u64);

pub struct TokenUsageSampler {
    home: Option<PathBuf>,
    cursors: HashMap<FileId, Cursor>,
    events: HashMap<EventKey, Event>,
}

impl TokenUsageSampler {
    pub fn from_env() -> Self {
        let home = std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
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
        let mut accessible = false;
        for directory in ["sessions", "archived_sessions"] {
            let path = home.join(directory);
            match fs::read_dir(&path) {
                Ok(entries) => {
                    accessible = true;
                    collect_files(entries, &mut files, &mut partial);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => partial = true,
            }
        }
        // Index every date directory: an old rollout can still be receiving new
        // turns, and an old parent is needed to identify a fork's copied history.
        let mut by_thread: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (path, _) in &files {
            if let Some(id) = filename_thread(path) {
                by_thread.entry(id).or_default().push(path.clone());
            }
        }
        let mut readable = 0;
        let mut attempted = 0;
        let mut present = HashSet::new();
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
                partial |= cursor.partial_until > now_secs;
                continue;
            }
            match read_changes(
                cursor,
                &path,
                &metadata,
                &by_thread,
                now_secs,
                &mut self.events,
            ) {
                Ok(()) => readable += 1,
                Err(_) => partial = true,
            }
            partial |= cursor.partial_until > now_secs;
        }
        self.cursors.retain(|id, _| present.contains(id));
        let mut totals = TokenTotals::default();
        for event in self.events.values() {
            if event.at > cutoff && event.at <= now_secs {
                partial |= !totals.add(event.usage);
            }
        }
        let available = accessible && (attempted == 0 || readable > 0 || !self.events.is_empty());
        TokenUsageState {
            totals: available.then_some(totals),
            partial: partial || !available,
            cache_write: None,
        }
    }
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

fn filename_thread(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    for (start, _) in name.char_indices() {
        if let Some(candidate) = name.get(start..start + 36)
            && uuid::Uuid::parse_str(candidate).is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn inherited_turns(paths: Option<&Vec<PathBuf>>) -> Option<HashSet<String>> {
    let paths = paths?;
    let mut turns = HashSet::new();
    for path in paths {
        let mut reader = BufReader::new(File::open(path).ok()?);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).ok()? == 0 {
                break;
            }
            // A concurrently written trailing line is not a complete record.
            if line.last() != Some(&b'\n') {
                break;
            }
            let row: Row = serde_json::from_slice(&line).ok()?;
            if (row.kind == "turn_context"
                || (row.kind == "event_msg" && row.payload.kind == "task_started"))
                && let Some(turn) = row.payload.turn_id
            {
                turns.insert(turn);
            }
        }
    }
    Some(turns)
}

fn anchor(file: &mut File, offset: u64) -> std::io::Result<u64> {
    let start = offset.saturating_sub(ANCHOR_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes)?;
    // Retain a fingerprint only; no conversation bytes survive a sample.
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    Ok(hash)
}

fn read_changes(
    cursor: &mut Cursor,
    path: &Path,
    metadata: &Metadata,
    by_thread: &HashMap<String, Vec<PathBuf>>,
    now: i64,
    events: &mut HashMap<EventKey, Event>,
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    if metadata.len() < cursor.offset
        || (cursor.offset > 0 && Some(anchor(&mut file, cursor.offset)?) != cursor.anchor)
    {
        *cursor = Cursor::default();
    }
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
        let row: Row = match serde_json::from_slice(&line) {
            Ok(row) => row,
            Err(_) => {
                cursor.partial_until = now.saturating_add(WINDOW);
                continue;
            }
        };
        if row.kind == "session_meta" && cursor.thread.is_none() {
            cursor.thread = row.payload.id;
            if let Some(parent) = row.payload.forked_from_id {
                cursor.fork = true;
                cursor.inherited_turns = inherited_turns(by_thread.get(&parent));
            }
        } else if row.kind == "turn_context"
            || (row.kind == "event_msg" && row.payload.kind == "task_started")
        {
            cursor.turn = row.payload.turn_id;
        } else if row.kind == "event_msg" && row.payload.kind == "token_count" {
            record_usage(cursor, row, now, events);
        }
    }
    let mut file = reader.into_inner();
    cursor.anchor = Some(anchor(&mut file, cursor.offset)?);
    cursor.length = metadata.len();
    cursor.modified = (metadata.mtime(), metadata.mtime_nsec());
    Ok(())
}

fn record_usage(cursor: &mut Cursor, row: Row, now: i64, events: &mut HashMap<EventKey, Event>) {
    // A quota-only event has info=null and consumes no additional tokens.
    let Some(info) = row.payload.info else {
        return;
    };
    let Some(total) = info.total_token_usage.totals() else {
        cursor.partial_until = now.saturating_add(WINDOW);
        return;
    };
    let Some(last) = info.last_token_usage.totals() else {
        cursor.partial_until = now.saturating_add(WINDOW);
        return;
    };
    let previous = cursor.previous.replace(total);
    if previous == Some(total) {
        return;
    }
    let difference = previous.and_then(|previous| total.difference(previous));
    if previous.is_some() && difference.is_none() {
        cursor.epoch += 1;
    }
    // History/compaction can reset cumulative counters. Never charge a restored
    // lifetime counter to the current timestamp; only a response-sized increment
    // which agrees with last_token_usage can be attributed to this event.
    let usage = match difference {
        Some(delta) if delta == last => delta,
        _ => last,
    };
    let Some(at) = row.timestamp.as_deref().and_then(timestamp_secs) else {
        cursor.partial_until = now.saturating_add(WINDOW);
        return;
    };
    if at <= now.saturating_sub(WINDOW) || usage == TokenTotals::default() {
        return;
    }
    if cursor.fork {
        match (&cursor.inherited_turns, &cursor.turn) {
            (Some(turns), Some(turn)) if turns.contains(turn) => return,
            (Some(_), Some(_)) => {}
            _ => {
                cursor.partial_until = now.saturating_add(WINDOW);
                return;
            }
        }
    }
    let Some(thread) = &cursor.thread else {
        cursor.partial_until = now.saturating_add(WINDOW);
        return;
    };
    if (previous.is_none() && total != last)
        || difference.is_some_and(|difference| difference != last)
        || at > now
    {
        cursor.partial_until = now.saturating_add(WINDOW);
    }
    let key = EventKey {
        thread: thread.clone(),
        turn: cursor.turn.clone(),
        epoch: cursor.epoch,
        cumulative: total,
    };
    events
        .entry(key)
        .and_modify(|event| event.at = event.at.min(at))
        .or_insert(Event { at, usage });
}

// RFC3339 UTC/offset timestamps, including the millisecond Z form emitted by
// Codex. Integer seconds define the public sampler's time precision.
pub(crate) fn timestamp_secs(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |start, end| text.get(start..end)?.parse::<i64>().ok();
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let mut suffix = &text[19..];
    if let Some(fraction) = suffix.strip_prefix('.') {
        let count = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            return None;
        }
        suffix = &fraction[count..];
    }
    let offset = if suffix == "Z" {
        0
    } else {
        let sign = match suffix.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if suffix.len() != 6 || suffix.as_bytes()[3] != b':' {
            return None;
        }
        let hours = suffix.get(1..3)?.parse::<i64>().ok()?;
        let minutes = suffix.get(4..6)?.parse::<i64>().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        sign * (hours * 3600 + minutes * 60)
    };
    // Gregorian civil date -> days since 1970-01-01 (400-year era arithmetic).
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("token-usage-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("sessions/2020/01/01")).unwrap();
            Self(root)
        }

        fn sampler(&self) -> TokenUsageSampler {
            TokenUsageSampler {
                home: Some(self.0.clone()),
                cursors: HashMap::new(),
                events: HashMap::new(),
            }
        }

        fn path(&self, id: &str) -> PathBuf {
            self.0
                .join(format!("sessions/2020/01/01/rollout-{id}.jsonl"))
        }

        fn write(&self, id: &str, rows: &[serde_json::Value]) {
            let mut file = File::create(self.path(id)).unwrap();
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

    const PARENT: &str = "11111111-1111-4111-8111-111111111111";
    const CHILD: &str = "22222222-2222-4222-8222-222222222222";

    fn meta(id: &str, parent: Option<&str>) -> serde_json::Value {
        json!({"type":"session_meta","payload":{"id":id,"forked_from_id":parent}})
    }

    fn turn(id: &str) -> serde_json::Value {
        json!({"type":"turn_context","payload":{"turn_id":id}})
    }

    fn usage(at: &str, total: [u64; 3], last: [u64; 3]) -> serde_json::Value {
        let values = |v: [u64; 3]| {
            json!({"input_tokens":v[0], "cached_input_tokens":v[1],
            "output_tokens":v[2],"reasoning_output_tokens":v[2],"total_tokens":v[0]+v[2]})
        };
        json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count", "info":{
            "total_token_usage":values(total),"last_token_usage":values(last)}}})
    }

    fn now() -> i64 {
        timestamp_secs("2020-09-19T12:00:00Z").unwrap()
    }

    #[test]
    fn retrospective_window_deduplicates_snapshots_and_expires_while_idle() {
        let fixture = Fixture::new();
        fixture.write(
            PARENT,
            &[
                meta(PARENT, None),
                turn("one"),
                usage("2020-09-19T10:59:59Z", [100, 20, 10], [100, 20, 10]),
                usage("2020-09-19T11:30:00Z", [140, 50, 15], [40, 30, 5]),
                usage("2020-09-19T11:40:00Z", [140, 50, 15], [40, 30, 5]),
            ],
        );
        let mut sampler = fixture.sampler();
        let state = sampler.sample(now());
        assert!(!state.partial);
        assert_eq!(
            state.totals,
            Some(TokenTotals {
                input: 40,
                cached_input: 30,
                output: 5
            })
        );
        assert_eq!(
            sampler.sample(now() + 1800).totals,
            Some(TokenTotals::default())
        );
    }

    #[test]
    fn append_defers_incomplete_line_and_archive_move_does_not_repeat_usage() {
        let fixture = Fixture::new();
        fixture.write(PARENT, &[meta(PARENT, None), turn("one")]);
        let mut sampler = fixture.sampler();
        assert_eq!(sampler.sample(now()).totals, Some(TokenTotals::default()));
        let row = usage("2020-09-19T11:55:00Z", [50, 40, 5], [50, 40, 5]).to_string();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(fixture.path(PARENT))
            .unwrap();
        write!(file, "{}", &row[..row.len() / 2]).unwrap();
        assert_eq!(sampler.sample(now()).totals, Some(TokenTotals::default()));
        writeln!(file, "{}", &row[row.len() / 2..]).unwrap();
        drop(file);
        let expected = Some(TokenTotals {
            input: 50,
            cached_input: 40,
            output: 5,
        });
        assert_eq!(sampler.sample(now()).totals, expected);
        fs::create_dir(fixture.0.join("archived_sessions")).unwrap();
        fs::rename(
            fixture.path(PARENT),
            fixture
                .0
                .join(format!("archived_sessions/rollout-{PARENT}.jsonl")),
        )
        .unwrap();
        assert_eq!(sampler.sample(now()).totals, expected);
    }

    #[test]
    fn forks_skip_retimestamped_history_but_independent_threads_count_separately() {
        let fixture = Fixture::new();
        fixture.write(
            PARENT,
            &[
                meta(PARENT, None),
                turn("parent-turn"),
                usage("2020-09-19T09:00:00Z", [100, 80, 10], [100, 80, 10]),
            ],
        );
        fixture.write(
            CHILD,
            &[
                meta(CHILD, Some(PARENT)),
                meta(PARENT, None),
                turn("parent-turn"),
                usage("2020-09-19T11:30:00Z", [100, 80, 10], [100, 80, 10]),
                turn("child-turn"),
                usage("2020-09-19T11:40:00Z", [150, 100, 15], [50, 20, 5]),
            ],
        );
        let mut sampler = fixture.sampler();
        let state = sampler.sample(now());
        assert!(!state.partial);
        assert_eq!(
            state.totals.unwrap(),
            TokenTotals {
                input: 50,
                cached_input: 20,
                output: 5
            }
        );
        fixture.write(
            PARENT,
            &[
                meta(PARENT, None),
                turn("independent"),
                usage("2020-09-19T11:40:00Z", [50, 20, 5], [50, 20, 5]),
            ],
        );
        assert_eq!(sampler.sample(now()).totals.unwrap().input, 100);
    }

    #[test]
    fn counter_reset_counts_last_response_without_lifetime_or_reasoning_double_add() {
        let fixture = Fixture::new();
        fixture.write(
            PARENT,
            &[
                meta(PARENT, None),
                turn("one"),
                usage("2020-09-19T10:00:00Z", [1000, 500, 100], [1000, 500, 100]),
                usage("2020-09-19T11:30:00Z", [50, 20, 5], [50, 20, 5]),
                usage("2020-09-19T11:40:00Z", [70, 30, 7], [20, 10, 2]),
            ],
        );
        let state = fixture.sampler().sample(now());
        assert!(!state.partial);
        assert_eq!(
            state.totals.unwrap(),
            TokenTotals {
                input: 70,
                cached_input: 30,
                output: 7
            }
        );
        // A partial/resumed log starts with an already-large lifetime total.
        fixture.write(
            PARENT,
            &[
                meta(PARENT, None),
                turn("resumed"),
                usage("2020-09-19T11:40:00Z", [10_000, 5000, 1000], [20, 10, 2]),
            ],
        );
        let state = fixture.sampler().sample(now());
        assert!(state.partial);
        assert_eq!(
            state.totals.unwrap(),
            TokenTotals {
                input: 20,
                cached_input: 10,
                output: 2
            }
        );
    }

    #[test]
    fn replacement_and_truncation_reuse_event_identity() {
        let fixture = Fixture::new();
        let rows = [
            meta(PARENT, None),
            turn("one"),
            usage("2020-09-19T11:30:00Z", [50, 20, 5], [50, 20, 5]),
        ];
        fixture.write(PARENT, &rows);
        let mut sampler = fixture.sampler();
        assert_eq!(sampler.sample(now()).totals.unwrap().input, 50);
        fs::remove_file(fixture.path(PARENT)).unwrap();
        fixture.write(PARENT, &rows);
        assert_eq!(sampler.sample(now()).totals.unwrap().input, 50);
        fixture.write(
            PARENT,
            &[
                meta(PARENT, None),
                turn("two"),
                usage("2020-09-19T11:45:00Z", [30, 10, 3], [30, 10, 3]),
            ],
        );
        assert_eq!(sampler.sample(now()).totals.unwrap().input, 80);
    }

    #[test]
    fn missing_malformed_and_unresolved_fork_are_not_complete_zero() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.sampler().sample(now()).totals,
            Some(TokenTotals::default())
        );
        let mut missing = fixture.sampler();
        missing.home = Some(fixture.0.join("absent"));
        assert!(missing.sample(now()).totals.is_none());
        fixture.write(
            CHILD,
            &[
                meta(CHILD, Some(PARENT)),
                turn("unknown"),
                usage("2020-09-19T11:30:00Z", [10, 5, 1], [10, 5, 1]),
            ],
        );
        let state = fixture.sampler().sample(now());
        assert!(state.partial);
        assert_eq!(state.totals, Some(TokenTotals::default()));
        fs::write(fixture.path(PARENT), b"{invalid}\n").unwrap();
        assert!(fixture.sampler().sample(now()).partial);
    }

    #[test]
    fn timestamps_accept_offsets_and_reject_invalid_dates() {
        assert_eq!(timestamp_secs("1970-01-01T00:00:00.001Z"), Some(0));
        assert_eq!(timestamp_secs("1970-01-01T09:00:00+09:00"), Some(0));
        assert_eq!(timestamp_secs("2026-02-29T00:00:00Z"), None);
        assert_eq!(timestamp_secs("2026-01-01T00:00:00.Z"), None);
    }
}
