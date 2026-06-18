# mini-system-monitor-rs

Small macOS system monitor prototype written in Rust with `eframe/egui`.

## Shows

- CPU usage
- Memory usage and used/total GiB
- Best-effort Apple Silicon SoC temperature when available
- Codex quota card with normal Codex and Spark 5h / weekly remaining quota
- Links to Codex usage and OpenAI status

On macOS, temperature is attempted through a private IOHID route first, then `sysinfo`
components and external command fallbacks. If no route is available, the UI shows
`温度 --`.

Codex quota uses the local Codex App Server over stdio. The app spawns
`codex app-server`, initializes the JSON-RPC connection, then reads
`account/rateLimits/read`. It does not use an OpenAI API key, private API, or web
scraping. Quota refresh runs on a separate 60s loop from the CPU/memory sampler.
If Spark is not present in `rateLimitsByLimitId`, the UI shows it as not detected
instead of guessing.

## Run

```bash
cargo run
```

## Validate

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build
```

If the existing `target/` directory causes Cargo fingerprint reads to stall, run
validation with a temporary target directory:

```bash
CARGO_TARGET_DIR=/tmp/mini-system-monitor-rs-target cargo test --offline --locked
```
