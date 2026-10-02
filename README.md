# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/vision.md)、[MVP](docs/mvp.md)、[アーキテクチャ](docs/architecture.md)に実装方針をまとめています。

## ワークスペース

実行プロセスのパッケージは`apps/`、内部ライブラリは`crates/`に配置します。

| パッケージ | 今後の役割 |
| --- | --- |
| [`ene-host`](apps/ene-host/) | AI接続、会話、タスク、権限、永続化を管理するホスト |
| [`ene-desktop`](apps/ene-desktop/) | Slintの会話・設定UI、音声再生、キャラクター描画プロセスの管理 |
| [`ene-avatar`](apps/ene-avatar/) | VRM 1.0の透明なWayland常駐表示 |
| [`ene-api-types`](crates/ene-api-types/) | REST APIのDTO・OpenAPIスキーマ・通知同期規則 |
| [`ene-api-client`](crates/ene-api-client/) | 型付きHTTP操作・認証付き通知WebSocket |
| [`ene-protocol`](crates/ene-protocol/) | アバターIPC・プロバイダープラグインの通信型 |

## 第1段階：Slint会話画面を試す

現在の実装はSlint 1.18.1の会話画面です。日本語の複数行入力、Enter／ボタン送信、固定の日本語仮応答、表示設定、ksniのシステムトレイを備えています。会話は起動中のみ保持し、表示設定・VRM・配置は端末側に保存します。別プロセスのVRM常駐表示とVRMAモーション再生も実装しています。ホストはまだ骨組みで、実モデルとの対話・ファイル作業・音声は未実装です。

LinuxのWaylandセッションで起動してください。Nixを使わないDebian系の環境では、Rustに加えて次の依存が必要です（ほかのOSの対応は今回の対象外です）。

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers fonts-noto-cjk
just verify
just run-desktop
# justがない場合も、明示的なデモモードを起動できます。
cargo build --release --locked -p ene-avatar
ENE_AVATAR_BIN="$PWD/target/release/ene-avatar" cargo run --locked -p ene-desktop -- --demo
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
| `just verify` | 整形確認、Clippy（型検査を含む）、テスト、ビルドを順に実行 |
| `just run-host` | ホストの骨組みを起動 |
| `just run-desktop` | Slint会話画面を明示的なデモモードで起動 |
| `just fetch-assets` | 固定コミットの公式Seed-sanとVRMA全4本をチェックサム付きで取得（`vrm` / `vrma`で種類を選択、個別名で1本を取得） |
| `just run-avatar` | キャラクター描画を単独起動 |

## VRMキャラクターの常駐表示

KDE/KWinなどのWayland layer-shell対応環境で利用できます。XWaylandへの切替はありません。

```sh
just fetch-assets  # 固定コミット・SHA-256を検証して開発用VRM・VRMAをキャッシュへ取得
just run-desktop  # ene-avatarもビルドし、会話画面とキャラクターを起動
```

Seed-san model by VirtualCast, Inc. — [取得元](https://github.com/vrm-c/vrm-specification/tree/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san)／[VRM Public License 1.0](https://vrm.dev/licenses/1.0/)。モデル内のライセンス設定に従い、クレジットを表示します。モデルはGitへ含めず、`$XDG_CACHE_HOME/ene/models/Seed-san.vrm`（既定`~/.cache/ene/models/Seed-san.vrm`）に置きます。設定の「VRMを選ぶ」で手持ちの埋め込み形式のVRM 1.0を選択できます。ファイル選択はOS標準ダイアログを使います。Linuxでは`xdg-desktop-portal`とデスクトップ用バックエンド（KDEでは`xdg-desktop-portal-kde`）が必要です。キャンセル時は設定を変更せず、ポータルの失敗は設定画面に表示します。パス欄への直接入力も利用できます。

輪郭内のクリックは会話表示、ドラッグは配置変更です。透明部分はクリック透過し、キーボードフォーカスと作業領域を占有しません。初期値30fps、設定で60fpsへ変更できます。大きさは静止時の全身を基準に、100%で画面の高さの約40%、200%で約80%とします。設定で50〜200%を直接入力し、「適用して閉じる」で反映・保存できます。ドラッグは画面の端を越えて移動でき、負の配置も保存します。見失ったときは設定またはトレイの「キャラクターを画面中央へ戻す」で再表示・配置できます。モデル・モーション・サイズ・fps・配置の変更とトレイでの表示切替は再起動不要です。非表示中は描画・アニメーション更新を止め、GPUとモデルをメモリに保持して再表示時に再開します。出力変更では必要なWaylandウィンドウだけを更新します。モデルの読込失敗も設定から選び直せます。「再起動」は描画エラー時の復旧操作です。

`just run-desktop`と`just run-avatar`は描画プロセスを依存ライブラリも含めreleaseビルドします。Bevyを未最適化のdebugビルドで実行した性能とは区別してください。表示画像はGPUからWaylandへ渡し、入力領域もGPUで最終画像から生成します。通常動作で画像のCPU読み戻しは行いません。[描画設計](docs/avatar-gpu-rendering.md)を参照してください。

テーマ（システム・ライト・ダーク）・フォント・文字サイズ・VRM・大きさ・表示・fps・出力名・位置は`$XDG_CONFIG_HOME/ene/client.json`（既定`~/.config/ene/client.json`）へ保存します。終了時は描画プロセスへ停止を要求し、3秒以内に応答しなければ強制終了して回収します。`ENE_AVATAR_BIN`で描画バイナリの場所を指定できます。ハードウェアGPUのない試験環境だけで`ENE_AVATAR_CPU_GPU=1`を指定するとWGPUのソフトウェアアダプターを使用します。

`just run-avatar`で描画を単独起動できます。通常の回帰チェックは`just verify`で実行します。

初期設定では`just fetch-assets`で取得したthree-vrmのサンプルVRMAを自動で繰り返し再生します。保存済みのモーション設定（静止姿勢を含む）は優先します。VRMA 1.0は設定の「VRMAを選ぶ」から読み込み、再生・一時停止・繰り返しを設定できます。「静止姿勢に戻す」でモーションを外します。モーションの切替・解除では描画環境とモデルを維持します。足の接地を崩さないよう、腰の水平・上下移動と回転を保持し、画面上の配置とカメラの基準は固定します。表示フレームとクリック領域は同時に更新します。

`just fetch-assets vrma`でVRMA全4本を取得できます。引数なし（または`all`）ではSeed-sanも取得し、`vrm`ではSeed-sanだけを取得します。モーションの保存先は`$XDG_CACHE_HOME/ene/motions/`（既定`~/.cache/ene/motions/`）です。

| 個別取得コマンド | 保存ファイル | 動作・確認用途 |
| --- | --- | --- |
| `just fetch-assets vrma-sample` | `three-vrm-sample.vrma` | 右腕・表情・視線の基本サンプル（初期モーション） |
| `just fetch-assets vrma-dance` | `quaternius-dance.vrma` | ダンス。腕・脚・腰が同時に動く全身の追従確認 |
| `just fetch-assets vrma-walk` | `josie-walk.vrma` | 歩行。左右の腕振り・脚運びの確認 |
| `just fetch-assets vrma-slow-run` | `josie-slow-run.vrma` | スローラン。歩行より大きな膝の曲げ伸ばしの確認 |

取得後、設定の「VRMAを選ぶ」で保存先を開き、確認したいファイルを選択してください。短い動作は「繰り返す」を有効にすると確認しやすくなります。ダンス・歩行・スローランを取得しても、初期モーションや保存済みの設定は変更しません。

three-vrmのサンプルはCopyright (c) 2019-2026 pixiv Inc.／[MIT License](https://github.com/pixiv/three-vrm/blob/1b4fc0cc7ef39a49d62bb7a66dcfeca8f65316f7/LICENSE)です。追加3本はCC0-1.0です。作者・出典・ライセンスへのリンク・固定URL・SHA-256は[scripts/fetch-assets.py](scripts/fetch-assets.py)に記録しています。開発用サンプルは上流から直接キャッシュへ取得し、リポジトリや配布物には含めません。将来同梱する場合は、その時点で各アセットの再配布条件に合わせて通知を整えます。

Slintは[Royalty-free License 2.0](https://slint.dev/terms-and-conditions#slint-royalty-free-license-2-0)を利用し、設定画面のアプリ情報に`AboutSlint`を表示しています。

```sh
cargo run --release --locked -p ene-avatar -- --standalone --model /path/to/avatar.vrm --motion /path/to/motion.vrma
# 一度だけ再生する場合は --no-loop を追加
```
