# mini-system-monitor-rs

[English](README.md) | [日本語](README.ja.md)

Native macOS system monitor written in Rust with `eframe/egui`. It combines
local CPU and memory metrics with Codex usage, service-tier controls, and RESET
credit management in a compact desktop window.

## Project status

This is an independent, unofficial utility and is not affiliated with or
endorsed by OpenAI. Its Codex integration depends on the local `codex
app-server` interface and may need updates when Codex changes.

## Features

### System monitor

- CPU usage
- Memory usage and used/total GiB
- Best-effort Apple Silicon SoC/PMU temperature with the detected source
- Always-on-top full and compact display modes
- Japanese, English, or system language
- Light, dark, or system theme
- Adjustable UI text size from 10 to 32 pt

The language, theme, text-size, and automatic RESET preferences are persisted.
The display mode can be toggled from the monitor surface and starts in full mode
on each launch.

### Codex usage and controls

- Codex 5-hour and weekly remaining quota
- Spark 5-hour and weekly quota when a Spark limit is returned
- Standard/Fast service-tier display and selector
- RESET credit count, ordered inventory, descriptions, and nearest expiry
- Exact RESET-credit expiry in the macOS system time zone plus relative time
- Optional automatic RESET use when the weekly remaining quota reaches 0%
- A separate, resizable Codex controls window
- Manual refresh and links to Codex usage and OpenAI status

RESET expiry timestamps are absolute values from Codex. The app converts them
with the macOS system time-zone rules, independently of the selected UI
language. A Mac set to Japan therefore shows `JST`; zones with daylight saving
time use the abbreviation and offset that apply to the expiry date. UTC is used
only if local conversion is unavailable.

## UI preview

Quota and RESET values depend on the signed-in Codex account; the screenshots
below show example states.

### Main monitor

<p>
  <img src="docs/images/system-monitor-dark-ja.png" alt="Mini System Monitor in Japanese with the dark theme" width="360">
  <img src="docs/images/system-monitor-light-en.png" alt="Mini System Monitor in English with the light theme" width="360">
</p>

### Codex controls

<img src="docs/images/codex-controls-dark-en.png" alt="Codex controls window in English with the dark theme" width="520">

## Codex connection

The app starts a local `codex app-server` process over stdio and initializes a
JSON-RPC connection. It looks for a viable Codex executable in the local
environment and supported Codex/ChatGPT app locations.

Normal refreshes read:

- `account/rateLimits/read`
- `config/read`

The Codex poller runs separately from the system-metrics sampler, normally every
60 seconds, and backs off after failures while retaining the last good result.
If Spark is absent from `rateLimitsByLimitId`, the UI reports it as not detected
instead of inventing a bucket.

The app does not require an OpenAI API key and does not scrape web pages. It does
require a working, signed-in local Codex installation whose app-server supports
the methods above.

### Operations that change Codex state

Selecting Standard or Fast writes the corresponding Codex user configuration
through `config/batchWrite`, reloads it, and verifies the effective value.

Automatic RESET is **off by default**. When enabled, it consumes a credit only
when all of the following are true:

- the Codex weekly window is the expected 10,080-minute window;
- weekly remaining quota is exactly 0%;
- a complete RESET-credit inventory is available;
- the same weekly reset boundary has not already been handled.

The nearest-expiring eligible credit is selected. An idempotency journal at
`~/Library/Application Support/mini-system-monitor-rs/codex-auto-reset.json`
prevents duplicate consumption across retries or restarts.

## Temperature behavior

On macOS, temperature collection first tries the private IOHID sensor route,
then `sysinfo` components, followed by bounded external-command fallbacks for
`osx-cpu-temp`, `istats`, and `powermetrics` when available. This is a
best-effort Apple Silicon SoC/PMU reading, not a guaranteed individual CPU-core
temperature. If no valid source is available, the UI shows `温度 --` / `Temp --`.

## Requirements

- macOS 14 or later for the packaged app
- The Rust toolchain pinned by `rust-toolchain.toml`
- Xcode Command Line Tools for native linking, bundle validation, and signing
- A working Codex CLI, Codex app, or ChatGPT app for Codex usage features
- An authenticated Codex session for account data

The system monitor continues to work when Codex data is unavailable.

Fetch the locked Rust dependencies once after cloning:

```bash
cargo fetch --locked
```

## Run the prebuilt native app

```bash
Scripts/materialize-app.sh
Scripts/run-app.sh
```

The materializer performs a locked, offline release build, ad-hoc signs and
verifies the bundle, then atomically refreshes the stable local runtime at
`dist/Mini System Monitor.app`. The launcher verifies that the bundle executable
exists and is executable, then opens the native app directly. It does not invoke
Cargo or use a binary under `target/`.

If the bundle or `dist/Mini System Monitor.app/Contents/MacOS/mini-system-monitor-rs`
is missing or not executable, `Scripts/run-app.sh` stops with the exact
materialization action. Run `Scripts/materialize-app.sh` again; do not use
`cargo run` or a `target/` binary as a runtime fallback.

Because materialization builds offline, run `cargo fetch --locked` once first on
a fresh clone.

## Development-only source execution

When explicitly developing the Rust source, this command is available:

```bash
cargo run --locked
```

This is not the normal user runtime, installed route, or runnable handoff.

## Build a bundle at a custom packaging location

```bash
Scripts/build-app.sh --output /path/to/package-output
```

This performs a locked, offline release build and produces an ad-hoc signed and
verified `Mini System Monitor.app` below the requested packaging directory using
the source-owned icon and bundle metadata. The destination app must not already
exist. This command is a construction primitive; use `Scripts/materialize-app.sh`
for the stable local runtime.

To install it, quit any running older copy and use the source-owned installer
described below.

The ad-hoc signature is intended for local use. Distributing a downloadable app
to other users requires an Apple Developer ID signature and notarization.

## Install with Codex or the terminal

This repository includes `AGENTS.md`, so Codex can discover the locked setup,
build, validation, and installation commands directly from the repository.
After fetching dependencies, install to the system Applications folder with:

```bash
Scripts/install-app.sh --destination "/Applications/Mini System Monitor.app"
```

The installer refuses to overwrite an existing app. After quitting the running
app, an explicitly authorized replacement can be installed with:

```bash
Scripts/install-app.sh \
  --destination "/Applications/Mini System Monitor.app" \
  --replace
```

Replacement preserves the previous app beside the destination as a timestamped
`Mini System Monitor.previous-*.app` bundle and reports its exact path. The same
command can be given to Codex after opening the cloned repository. Installation
changes the selected Applications directory and may require macOS permission.

## Validate

```bash
Scripts/check.sh
```

The check script runs formatting, tests, Clippy, and the development build in a
dedicated temporary Cargo target directory. It removes that directory on
success, failure, or interruption, so validation does not leave `target/` in
the repository.

When validating the normal packaged runtime, materialize it and execute the app
or its bundle executable directly from `dist/Mini System Monitor.app`; do not
use Cargo as the runtime wrapper.

## License

Released under the [MIT License](LICENSE).
