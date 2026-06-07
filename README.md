# mini-system-monitor-rs

Small macOS system monitor prototype written in Rust with `eframe/egui`.

## Shows

- CPU usage
- Memory usage and used/total GiB
- Best-effort Apple Silicon SoC temperature when available

On macOS, temperature is attempted through a private IOHID route first, then `sysinfo`
components and external command fallbacks. If no route is available, the UI shows
`温度 --`.

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
