# mini-system-monitor-rs Local Constitution

このファイルは `/Users/suzukimakoto/Desktop/git/current-work/mini-system-monitor-rs` 配下における Codex の局所 `AGENTS.md` であり、本スコープ内の実行条件を定義する SSOT である。
本書は助言集ではなく、既定動作、禁止事項、優先順位、ツール選択、検証条件を拘束する運用契約として扱う。
上位の `AGENTS.md`、システム指示、開発者指示、ユーザーの明示要求と競合する場合は、Codex の優先順位規則に従う。
より深い階層に別の `AGENTS.md` が存在する場合、そのスコープではより局所のファイルを優先する。

## Scope

- この repo は `/Users/suzukimakoto/Desktop/git/current-work/AGENTS.md` の current-work 管理下にある Rust/macOS GUI アプリである。
- 配置、分類、正本境界、昇格後の Git 管理は `/Users/suzukimakoto/Desktop/git/AGENTS.md` と `/Users/suzukimakoto/Desktop/git/current-work/AGENTS.md` を上位方針として扱う。
- この repo は `mini-system-monitor-rs` のソース正本であり、移動元の旧実験場所を正本として参照しない。

## Path Contract

- repo 内の起動、検証、生成物、アプリバンドル、バイナリへの参照は repo ルートからの相対パスで書く。
- 移動元など旧実験場所への絶対パスを README、source、config、script、document に再導入しない。
- repo 外の macOS system resource を参照する場合だけ、必要最小限の絶対パスを許可する。例: `/System/Library/Fonts/...`。
- 移動や昇格後の整備では、少なくとも次を検索して旧配置依存がないことを確認する。

- 検索時は、ユーザーが示した移動元ディレクトリ名、移動元絶対パス、ユーザー home 配下の不要な絶対パスを対象にする。

## Build And Runtime

- 開発実行は `cargo run` を使う。
- ネイティブ release バイナリは `cargo build --release` で生成し、`target/release/mini-system-monitor-rs` を実行対象とする。
- macOS `.app` バンドルや release binary は `target/` 配下の再生成可能成果物として扱い、source 正本へ混ぜない。
- `target/`、`.DS_Store`、一時スクリーンショット、ログ、cache は Git 管理対象にしない。

## Validation

- Rust source を変更した場合、原則として次を実行する。

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build
```

- release binary または `.app` の挙動が作業対象の場合は、必要に応じて `cargo build --release` も実行する。
- GUI 表示を変更した場合は、可能な範囲で起動確認とスクリーンショット確認を行う。画面ロック、権限、表示対象の制約で確認できない場合は、未検証範囲として報告する。

## macOS Sensors

- CPU 温度取得は macOS private IOHID 経路を含むため、OS 更新や権限条件で壊れる可能性がある前提で扱う。
- 温度表示は厳密な CPU core 温度と断定せず、Apple Silicon の SoC/PMU 系温度として慎重に表現する。
- 温度が取れない環境では `None` / `温度 --` へ落とす挙動を正常な fallback として維持する。
