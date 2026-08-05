# mini-system-monitor-rs Local Constitution

このファイルは、このリポジトリ配下における Codex の局所 `AGENTS.md` であり、本スコープ内の実行条件を定義する SSOT である。
本書は助言集ではなく、既定動作、禁止事項、優先順位、ツール選択、検証条件を拘束する運用契約として扱う。
上位の `AGENTS.md`、システム指示、開発者指示、ユーザーの明示要求と競合する場合は、Codex の優先順位規則に従う。
より深い階層に別の `AGENTS.md` が存在する場合、そのスコープではより局所のファイルを優先する。

## Scope

- この repo は Rust と `eframe/egui` で実装されたmacOS GUIアプリである。
- Git checkoutのrepo rootを `mini-system-monitor-rs` のソース正本として扱う。
- clone元以外のローカル作業コピー、生成物、cache、旧実験場所を正本として参照しない。

## Path Contract

- repo 内の起動、検証、生成物、アプリバンドル、バイナリへの参照は repo ルートからの相対パスで書く。
- ローカル作業コピーや旧実験場所への絶対パスを README、source、config、script、document に導入しない。
- repo 外の macOS system resource を参照する場合だけ、必要最小限の絶対パスを許可する。例: `/System/Library/Fonts/...`。
- 公開整備では、ユーザーhome配下の絶対パスと既知の旧配置名を検索し、配布対象の正本に残さない。

## Build And Runtime

- fresh clone では最初に `cargo fetch --locked` を実行し、`Cargo.lock` の依存関係を取得する。
- 開発実行は `cargo run --locked` を使う。
- ネイティブ release バイナリは `cargo build --release --locked` で生成し、`target/release/mini-system-monitor-rs` を実行対象とする。
- macOS `.app` バンドルや release binary は `target/` 配下の再生成可能成果物として扱い、source 正本へ混ぜない。
- `target/`、`.DS_Store`、一時スクリーンショット、ログ、cache は Git 管理対象にしない。

## Installation

- Codex は repo root の本書を読み、ユーザーがインストールを明示した場合だけ `Scripts/install-app.sh --destination "/Applications/Mini System Monitor.app"` を使う。
- インストール前に `cargo fetch --locked` を完了する。置換対象の `.app` に含まれる `mini-system-monitor-rs` が実行中の場合は停止し、ユーザーへ終了を求める。
- 初回インストールでは既存の宛先がないことを要求する。既存アプリの置換は、ユーザーがその置換を明示した場合だけ `--replace` を付ける。
- `--replace` は旧アプリを同じディレクトリの時刻付き `.previous-*.app` へ退避してから新しいアプリを設置する。退避先を最終報告に含める。
- ビルド済みアプリの手動コピーや既存アプリの直接削除で、source-owned installerを迂回しない。
- インストーラー変更時の主要経路確認には、一時ディレクトリを `--destination` の親として使い、実際の `/Applications` を変更しない。

## Validation

- Rust source を変更した場合、原則として次を実行する。

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --all-features -- -D warnings
cargo build --locked
```

- release binary または `.app` の挙動が作業対象の場合は、必要に応じて `cargo build --release --locked` も実行する。
- GUI 表示を変更した場合は、可能な範囲で起動確認とスクリーンショット確認を行う。画面ロック、権限、表示対象の制約で確認できない場合は、未検証範囲として報告する。

## macOS Sensors

- CPU 温度取得は macOS private IOHID 経路を含むため、OS 更新や権限条件で壊れる可能性がある前提で扱う。
- 温度表示は厳密な CPU core 温度と断定せず、Apple Silicon の SoC/PMU 系温度として慎重に表現する。
- 温度が取れない環境では `None` / `温度 --` へ落とす挙動を正常な fallback として維持する。
