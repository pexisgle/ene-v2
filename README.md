# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/vision.md)、[全体アーキテクチャ](docs/architecture.md)に実装方針をまとめています。

## 設計文書

現在の採用設計は[全体アーキテクチャ](docs/architecture.md)、[技術スタック・実装リファレンス](docs/design/tech-stack.md)、[心と自動記憶](docs/design/mind.md)、[作業相手「手」API](docs/design/work.md)、[コンテキスト圧縮・セッション管理](docs/design/compaction.md)、[権限・Sandbox・セキュリティ](docs/design/permissions.md)、[知覚・音声・アバターの共通仕様](docs/design/voice-avatar.md)、[デスクトップクライアント実装](docs/design/desktop-client.md)、[プラグイン拡張](docs/design/plugins.md)、[ホストAPI](docs/api.md)です。[調査・選定根拠](docs/references.md#alpha-design-sources)を参照してください。現行の動作・起動方法は各パッケージのREADMEを参照してください。

## ワークスペース

実行プロセスのパッケージは`apps/`、内部ライブラリは`crates/`に配置します。

| パッケージ | 役割 |
| --- | --- |
| [`ene-host`](apps/ene-host/) | AI接続、会話、タスク、権限、永続化を管理するホスト |
| [`ene-desktop`](apps/ene-desktop/) | ホストへ接続し、アバター描画・音声デバイス・知覚を担うデスクトップクライアント |
| [`ene-avatar`](apps/ene-avatar/) | 単一Bevy AppでのVRM描画、透過ウィンドウ、JSON Linesによる表示制御 |
| [`ene-plugin`](crates/ene-plugin/) | プラグイン作者向けの型定義とRust SDK |
| [`ene-plugin-runtime`](crates/ene-plugin-runtime/) | ホストに組み込まれたExtism実行、検証、実行制限 |

ホストのアプリケーション処理、許可付きHTTP通信、Connect API、SQLite保存は `ene-host` にまとめています。起動・接続方法と内部構成は [ene-host のクレートドキュメント](apps/ene-host/src/lib.rs) を参照してください。Wasmプラグインは `ene-plugin-runtime` がExtismを直接呼び出します。専用runner・自己再起動・Luau実行はありません。`ene-host` はこの実行基盤をリンクし、有効なプロバイダーを起動してLLM・TTS・STT接続に使用します。型とSDKは `ene-plugin` が提供します。

## Nixで開発する

現在の開発シェルは`x86_64-linux`向けです。Nixのflakesとnix-commandを有効にした環境で、リポジトリのルートから実行します。

```sh
nix develop
just verify
just run
```

direnvを使う場合は、リポジトリのルートで一度`direnv allow`を実行すると、ディレクトリへ入るたびに開発環境が有効になります。

## Nixを使わずに開発する

rustupと`buf`、そのOSでRustプログラムをリンクするための開発ツールを用意します。Linux（x86_64）ではClangとLLD、WindowsではMSVCのC++ビルドツールが必要です。`buf`はプロトコル生成に使い、CIとNixの開発シェルでは1.73.0に固定しています。

リポジトリのルートで固定バージョンのツールチェーンをインストールし、通常のCargoで検査できます。

```sh
rustup toolchain install 1.100.0 --profile minimal --component rustfmt --component clippy --component rust-analyzer --component rust-src --target wasm32-unknown-unknown
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown
cargo test --workspace --all-features --locked
cargo build --workspace --all-targets --all-features --locked
cargo run --locked -p ene-host
```

`just verify`は整形とClippy（テストを含む型検査）を実行します。テストは対象の挙動を変更したときに`just test`で実行し、変更していない合格済みのテストを繰り返す必要はありません。成果物が必要なときは`just build`を実行します。

## Windows インストーラーを作る

Windows 10/11 x64 向けのセットアップ EXE を生成します。上記の Windows 開発環境に加え、x64 MSVC の再頒布用ランタイムファイルと [Inno Setup 6.3 以降](https://jrsoftware.org/inno-setup/downloads)を用意し、リポジトリのルートで実行します。

```powershell
just installer-windows
```

`target/installer/ene-<バージョン>-windows-x64-setup.exe` が生成されます。インストーラーは管理者権限なしで実行でき、デスクトップ・ローカルホスト・アバター描画の実行ファイル、MSVC ランタイム、`assets/ENE.vrm`、`assets/vrma/` 直下の採用済み VRMA を収録します。

スタートメニューの **ene** またはインストール先の `ene-desktop.exe` を開くと、隣にある `ene-host.exe` も起動します。タスクトレイの **終了** でデスクトップを終了すると、ホストも終了します。`ENE_HOST_CONNECTION` で接続先を指定した場合は、そのホストに接続します。ローカルホストは既定でアバターのモーションと作業フォルダーのファイル操作を許可し、端末で有効にした画面操作も受け付けます。**設定 → この端末 → 許可ルール → すべて省略** で、許可済みツールの既定の確認を省略できます。

会話や設定はユーザーのデータフォルダーに保存されます。公開配布前には、`ENE.vrm` に記録されている作者（pexisgle）と配布条件（`allowRedistribution: false`）を確認してください。

## ビルド高速化

[Bevyのセットアップガイド](https://bevy.org/learn/quick-start/getting-started/setup/)に沿って、次の設定を使います。

- Linux（x86_64）ではClang＋LLDでリンクします。
- 開発時の最適化はワークスペース内クレートを`opt-level = 1`、依存ライブラリを`opt-level = 3`に設定しています。CIでは依存ライブラリも`opt-level = 0`で検査し、コンパイル時間を優先します。

Nixではrust-overlayが`rust-toolchain.toml`を読み、必要なコンポーネントをまとめて提供します。Nixとrustupで同じツールチェーンを使うため、バージョンを`1.100.0`に固定しています。更新時は`nix flake update rust-overlay`を実行し、`rust-toolchain.toml`と上記インストールコマンドのバージョンを揃えて変更します。

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
| `just installer-windows` | キャラクターとモーションを同梱した Windows x64 インストーラーを生成 |
| `just fetch-assets` | 開発用の Seed-san VRM モデルを取得。モーションは `assets/vrma` に同梱 |
| `just verify` | 整形確認とClippy（テストを含む型検査）を実行 |
| `just build-test-plugins` | 結合テスト用の参照Wasmをビルド |
| `just run` | ホストとデスクトップクライアント（アバター描画付き）を同時に起動 |
| `just run-host` | 認証付きホストAPIを起動 |
| `just run-desktop` | [ホストへ接続するデスクトップクライアント](apps/ene-desktop/src/lib.rs)を起動 |
| `just run-avatar` | [JSON Linesで操作する透過描画プロセス](apps/ene-avatar/src/main.rs)を起動 |
| `just run-avatar-cli` | 本体をビルドし、対話式 CLI からアバターを操作 |

