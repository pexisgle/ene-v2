# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/vision.md)、[Alphaホストの範囲](docs/alpha.md)、[アーキテクチャ](docs/architecture.md)に実装方針をまとめています。

## 設計文書

現在の採用設計は[アーキテクチャ](docs/architecture.md)、[心と自動記憶](docs/design/mind.md)、[作業相手API](docs/design/work.md)、[権限](docs/design/permissions.md)、[ホストAPI](docs/api.md)です。[調査・選定根拠](docs/references.md#alpha-design-sources)と[実装順序・検証条件](docs/alpha.md)を分けて記録しています。設計の追加だけで実装済みにはなりません。現行の動作・起動方法は各パッケージのREADMEを参照してください。

## ワークスペース

実行プロセスのパッケージは`apps/`、内部ライブラリは`crates/`に配置します。

| パッケージ | 役割 |
| --- | --- |
| [`ene-host`](apps/ene-host/) | AI接続、会話、タスク、権限、永続化を管理するホスト |
| [`ene-desktop`](apps/ene-desktop/) | Slintの会話・設定UI、音声再生、キャラクター描画プロセスの管理 |
| [`ene-avatar`](apps/ene-avatar/) | 単一Bevy AppでのVRM描画、透過ウィンドウ、JSON Linesによる表示制御 |
| [`ene-plugin`](crates/ene-plugin/) | プラグイン作者向けの型定義とRust SDK |
| [`ene-plugin-runtime`](crates/ene-plugin-runtime/) | Extismの組み込み実行、検証、実行制限 |

ホストのアプリケーション処理、許可付きHTTP通信、Connect API、SQLite保存は `ene-host` にまとめています。起動・接続方法と内部構成は [ene-host のREADME](apps/ene-host/README.md) を参照してください。Wasmプラグインは `ene-plugin-runtime` からExtismを直接呼び出します。専用runner・自己再起動・Luau実行はありません。

## Nixで開発する

現在の開発シェルは`x86_64-linux`向けです。Nixのflakesとnix-commandを有効にした環境で、リポジトリのルートから実行します。

```sh
nix develop
just verify
just run-host
just run-desktop
```

direnvを使う場合は、リポジトリのルートで一度`direnv allow`を実行すると、ディレクトリへ入るたびに開発環境が有効になります。

## Nixを使わずに開発する

rustupと、そのOSでRustプログラムをリンクするための開発ツールを用意します。Linux（x86_64）ではClangとLLD、WindowsではMSVCのC++ビルドツールが必要です。

リポジトリのルートでnightlyツールチェーンをインストールし、通常のCargoで検査できます。

```sh
rustup toolchain install nightly-2026-10-07 --profile minimal --component rustfmt --component clippy --component rust-analyzer --component rust-src --component rustc-codegen-cranelift-preview --target wasm32-unknown-unknown
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown
cargo test --workspace --all-features --locked
cargo build --workspace --all-targets --all-features --locked
cargo run --locked -p ene-host
cargo run --locked -p ene-desktop
```

`just verify`は整形とClippy（テストを含む型検査）を実行します。テストは対象の挙動を変更したときに`just test`で実行し、変更していない合格済みのテストを繰り返す必要はありません。成果物が必要なときは`just build`を実行します。

## ビルド高速化

[Bevyのセットアップガイド](https://bevy.org/learn/quick-start/getting-started/setup/)に沿って、nightlyと次の設定を使います。

- Linux（x86_64）ではClang＋LLDでリンクし、`-Zshare-generics=y`でジェネリックのインスタンス化を共有します。
- 開発ビルドのワークスペース内クレートはCranelift、依存ライブラリはLLVMでコンパイルします。releaseビルドはLLVMを使います。
- 開発時の最適化はワークスペース内クレートを`opt-level = 1`、依存ライブラリを`opt-level = 3`に設定しています。

Nixではrust-overlayが`rust-toolchain.toml`を読み、必要なコンポーネントをまとめて提供します。Nixとrustupで同じnightlyを使うため、日付を`nightly-2026-10-07`に固定しています。更新時は`nix flake update rust-overlay`を実行し、`rust-toolchain.toml`と上記インストールコマンドの日付を揃えて変更します。

Wasmプラグイン用の `wasm32-unknown-unknown` ターゲットも共通設定に含めます。プラグイン実行にbubblewrapは不要です。

ツールチェーンとビルド設定の変更後は初回に再コンパイルが発生します。キャラクター描画はBevyの必要な機能のみ有効にし、`dynamic_linking`は使っていません。

## テストの方針

- 内部テストは、対象の実装ファイル末尾の `#[cfg(test)] mod tests` に置きます。複数モジュールの連携を検証する内部テストは、構成を担う `lib.rs` などにまとめます。
- 重大な失敗、状態破壊、参照の混線、数値計算の不変条件を重点的に検証します。設定値の写し合わせや、依存ライブラリの仕様を網羅するためだけのテストは増やしません。
- 補助コードは利用するテストモジュール内に置きます。テストの配置のために実装を公開したり、Cargoの自動検出を無効にしたりしません。
- `tests/` は公開APIや実行ファイルへの入出力を外部から検証する結合テストに使います。
- シェーダーなど通常のCPUテストで確認できない重要な処理は、環境依存のテストとして分け、対応するCIで実行します。

## 開発コマンド

| コマンド | 処理 |
| --- | --- |
| `just` | 利用可能なレシピの一覧 |
| `just fmt` | ワークスペース全体のRustコードを整形 |
| `just fmt-check` | 整形済みかを確認 |
| `just check` | 全クレート・ターゲット・機能を型検査 |
| `just clippy` | 全クレート・ターゲット・機能を検査し、警告をエラーにする |
| `just test` | テスト用Wasmをビルドし、ワークスペースのテストを実行 |
| `just build` | 全クレート・ターゲット・機能をビルド |
| `just fetch-assets [asset]` | 開発用VRM・VRMAを取得（省略時は全件、`vrma`でモーションのみ、個別名も指定可能） |
| `just verify` | 整形確認とClippy（テストを含む型検査）を実行 |
| `just build-test-plugins` | 結合テスト用の参照Wasmをビルド |
| `just run-host` | 認証付きホストAPIを起動 |
| `just run-desktop` | デスクトップの骨組みを起動 |
| `just run-avatar` | [JSON Linesで操作する透過描画プロセス](apps/ene-avatar/README.md)を起動 |
| `just run-avatar-cli` | 本体をビルドし、対話式 CLI からアバターを操作 |
