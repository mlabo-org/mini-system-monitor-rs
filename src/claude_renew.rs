//! Delegated renewal of Claude Code's expired sign-in, ported from CodexBar's
//! delegated refresh (https://github.com/steipete/CodexBar, Copyright (c) 2026
//! Peter Steinberger, MIT License): the app never uses the refresh
//! token itself. It starts the `claude` CLI interactively in a pseudo-terminal
//! inside an app-owned probe directory, opens `/status`, and lets Claude Code
//! renew the token and store it in the keychain. Success means the keychain
//! item actually changed, not merely that the CLI ran.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The native installer's link under the home directory; GUI apps do not
/// inherit the shell PATH.
const CLAUDE_CLI: &str = ".local/bin/claude";
const PROBE_DIRECTORY: &str = "Library/Application Support/mini-system-monitor-rs/claude-probe";
const SESSION_ID_FILE: &str = ".session-id";
// The probe is not a user session: no tools, MCP servers, hooks, or Remote
// Control, and no auto-update of the user's installation.
const SETTINGS: &str = r#"{"remoteControlAtStartup":false,"disableAllHooks":true}"#;
const PTY_ROWS: u16 = 50;
const PTY_COLUMNS: u16 = 160;
// Claude's TUI drops keystrokes typed while it is still starting.
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const STATUS_DURATION: Duration = Duration::from_secs(8);
const ENTER_EVERY: Duration = Duration::from_millis(800);
const KEYCHAIN_SETTLE: Duration = Duration::from_secs(2);
const CURSOR_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
const TRUST_YES: &str = "yes,itrustthisfolder";
const TRUST_NO: &str = "no,exit";
const MENU_SETTLE: Duration = Duration::from_millis(500);
/// Screen text (lowercase, whitespace removed) and the keys that answer it.
/// The folder-trust menu is answered separately (`trust_needs_move_down`).
const PROMPT_ANSWERS: [(&str, &str); 5] = [
    ("doyoutrustthefilesinthisfolder?", "y\r"),
    ("readytocodehere?", "\r"),
    ("pressentertocontinue", "\r"),
    ("showclaudecodestatus", "\r"),
    ("showclaudecode", "\r"),
];

/// Runs the probe and reports whether the keychain item read by
/// `read_keychain` changed.
pub fn renew(read_keychain: impl Fn() -> Option<Vec<u8>>) -> bool {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return false;
    };
    let home = PathBuf::from(home);
    let cli = home.join(CLAUDE_CLI);
    if !cli.exists() {
        return false;
    }
    let Some(probe) = prepare_probe_directory(&home) else {
        return false;
    };
    let baseline = read_keychain();
    let session_id = probe_session_id(&probe);
    remove_probe_transcripts(&home, &probe);
    let _ = run_status(&cli, &probe, &session_id);
    remove_probe_transcripts(&home, &probe);

    let deadline = Instant::now() + KEYCHAIN_SETTLE;
    loop {
        let current = read_keychain();
        if current.is_some() && current != baseline {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn prepare_probe_directory(home: &Path) -> Option<PathBuf> {
    let probe = home.join(PROBE_DIRECTORY);
    let claude = probe.join(".claude");
    fs::create_dir_all(&claude).ok()?;
    fs::write(
        claude.join("settings.local.json"),
        "{\n  \"disableDeepLinkRegistration\" : \"disable\"\n}\n",
    )
    .ok()?;
    Some(probe)
}

/// A fixed ID keeps the probe from creating a new session on every run.
fn probe_session_id(probe: &Path) -> String {
    let path = probe.join(SESSION_ID_FILE);
    if let Some(id) = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| uuid::Uuid::parse_str(raw.trim()).ok())
    {
        return id.to_string();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let _ = fs::write(&path, &id);
    id
}

/// Claude treats `--session-id` as creation-only while its transcript
/// exists, and the probe's transcripts are of no use to anyone.
fn remove_probe_transcripts(home: &Path, probe: &Path) {
    let config_root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let directory = config_root
        .join("projects")
        .join(claude_project_directory_name(probe));
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && path.is_file()
        {
            let _ = fs::remove_file(path);
        }
    }
    let _ = fs::remove_dir(&directory);
}

/// Claude's per-project directory name: every character other than an ASCII
/// letter or digit becomes `-`. The probe path is far below the 200-character
/// point where Claude switches to a hashed name.
fn claude_project_directory_name(directory: &Path) -> String {
    directory
        .to_string_lossy()
        .encode_utf16()
        .map(|unit| match unit {
            48..=57 | 65..=90 | 97..=122 => char::from(unit as u8),
            _ => '-',
        })
        .collect()
}

fn run_status(cli: &Path, probe: &Path, session_id: &str) -> std::io::Result<()> {
    let (primary, secondary) = open_pty()?;
    let mut command = Command::new(cli);
    command
        .args([
            "--allowed-tools",
            "",
            "--strict-mcp-config",
            "--settings",
            SETTINGS,
            "--session-id",
            session_id,
        ])
        .current_dir(probe)
        .env("PWD", probe)
        .env("TERM", "xterm-256color")
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::from(secondary.try_clone()?))
        .stdout(Stdio::from(secondary.try_clone()?))
        .stderr(Stdio::from(secondary));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ANTHROPIC_") {
            command.env_remove(key);
        }
    }
    // SAFETY: only async-signal-safe calls between fork and exec. A new session
    // with the PTY as its controlling terminal lets the whole process group be
    // signalled, and hangs it up if this app dies mid-probe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(command);
    let mut terminal = fs::File::from(primary);
    let result = drive_status(&mut terminal, &mut child);
    stop(terminal, &mut child);
    result
}

fn open_pty() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut primary = -1;
    let mut secondary = -1;
    let mut size = libc::winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLUMNS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two new descriptors on success, owned below.
    let opened = unsafe {
        libc::openpty(
            &mut primary,
            &mut secondary,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if opened != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just opened and nothing else owns them.
    let (primary, secondary) = unsafe {
        (
            OwnedFd::from_raw_fd(primary),
            OwnedFd::from_raw_fd(secondary),
        )
    };
    // SAFETY: plain fcntl on a descriptor this function owns.
    unsafe {
        libc::fcntl(primary.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    Ok((primary, secondary))
}

/// Types `/status`, answers first-run prompts and the terminal's cursor
/// queries, and keeps pressing Enter until the status time is up.
fn drive_status(terminal: &mut fs::File, child: &mut Child) -> std::io::Result<()> {
    let started = Instant::now();
    let mut screen = String::new();
    let mut answered = [false; PROMPT_ANSWERS.len()];
    let mut trusted = false;
    let mut ready_at = started + STARTUP_DELAY;
    let mut typed = false;
    let mut last_enter = Instant::now();
    let mut buffer = [0_u8; 8192];
    while Instant::now() < ready_at + STATUS_DURATION {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        match terminal.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                let chunk = &buffer[..read];
                if chunk
                    .windows(CURSOR_QUERY.len())
                    .any(|window| window == CURSOR_QUERY)
                {
                    terminal.write_all(CURSOR_REPLY)?;
                }
                screen.push_str(&normalized_screen_text(chunk));
                if screen.len() > 16_384 {
                    screen.drain(..screen.len() - 8_192);
                }
                if !trusted && let Some(move_down) = trust_needs_move_down(&screen) {
                    // Keys sent while the menu is still drawing are lost or undone.
                    thread::sleep(MENU_SETTLE);
                    if move_down {
                        terminal.write_all(b"\x1b[B")?;
                        thread::sleep(MENU_SETTLE);
                    }
                    terminal.write_all(b"\r")?;
                    trusted = true;
                    ready_at = Instant::now() + STARTUP_DELAY;
                }
                for (index, (needle, keys)) in PROMPT_ANSWERS.iter().enumerate() {
                    if !answered[index] && screen.contains(needle) {
                        terminal.write_all(keys.as_bytes())?;
                        answered[index] = true;
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            // EIO once the CLI has closed its side of the terminal.
            Err(_) => return Ok(()),
        }
        if !typed && Instant::now() >= ready_at {
            terminal.write_all(b"/status\r")?;
            typed = true;
            last_enter = Instant::now();
        } else if typed && last_enter.elapsed() >= ENTER_EVERY {
            terminal.write_all(b"\r")?;
            last_enter = Instant::now();
        }
    }
    Ok(())
}

/// Once the folder-trust menu is on screen, whether the cursor must move down
/// to reach "Yes, I trust this folder". The probe directory is the app's own,
/// and Claude remembers the choice. Current Claude Code lists "No, exit" first
/// with the cursor on it, so a bare Enter, as in CodexBar's prompt table,
/// would quit.
fn trust_needs_move_down(screen: &str) -> Option<bool> {
    let yes = screen.find(TRUST_YES)?;
    Some(screen.find(TRUST_NO).is_some_and(|no| no < yes))
}

/// Lowercase screen text with ANSI escape sequences and whitespace removed,
/// so prompts match however the TUI lays them out.
fn normalized_screen_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            if chars.next_if_eq(&'[').is_some() {
                // CSI: parameters and intermediates up to a final byte @..~.
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            } else if chars.next_if_eq(&']').is_some() {
                // OSC: up to BEL or ESC \.
                while let Some(next) = chars.next() {
                    if next == '\u{7}' || (next == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if !character.is_whitespace() && !character.is_control() {
            normalized.extend(character.to_lowercase());
        }
    }
    normalized
}

/// Closes the status panel and asks the CLI to exit, then terminates its
/// process group if it lingers. The terminal keeps being drained meanwhile:
/// a session leader cannot finish exiting while its terminal output is
/// unread, and closing the terminal releases it for good.
fn stop(mut terminal: fs::File, child: &mut Child) {
    let _ = terminal.write_all(b"\x1b");
    thread::sleep(Duration::from_millis(150));
    let _ = terminal.write_all(b"/exit\r");
    let group = child.id() as libc::pid_t;
    let mut wait_for = |timeout: Duration, child: &mut Child| {
        let deadline = Instant::now() + timeout;
        let mut buffer = [0_u8; 8192];
        while Instant::now() < deadline {
            while matches!(terminal.read(&mut buffer), Ok(read) if read > 0) {}
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    };
    if wait_for(Duration::from_secs(1), child) {
        return;
    }
    // SAFETY: signals only the probe's own process group.
    unsafe { libc::kill(-group, libc::SIGTERM) };
    if wait_for(Duration::from_secs(1), child) {
        return;
    }
    // SAFETY: as above.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    drop(terminal);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_directory_name_matches_claude() {
        assert_eq!(
            claude_project_directory_name(Path::new(
                "/Users/me/Library/Application Support/x-y/claude-probe"
            )),
            "-Users-me-Library-Application-Support-x-y-claude-probe"
        );
    }

    #[test]
    fn trust_menu_moves_past_a_leading_no() {
        assert_eq!(
            trust_needs_move_down("❯no,exityes,itrustthisfolder"),
            Some(true)
        );
        assert_eq!(
            trust_needs_move_down("❯yes,itrustthisfolderno,exit"),
            Some(false)
        );
        assert_eq!(trust_needs_move_down("no,exit"), None);
    }

    #[test]
    fn screen_text_drops_escapes_and_spacing() {
        assert_eq!(
            normalized_screen_text(
                b"\x1b[2J\x1b]0;title\x07 Do you \x1b[1mtrust\x1b[0m the files\r\n in this folder?"
            ),
            "doyoutrustthefilesinthisfolder?"
        );
    }
}
