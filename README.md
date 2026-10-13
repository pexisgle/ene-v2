# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/vision.md)、[MVP](docs/mvp.md)、[アーキテクチャ](docs/architecture.md)に実装方針をまとめています。

## ワークスペース

実行プロセスのパッケージは`apps/`、内部ライブラリは`crates/`に配置します。

| パッケージ | 今後の役割 |
| --- | --- |
| [`ene-host`](apps/ene-host/) | AI接続、会話、タスク、権限、永続化を管理するホスト |
| [`ene-desktop`](apps/ene-desktop/) | Slintの会話・設定UI、音声再生、キャラクター描画プロセスの管理 |

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
rustup toolchain install nightly-2026-10-07 --profile minimal --component rustfmt --component clippy --component rust-analyzer --component rust-src --component rustc-codegen-cranelift-preview
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
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

ツールチェーンとビルド設定の変更後は初回に再コンパイルが発生します。Craneliftは開発用の設定で、Wasmには対応していません。現在はBevyが未導入のため、`dynamic_linking`はキャラクター描画クレートを追加した際に開発用機能として有効にします。

## 開発コマンド

| コマンド | 処理 |
| --- | --- |
| `just` | 利用可能なレシピの一覧 |
| `just fmt` | ワークスペース全体のRustコードを整形 |
| `just fmt-check` | 整形済みかを確認 |
| `just check` | 全クレート・ターゲット・機能を型検査 |
| `just clippy` | 全クレート・ターゲット・機能を検査し、警告をエラーにする |
| `just test` | ワークスペースのテストを実行 |
| `just build` | 全クレート・ターゲット・機能をビルド |
| `just fetch-assets [asset]` | 開発用VRM・VRMAを取得（省略時は全件、`vrma`でモーションのみ、個別名も指定可能） |
| `just verify` | 整形確認とClippy（テストを含む型検査）を実行 |
| `just run-host` | ホストの骨組みを起動 |
| `just run-desktop` | デスクトップの骨組みを起動 |
