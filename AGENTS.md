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
- `cargo run --locked` は明示的な開発専用実行にだけ使う。通常利用、インストール済みアプリ、動作確認のhandoff、またはバイナリ欠落時のfallbackに使わない。
- 通常利用向けのローカルruntimeは `Scripts/materialize-app.sh` で生成する `dist/Mini System Monitor.app` とする。`Scripts/run-app.sh` はbundleとnative executableの存在・実行権限を確認してからこのapp bundleを直接開き、Cargoを経由しない。
- `dist/Mini System Monitor.app` が存在しない、bundle内の `Contents/MacOS/mini-system-monitor-rs` が存在しない、または実行可能でない場合、`Scripts/run-app.sh` は起動を停止し、repo rootで `Scripts/materialize-app.sh` を実行するよう報告する。`cargo run`、`target/` 内の探索、compile-if-missingへfallbackしない。
- `Scripts/build-app.sh` とCargoのrelease出力はconstruction専用とする。`target/` 内のbinaryやapp bundleを通常実行先、インストール元、またはhandoff成果物にしない。
- `dist/`、`target/`、`.DS_Store`、一時スクリーンショット、ログ、cacheはGit管理対象に含めず、source正本と同一視しない。

## Installation

- Codex は repo root の本書を読み、ユーザーがインストールを明示した場合だけ `Scripts/install-app.sh --destination "/Applications/Mini System Monitor.app"` を使う。
- インストール後の通常runtimeは指定した `.app` bundle内のnative executableを直接起動する。インストーラーによるCargo buildはinstallation時のconstructionであり、アプリ起動時には実行しない。
- インストール前に `cargo fetch --locked` を完了する。置換対象の `.app` に含まれる `mini-system-monitor-rs` が実行中の場合は停止し、ユーザーへ終了を求める。
- 初回インストールでは既存の宛先がないことを要求する。既存アプリの置換は、ユーザーがその置換を明示した場合だけ `--replace` を付ける。
- `--replace` は旧アプリを同じディレクトリの時刻付き `.previous-*.app` へ退避してから新しいアプリを設置する。退避先を最終報告に含める。
- ビルド済みアプリの手動コピーや既存アプリの直接削除で、source-owned installerを迂回しない。
- インストーラー変更時の主要経路確認には、一時ディレクトリを `--destination` の親として使い、実際の `/Applications` を変更しない。

## Validation

- Rust source またはbuild scriptを変更した場合は、次のsource-owned検証経路を実行する。

```bash
Scripts/check.sh
```

- `Scripts/check.sh` はrepo外の専用一時ディレクトリにCargo targetを作成し、正常終了、エラー終了、SIGINTまたはSIGTERMによる中断のいずれでも終了処理で削除する。検証目的でrepo直下の `target/` を作成または再利用しない。

- materializationまたは通常runtime経路が作業対象の場合は、`Scripts/materialize-app.sh` を実行し、`dist/Mini System Monitor.app` の署名、bundle executableの存在・実行権限、Cargoを介さない代表的な直接起動を一つの受け入れ束で確認する。
- GUI 表示を変更した場合は、可能な範囲で起動確認とスクリーンショット確認を行う。画面ロック、権限、表示対象の制約で確認できない場合は、未検証範囲として報告する。

## macOS Sensors

- CPU 温度取得は macOS private IOHID 経路を含むため、OS 更新や権限条件で壊れる可能性がある前提で扱う。
- 温度表示は厳密な CPU core 温度と断定せず、Apple Silicon の SoC/PMU 系温度として慎重に表現する。
- 温度が取れない環境では `None` / `温度 --` へ落とす挙動を正常な fallback として維持する。
