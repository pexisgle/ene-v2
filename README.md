# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/vision.md)、[MVP](docs/mvp.md)、[アーキテクチャ](docs/architecture.md)に実装方針をまとめています。

## ワークスペース

実行プロセスのパッケージは`apps/`、内部ライブラリは`crates/`に配置します。

| パッケージ | 今後の役割 |
| --- | --- |
| [`ene-host`](apps/ene-host/) | AI接続、会話、タスク、権限、永続化を管理するホスト |
| [`ene-desktop`](apps/ene-desktop/) | Slintの会話・設定UI、音声再生、キャラクター描画プロセスの管理 |
| [`ene-avatar`](apps/ene-avatar/) | VRM 1.0の透明なWayland常駐表示 |
| [`ene-shared`](crates/ene-shared/) | クライアントと描画プロセスの共通設定・IPC型 |

## 第1段階：Slint会話画面を試す

現在の実装はSlint 1.18.1の会話画面です。日本語の複数行入力、ボタン送信、固定の日本語仮応答、表示設定、Slintのシステムトレイを備えています。会話は起動中のみ保持し、表示設定・VRM・配置は端末側に保存します。別プロセスのVRM常駐表示とVRMAモーション再生も実装しています。ホストはまだ骨組みで、実モデルとの対話・ファイル作業・音声は未実装です。

LinuxのWaylandセッションで起動してください。Nixを使わないDebian系の環境では、Rustに加えて次の依存が必要です（ほかのOSの対応は今回の対象外です）。

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers fonts-noto-cjk
just verify
just run-desktop
# justがない場合も、明示的なデモモードを起動できます。
cargo run --locked -p ene-desktop -- --demo
```

`--demo`を付けない起動ではホスト未接続を表示し、送信を無効にします。デモはAPIキーを必要とせず、外部へ会話を送信しません。Enterで送信、Shift+Enterで改行します。IMEの変換・確定中は送信しません。送信ボタンも利用できます。正常にトレイ登録できた場合、×は画面の非表示、トレイクリックは再表示、トレイメニューは設定・クライアント終了です。登録を確認できない場合は画面を開いたままにし、設定画面の終了ボタンを利用できます。

ネイティブIMEの候補位置・確定・取消とKDE/KWinでの操作確認は実機で行ってください。

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

rustupと、そのOSでRustプログラムをリンクするための開発ツールを用意します。LinuxではCコンパイラー、WindowsではMSVCのC++ビルドツールが必要です。

リポジトリのルートで固定したツールチェーンをインストールし、通常のCargoで検査できます。

```sh
rustup toolchain install 1.98.1 --profile minimal --component rustfmt --component clippy --component rust-analyzer --component rust-src
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-targets --all-features --locked
cargo run --locked -p ene-host
cargo run --locked -p ene-desktop
```

justをインストールしている場合は、同じ検査を`just verify`で実行できます。

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
| `just verify` | 整形確認、型検査、Clippy、テスト、ビルドを順に実行 |
| `just run-host` | ホストの骨組みを起動 |
| `just run-desktop` | Slint会話画面を明示的なデモモードで起動 |
| `just fetch-avatar` | 固定コミットの公式Seed-sanをチェックサム付きで取得 |
| `just fetch-vrma` | 固定コミットのVRMAサンプルをチェックサム付きで取得 |
| `just run-avatar` | キャラクター描画を単独起動 |

## VRMキャラクターの常駐表示

KDE/KWinなどのWayland layer-shell対応環境で利用できます。XWaylandへの切替はありません。

```sh
just fetch-avatar  # 固定コミット・SHA-256を検証して開発用Seed-sanをキャッシュへ取得
just run-desktop  # ene-avatarもビルドし、会話画面とキャラクターを起動
```

Seed-san model by VirtualCast, Inc. — [モデルの利用条件・取得元](licenses/Seed-san.md)。モデルはGitへ含めず、`$XDG_CACHE_HOME/ene/models/Seed-san.vrm`（既定`~/.cache/ene/models/Seed-san.vrm`）に置きます。設定の「VRMを選ぶ」で手持ちの埋め込み形式のVRM 1.0を選択できます。ファイル選択はアプリ内で行い、フォルダ移動とファイルパスの貼り付けに対応します。外部ポータルは不要です。

輪郭内のクリックは会話表示、ドラッグは配置変更です。透明部分はクリック透過し、キーボードフォーカスと作業領域を占有しません。トレイから表示切替・再起動を行えます。初期値30fps、設定で60fpsへ変更できます。大きさは設定で50〜200%に変更できます。非表示時はGPU描画を止めて資源を解放し、再表示時にモデルを読み直します。

テーマ（システム・ライト・ダーク）・フォント・文字サイズ・VRM・大きさ・表示・fps・出力名・位置は`$XDG_CONFIG_HOME/ene/client.json`（既定`~/.config/ene/client.json`）へ保存します。終了時は描画プロセスへ停止を要求し、3秒以内に応答しなければ強制終了して回収します。`ENE_AVATAR_BIN`で描画バイナリの場所を指定できます。ハードウェアGPUのない試験環境だけで`ENE_AVATAR_CPU_GPU=1`を指定するとWGPUのソフトウェアアダプターを使用します。

`just run-avatar`で描画を単独起動できます。通常の回帰チェックは`just verify`で実行します。

VRMA 1.0は設定の「VRMAを選ぶ」から読み込み、再生・一時停止・繰り返しを設定できます。「静止姿勢に戻す」でモーションを外します。常駐表示では横移動を固定し、上下の動きと回転を保ちます。表示フレームとクリック領域は同時に更新します。

`just fetch-vrma`で[pixiv three-vrmのサンプル](https://github.com/pixiv/three-vrm/blob/1b4fc0cc7ef39a49d62bb7a66dcfeca8f65316f7/packages/three-vrm-animation/examples/models/test.vrma)を取得できます。保存先は`$XDG_CACHE_HOME/ene/motions/three-vrm-sample.vrma`（既定`~/.cache/ene/motions/three-vrm-sample.vrma`）です。取得後、設定の「VRMAを選ぶ」でこのファイルを指定してください。Copyright (c) 2019-2026 pixiv Inc.／[MIT License](licenses/three-vrm-animation.txt)。ライセンスはキャッシュにも保存します。

```sh
cargo run --locked -p ene-avatar -- --standalone --model /path/to/avatar.vrm --motion /path/to/motion.vrma
# 一度だけ再生する場合は --no-loop を追加
```
