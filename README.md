# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/vision.md)、[MVP](docs/mvp.md)、[アーキテクチャ](docs/architecture.md)に実装方針をまとめています。

## ワークスペース

実行プロセスのパッケージは`apps/`、内部ライブラリは`crates/`に配置します。ホストのAPIは`apps/ene-host/src/api/v1/`にリソース別で配置し、親子のRouterを`nest`で組み立てます。`api/`に認証と共通の入力処理を集約し、対話処理・DB処理を分離します。共有状態は最上位のRouterで渡します。規則と処理手順は`ene-core`、SQL・OS依存の実装はホストの`infrastructure/`へ配置し、APIからapplicationを呼びます。

| パッケージ | 役割 |
| --- | --- |
| [`ene-host`](apps/ene-host/) | AI接続、会話、タスク、権限、永続化を管理するホスト |
| [`ene-desktop`](apps/ene-desktop/) | Slintの会話・設定UI、音声再生、キャラクター描画プロセスの管理 |
| [`ene-avatar`](apps/ene-avatar/) | VRM 1.0の透明なWayland常駐表示 |
| [`ene-core`](crates/ene-core/) | ホストの規則・処理手順と外部処理へのインターフェース |
| [`ene-api-types`](crates/ene-api-types/) | REST APIの型・OpenAPIスキーマ・通知再同期 |
| [`ene-api-client`](crates/ene-api-client/) | 型付きHTTP操作・認証付き通知WebSocket |
| [`ene-protocol`](crates/ene-protocol/) | アバターIPC・プロバイダープラグインの通信契約 |

## 起動と検証

現在の実装はSlint 1.18.1の単一ウィンドウです。「対話」と「設定」を切り替えて利用します。日本語の複数行入力、Enter／ボタン送信、固定の日本語仮応答、表示設定、ksniのシステムトレイを備えています。デモの会話は起動中のみ保持し、ホスト接続時の会話はSQLiteへ保存します。表示設定・VRM・配置は端末側に保存します。別プロセスのVRM常駐表示とVRMAモーション再生も実装しています。ホストの認証・初期化・履歴・通知購読も実装しています。OpenRouterとの逐次テキスト対話・再接続・設定・送信許可も利用できます。ファイル作業と音声は未実装です。

LinuxのWaylandセッションで起動してください。Nixを使わないDebian系の環境では、Rustに加えて次の依存が必要です（ほかのOSの対応は今回の対象外です）。

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers fonts-noto-cjk
just verify
just run-desktop
# justがない場合も、明示的なデモモードを起動できます。
cargo build --release --locked -p ene-avatar
ENE_AVATAR_BIN="$PWD/target/release/ene-avatar" cargo run --locked -p ene-desktop -- --demo
```

`--demo`を付けない起動ではホスト選択から「このPCのホストに接続」を選びます。接続・モデル・資格情報・送信許可が揃うまで送信を無効にします。デモはAPIキーを必要とせず、外部へ会話を送信しません。Enterで送信、Shift+Enterで改行します。IMEの変換・確定中は送信しません。送信ボタンも利用できます。正常にトレイ登録できた場合、×は画面の非表示、トレイクリックは再表示、トレイメニューから設定やホスト選択画面を開けます。クライアント終了はホスト選択画面から選びます。登録を確認できない場合は画面を開いたままにし、ホスト選択画面の終了操作を利用できます。

ネイティブIMEの候補位置・確定・取消とKDE/KWinでの操作確認は実機で行ってください。

ホストは`just run-host`で起動します。現在のユーザーが所有する非公開の`XDG_RUNTIME_DIR`が必要です。接続先と起動ごとのBearerトークンは`$XDG_RUNTIME_DIR/ene-host/connection.json`、DBは`$XDG_STATE_HOME/ene/host.sqlite3`（既定`~/.local/state/ene/host.sqlite3`）へ保存します。systemd用テンプレートは[ene-host.service](packaging/systemd/ene-host.service)です。

## ホストとのテキスト対話

`just run-host`と、別の端末で`just connect-desktop`を起動します。ホスト選択から接続し、設定の「ホスト・接続」でプロバイダーを選び、接続先とモデルIDを保存します。組込OpenRouterでは、会話用・作業用の検証モデルは`openai/gpt-6-luna`、配信先はOpenAI限定、フォールバックは禁止です。次にAPIキーを登録します。ホストの`OPENROUTER_API_KEY`を使う操作も用意しています。環境変数由来のキーはホスト起動中だけ保持し、手入力のキーはOSのSecret Serviceへ保存します。ストアが使えなければ起動中だけ保持し、その状態を画面へ表示します。キーをクライアント設定・DB・取得API・ログへ保存しません。

「権限とデータ」で会話送信と疎通確認・モデル情報の送受信を別々に許可し、保存します。許可後に「疎通確認」で実モデルの逐次応答を確認できます。対話は生成途中から表示し、クライアント再起動後も履歴を取得します。「過去の会話を読み込む」でさらに遡れます。通知購読→スナップショット→連番差分の順に同期し、欠番・epoch変更・Unicodeスカラーoffset不一致では購読をやり直します。再接続は1・2・4・8・15秒、受付が不明な発言は同じIdempotency-Keyで確認します。許可・資格情報の解除は生成を待たず新規送信を禁止し、返答を中断状態で保存します。ホスト再起動時の未完了生成・疎通確認は自動再実行しません。

別クライアント向けの契約は、認証付き`GET /v1/openapi.json`で取得できます。`cargo run --locked -p ene-host -- --print-openapi`でも、ホストの起動・DBの変更なしでOpenAPI 3.1 JSONを出力できます。Axumのルート登録と共通DTOから生成するため、仕様用のルート一覧を別管理しません。Rustからは`ene-api-client::ApiClient`を利用します。平文HTTPはIPリテラルのループバックだけ、その他は検証付きHTTPSを要求し、BearerをURLへ含めず、リダイレクトを追いません。現行ホストは引き続き同一PCのループバック限定です。

```rust,no_run
use ene_api_client::{ApiClient, IdempotencyKey, types::{RuntimeConnection, MessageInput}};
# async fn example(connection: RuntimeConnection) -> Result<(), ene_api_client::Error> {
let client = ApiClient::new(&connection)?;
let mut events = client.subscribe().await?;
let snapshot = client.snapshot(Some(&events.ready().connection_id)).await?;
// スナップショット取得中の通知を保持し、Replicaで連番・Unicode offsetを検査する。
let key = IdempotencyKey::new("one-user-intent")?;
let input = MessageInput { text: "こんにちは".into(), resources: vec![], question_id: None, decision: None };
let accepted = client.send_message(&snapshot.companions[0].id, &input, &key).await?;
// 受付結果が不明なら、同じinputとkeyで再確認する。自動再送は行わない。
events.close().await?;
# Ok(()) }
```

HTTP応答・通知は各16MiBまでで、異常・上限超過は同期をやり直します。OpenAPIの`x-ene-events`にWebSocketのフレーム型と購読→スナップショット→差分適用の手順を記載しています。ブラウザー向けの認証・CORSやリモートホストの運用は別途対応します。

systemdユーザーサービスを使う場合は、`cargo build --release --locked -p ene-host`後にバイナリを`~/.local/bin/ene-host`、既存テンプレートを`~/.config/systemd/user/ene-host.service`へ配置し、`systemctl --user daemon-reload`と`systemctl --user enable --now ene-host`を実行します。APIキーは画面から登録でき、環境変数を利用する場合はサービス側に渡す必要があります。クライアント終了でホストを停止しません。

## プロバイダープラグイン

Linuxの`bubblewrap`が必要です（Debian系では`sudo apt-get install bubblewrap`）。`$XDG_DATA_HOME/ene/providers/`（既定`~/.local/share/ene/providers/`、`ENE_PROVIDER_DIR`で変更可能）の各フォルダに`manifest.json`と実装を配置します。発見だけでは実行しません。設定のプロバイダー一覧で再走査し、選択して有効化すると隔離環境で初期化します。有効化は現在のホスト起動中に適用され、再起動後には再度有効化します。接続設定・モデル割当・許可はDBに保存します。

継続利用できる[OpenRouterプラグインの実装例](examples/providers/openrouter/)を用意しています。デフォルトはRust製の実行ファイルです。プラグインはNDJSONを扱える言語で書けるため、Python版も選択できます。Rust版・ホストの実行にPythonは不要です。

```sh
# x86_64 Linux向けに静的リンクし、隔離環境へ動的ライブラリを追加せずに実行する。
rustup target add --toolchain 1.98.1 x86_64-unknown-linux-musl
rustup run 1.98.1 cargo build --release --locked --target x86_64-unknown-linux-musl -p ene-provider-openrouter
provider_dir="${ENE_PROVIDER_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/ene/providers}/openrouter-plugin"
install -d -m 700 "$provider_dir"
install -m 755 target/x86_64-unknown-linux-musl/release/ene-provider-openrouter "$provider_dir/provider"
install -m 600 examples/providers/openrouter/manifest.json "$provider_dir/manifest.json"
```

Python版を選ぶ場合は、同じフォルダへ`provider.py`を置き、manifestの`command`をPython起動に変更します。隔離環境の`/usr/bin/python3`が必要です。切替・他のCPU向けの配置は[実装例のREADME](examples/providers/openrouter/README.md)を参照してください。

有効化後に接続・モデル・資格情報を登録し、表示された送信先と用途を許可してください。この例はOpenRouter専用で、OpenAI限定・フォールバックなしです。組込の`openrouter`とは別の`openrouter-plugin`として登録されます。他サービスには、そのAPI形式と送信条件に合わせた別の実装を用意します。モデル一覧がない場合もモデルIDを直接入力できます。宣言した設定項目だけをUIで扱い、任意のUIコードは実行しません。

プラグインは隔離した別プロセスで起動し、直接のネットワーク・ホストDB・資格情報・ホームへのアクセスを公開しません。資格情報はホストがHTTP brokerで付与します。初期認証方式は認証なし・Bearer・固定ヘッダー・クエリです。OAuth・署名方式は未対応です。APIバージョンなどの非秘密の固定ヘッダーはmanifestの`headers`で宣言します。外部通信は宣言したHTTPS送信先、または明示許可したHTTPループバックに限定し、リダイレクトを拒否します。OpenRouter仲介時の実処理先とフォールバック禁止はbrokerも強制します。隔離失敗時の無制限起動、別実装への自動切替、生成の自動再送はありません。

通信はversion 1のNDJSON（改行込み1MiB、待ち行列16件、1生成あたりHTTPは直列・最大8要求）です。アバターIPCの64KiB制限とは分離しています。生成・モデル取得ごとにプロセスを起動し、終了時には停止を要求して3秒後に強制終了・回収します。取消・許可や資格情報の解除では通信を中断し、遅着応答を破棄します。実装・manifest・接続条件が変わった場合には再走査・有効化・送信許可の再設定が必要です。通信型とmanifestの契約は[`ene-protocol::provider`](crates/ene-protocol/src/provider.rs)に定義しています。

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
| `just run-host` | 認証付きループバックホストを起動 |
| `just run-desktop` | Slint会話画面を明示的なデモモードで起動 |
| `just connect-desktop` | このPCのホストへ接続する会話画面を起動 |
| `just fetch-assets` | 固定コミットの公式Seed-sanとVRMA全4本をチェックサム付きで取得（`vrm` / `vrma`で種類を選択、個別名で1本を取得） |
| `just run-avatar` | キャラクター描画を単独起動 |

## 対話・設定UI

通常ウィンドウは1つで、avatarは別の透明な常駐ウィンドウです。起動時はホスト選択画面を開きます。実ホストへの接続と履歴の再同期に対応し、`--demo`起動ではここから「対話を始める」を選びます。デモであることは対話画面上部に表示します。設定画面からホスト選択へ戻れ、クライアント終了も同じ画面で選べます。設定画面を開いても会話の下書き・履歴・閲覧位置を維持します。初期サイズ720×760、最小420×480とし、幅と文字サイズに応じてカテゴリ一覧・設定フォーム・入力欄の配置を変更します。

設定の「この端末」では外観、デスクトップ表示、ローカルVRM・VRMAを変更できます。「保存」で永続化してから反映し、設定画面に留まります。「変更を戻す」で最新の保存済み値へ戻ります。表示切替・中央へ戻す・描画の再起動は即時操作で、フォームの編集中の変更を保存しません。未保存の設定を残したまま終了する場合は、破棄するか画面へ戻るか選択します。

「コンパニオン」で名前と口調、「ホスト・接続」でプロバイダー・モデル・資格情報、「権限とデータ」で用途ごとの送信許可を管理します。音声・フォルダ作業はまだ利用できません。設定の所有者と将来の音声操作については[デスクトップUI設計](docs/desktop-ui.md)を参照してください。

## VRMキャラクターの常駐表示

KDE/KWinなどのWayland layer-shell対応環境で利用できます。XWaylandへの切替はありません。

```sh
just fetch-assets  # 固定コミット・SHA-256を検証して開発用VRM・VRMAをキャッシュへ取得
just run-desktop  # ene-avatarもビルドし、会話画面とキャラクターを起動
```

Seed-san model by VirtualCast, Inc. — [取得元](https://github.com/vrm-c/vrm-specification/tree/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san)／[VRM Public License 1.0](https://vrm.dev/licenses/1.0/)。モデル内のライセンス設定に従い、クレジットを表示します。モデルはGitへ含めず、`$XDG_CACHE_HOME/ene/models/Seed-san.vrm`（既定`~/.cache/ene/models/Seed-san.vrm`）に置きます。設定の「VRMを選ぶ」で手持ちの埋め込み形式のVRM 1.0を選択できます。ファイル選択はOS標準ダイアログを使います。Linuxでは`xdg-desktop-portal`とデスクトップ用バックエンド（KDEでは`xdg-desktop-portal-kde`）が必要です。キャンセル時は設定を変更せず、ポータルの失敗は設定画面に表示します。パス欄への直接入力も利用できます。

輪郭内のクリックは会話表示、ドラッグは配置変更です。透明部分はクリック透過し、キーボードフォーカスと作業領域を占有しません。初期値30fps、設定で60fpsへ変更できます。大きさは静止時の全身を基準に、100%で画面の高さの約40%、200%で約80%とします。設定で50〜200%を直接入力し、「保存」で反映・保存できます。ドラッグは画面の端を越えて移動でき、負の配置も保存します。見失ったときは設定またはトレイの「キャラクターを画面中央へ戻す」で再表示・配置できます。モデル・モーション・サイズ・fps・配置の変更とトレイでの表示切替は再起動不要です。非表示中は描画・アニメーション更新を止め、GPUとモデルをメモリに保持して再表示時に再開します。出力変更では必要なWaylandウィンドウだけを更新します。モデルの読込失敗も設定から選び直せます。「再起動」は描画エラー時の復旧操作です。

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
