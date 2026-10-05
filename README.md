# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。[構想](docs/product/vision.md)、[MVP](docs/product/mvp.md)、[アーキテクチャ](docs/design/architecture.md)に実装方針をまとめています。文書の一覧と配置・更新ルールは[ドキュメント案内](docs/README.md)を参照してください。

## ワークスペース

機能追加は[共通基盤とコード生成テンプレート](docs/design/feature-framework.md)を入口にします。`just new-feature <名前> <name|mind>` で動作する設定変更のDTO・コア処理・認証付きAPI・型付きクライアント・回帰テストを生成できます。保存・通知・取消し・終了待ちはホストの共通基盤を利用します。

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

現在の実装はSlint 1.18.1の単一ウィンドウです。会話を主画面とし、上部の設定アイコンから設定を開きます。起動画面と「ホスト管理」で、ホストの稼働・アプリの接続・会話の準備を別々に確認できます。日本語の複数行入力、Enter／ボタン送信、実AIの逐次応答、表示設定、ksniのシステムトレイを備えています。デモの会話は起動中のみ保持し、ホスト接続時の会話はSQLiteへ保存します。表示設定・VRM・配置は端末側に保存します。別プロセスのVRM常駐表示とVRMAモーション再生も実装しています。ホストの認証・初期化・履歴・通知購読も実装しています。OpenAI公式APIとの逐次テキスト対話・再接続・設定・送信許可も利用できます。ファイル作業と音声は未実装です。

LinuxのWaylandセッションで起動してください。Nixを使わないDebian系の環境では、Rustに加えて次の依存が必要です（ほかのOSの対応は今回の対象外です）。

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers fonts-noto-cjk
just verify
just run-desktop
# justがない場合
cargo build --locked -p ene-avatar -p ene-host
cargo run --locked -p ene-desktop
```

通常の`just run-desktop`では右下の「＋」でこのPCのホストを追加するか、一覧の「接続」を選びます。ホストが未起動ならアプリから起動し、稼働中ならそのホストに接続して実際のAIと話します。接続・モデル・資格情報・送信許可が揃うまで送信を無効にします。`just run-demo`（`--demo`）は固定の仮応答を試す専用モードです。APIキーを必要とせず、外部へ会話を送信しません。デモの設定画面から「実際のAIを設定」で接続先の選択へ進めます。デモの履歴は引き継がず、入力中の文章は保持します。Enterで送信、Shift+Enterで改行します。IMEの変換・確定中は送信しません。送信ボタンも利用できます。トレイ登録済みなら×は非表示、トレイクリックで会話を開きます。トレイが使えない場合は×で終了処理に進みます。トレイの「eneを終了」でも終了できます。未保存の設定・未送信の文章・処理中の要求がある場合は確認を表示します。非表示中にトレイが消えた場合はウィンドウを復帰します。

ネイティブIMEの候補位置・確定・取消とKDE/KWinでの操作確認は実機で行ってください。

ホストはデスクトップの右下の「＋」またはカードの「起動して接続」から起動できます。`just run-desktop`と`just run-demo`は、起動用の`ene-host`もビルドします。既定ではデスクトップと同じフォルダーの`ene-host`を使い、`ENE_HOST_BIN`で実行ファイルを指定できます。ホストはアプリから独立して稼働し、接続の中止・解除やアプリの終了では停止しません。起動の失敗は画面へ表示し、再試行できます。稼働中にホストが停止した場合、接続は自動で再試行しますが、ホストを起動し直す操作は「再試行」で明示的に行います。端末から単独で起動する場合は`just run-host`を使います。現在のユーザーが所有する非公開の`XDG_RUNTIME_DIR`が必要です。接続先と起動ごとのBearerトークンは`$XDG_RUNTIME_DIR/ene-host/connection.json`、DBは`$XDG_STATE_HOME/ene/host.sqlite3`（既定`~/.local/state/ene/host.sqlite3`）へ保存します。systemd用テンプレートは[ene-host.service](packaging/systemd/ene-host.service)です。

## ホストとのテキスト対話

`just run-desktop`を起動し、右下の「＋」でこのPCのホストを追加するか、一覧の「接続」を選びます。起動画面から接続すると、未設定の場合は必要なAI設定へ進みます。サービスとAPIキーを入力し、画面下の「保存」で反映します。外観・コンパニオン・AIサービス・送信許可は、同じ「元に戻す」「保存」で操作します。元に戻す対象は開いている画面の変更だけです。接続先URLやプラグインの再走査は「接続の詳細」を開くと操作できます。モデル未選択でも先に接続先を保存できます。組込OpenAIは公式の`https://api.openai.com/v1`へ接続し、Responses APIで逐次応答を取得します。会話用・作業用の確認モデルは`gpt-6-luna`です。OpenAIのAPIキーは接続先・モデルと同じ「保存」で登録します。ホストの`OPENAI_API_KEY`を使う選択やキーの解除も「保存」で反映します。環境変数由来のキーはホスト起動中だけ保持し、手入力のキーはOSのSecret Serviceへ保存します。ストアが使えなければ起動中だけ保持し、その状態を画面へ表示します。キーをクライアント設定・DB・取得API・ログへ保存しません。

「送信許可」で会話送信と疎通確認・モデル情報の送受信を別々に許可し、保存します。許可後に「疎通確認」で実モデルの逐次応答を確認できます。対話は生成途中から表示し、クライアント再起動後も履歴を取得します。「過去の会話を読み込む」でさらに遡れます。通知購読→スナップショット→連番差分の順に同期し、欠番・epoch変更・Unicodeスカラーoffset不一致では購読をやり直します。再接続は1・2・4・8・15秒です。「ホスト管理」にある「再試行」は同じ接続処理を起こし、受付が不明な発言は同じIdempotency-Keyで確認します。許可・資格情報の解除は生成を待たず新規送信を禁止し、返答を中断状態で保存します。ホスト再起動時の未完了生成・疎通確認は自動再実行しません。

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

以前の組込OpenRouter接続は自動変換しません。OpenAI用の接続・モデル割当・APIキー・送信許可を登録し直してください。OpenRouterを継続する場合は、以下の追加プラグインを有効化して接続設定と送信許可を登録し直します。

## プロバイダープラグイン

Linuxの`bubblewrap`がホストの`PATH`に必要です（Debian系では`sudo apt-get install bubblewrap`、Nixの開発シェルには同梱）。`$XDG_DATA_HOME/ene/providers/`（既定`~/.local/share/ene/providers/`、`ENE_PROVIDER_DIR`で変更可能）の各フォルダに`manifest.json`と実装を配置します。発見だけでは実行しません。設定のプロバイダー一覧で再走査し、選択して有効化すると隔離環境で初期化します。有効化は現在のホスト起動中に適用され、再起動後には再度有効化します。接続設定・モデル割当・許可はDBに保存します。

[OpenRouterプラグイン](providers/openrouter/)を用意しています。Rust製の実行ファイルで、通信型とNDJSON処理は`ene-protocol`を共有します。

```sh
# x86_64 Linux向けに静的リンクし、隔離環境へ動的ライブラリを追加せずに実行する。
rustup target add --toolchain 1.98.1 x86_64-unknown-linux-musl
rustup run 1.98.1 cargo build --release --locked --target x86_64-unknown-linux-musl -p ene-provider-openrouter
provider_dir="${ENE_PROVIDER_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/ene/providers}/openrouter"
install -d -m 700 "$provider_dir"
install -m 755 target/x86_64-unknown-linux-musl/release/ene-provider-openrouter "$provider_dir/provider"
install -m 600 providers/openrouter/manifest.json "$provider_dir/manifest.json"
```

他のCPU向けの配置は[プラグインのREADME](providers/openrouter/README.md)を参照してください。

有効化後は、モデル未選択のまま接続を保存し、資格情報・「疎通確認・モデル情報」の送信許可を登録してから「モデル一覧を取得」で選択・保存してください。このプラグインはOpenRouter専用で、OpenAI限定・フォールバックなしです。組込のOpenAI公式APIとは別の`openrouter`として登録されます。他サービスには、そのAPI形式と送信条件に合わせた別の実装を用意します。manifestへのモデル列挙は不要です。OpenRouterプラグインは`/models?providers=openai`からOpenAI配信のモデル一覧と上限・入力形式などを取得します。組込OpenAIの一覧APIが公開しないコンテキスト上限・入力／出力上限・形式はmodels.devで補完します。APIの値を優先し、完全一致で情報が見つからないモデルは不明として扱います。models.devへキーや会話は送りません。公開カタログを取得できない場合も公式APIから取得したモデル一覧を表示し、補完できない情報は不明とします。補完用の送信先が増えるため、既存の送信許可は設定し直してください。[モデル情報の契約](docs/design/api.md#モデル一覧とメタデータ)を参照してください。モデル一覧がない場合もモデルIDを直接入力できます。宣言した設定項目だけをUIで扱い、任意のUIコードは実行しません。

プラグインは隔離した別プロセスで起動し、直接のネットワーク・ホストDB・資格情報・ホームへのアクセスを公開しません。資格情報はホストがHTTP brokerで付与します。初期認証方式は認証なし・Bearer・固定ヘッダー・クエリです。OAuth・署名方式は未対応です。APIバージョンなどの非秘密の固定ヘッダーはmanifestの`headers`で宣言します。外部通信は宣言したHTTPS送信先、または明示許可したHTTPループバックに限定し、リダイレクトを拒否します。OpenRouter仲介時の実処理先とフォールバック禁止はbrokerも強制します。隔離失敗時の無制限起動、別実装への自動切替、生成の自動再送はありません。

会話の生成要求はJSONで256KiB以内に収まる最近の会話ターンを使います。保存済みの履歴は保持し、今回の入力自体が上限を超えた場合は短くするよう案内します。

通信はversion 1のNDJSON（改行込み1MiB、待ち行列16件、1生成あたりHTTPは直列・最大8要求）です。アバターIPCの64KiB制限とは分離しています。生成・モデル取得ごとにプロセスを起動し、終了時には停止を要求して3秒後に強制終了・回収します。取消・許可や資格情報の解除では通信を中断し、遅着応答を破棄します。実装・manifest・接続条件が変わった場合には再走査・有効化・送信許可の再設定が必要です。実装revisionは相対パス・種類・権限・長さ・内容を区切ってハッシュ化します。この形式変更でも既存のプラグイン許可は再設定が必要です。通信型とmanifestの契約は[`ene-protocol::provider`](crates/ene-protocol/src/provider.rs)に定義しています。

## Nixで開発する

現在の開発シェルは`x86_64-linux`向けです。Nixのflakesとnix-commandを有効にした環境で、リポジトリのルートから実行します。

```sh
nix develop
just verify
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
cargo run --locked -p ene-desktop
```

`just verify`は整形とClippy（テストを含む型検査）を実行します。テストは対象の挙動を変更したときに`just test`で実行し、変更していない合格済みのテストを繰り返す必要はありません。成果物が必要なときは`just build`を実行します。
初期の結合確認用テスト、疑似プロバイダー、プロセス起動用フィクスチャと画像出力は削除しました。残したテストは認証・送信許可・保存の整合性・通信境界に絞っています。

## 開発コマンド

| コマンド | 処理 |
| --- | --- |
| `just` | 利用可能なレシピの一覧 |
| `just fmt` | ワークスペース全体のRustコードを整形 |
| `just fmt-check` | 整形済みかを確認 |
| `just check` | 全クレート・ターゲット・機能を型検査 |
| `just clippy` | 全クレート・ターゲット・機能を検査し、警告をエラーにする |
| `just test` | 通常のワークスペースのテストを実行 |
| `just build` | 全クレート・ターゲット・機能をビルド |
| `just verify` | 整形確認とClippy（テストを含む型検査）を実行 |
| `just run-host` | 認証付きループバックホストを起動 |
| `just run-desktop` | ローカルホストの起動・接続と実AI設定を利用する会話画面を起動 |
| `just run-demo` | 外部送信のない仮応答デモを起動 |
| `just fetch-assets` | 固定コミットの公式Seed-sanとVRMA全4本をチェックサム付きで取得（`vrm` / `vrma`で種類を選択、個別名で1本を取得） |
| `just run-avatar` | キャラクター描画を単独起動 |

## 対話・設定UI

通常ウィンドウは1つで、avatarは別の透明な常駐ウィンドウです。起動時に会話の保存先を選び、開始後は会話と入力を中心に表示します。上部右側にホストと設定のアイコンを置きます。ホストアイコンは状態表示と管理への入口を兼ね、ホバーまたはキーボードフォーカスで稼働状態とアプリの接続状態を確認できます。「ホスト管理」では接続を解除した後も状態を自動更新し、起動・接続・再試行と、設定不足の解消へ進めます。接続先URLや直近のエラーはカードの詳細アイコンから確認できます。設定を開いても会話の下書き・履歴・閲覧位置を維持し、「会話に戻る」で復帰します。設定済みなら接続後に会話へ進み、未設定なら必要なAI設定へ案内します。デモの案内は会話画面の上部に表示します。初期サイズ900×760、最小420×480とし、幅と文字サイズに応じて配置を変更します。外観・モーション設定とAI設定は、どの画面幅でも同じ順序の縦1列に並べます。大画面では設定の幅を抑え、外側の余白を広げます。会話本文の読み幅と長文入力欄の高さには上限を設け、フルHD・WQHD・ウルトラワイド・4Kと縦長・横長のウィンドウを描画テストで確認します。

設定の「外観」では外観、デスクトップ表示、ローカルVRM・VRMAを変更できます。「保存」で永続化してから反映し、設定画面に留まります。「元に戻す」で最新の保存済み値へ戻ります。フォントはシステムの一覧から選び、日本語・英数字の見本を確認できます。表示切替・中央へ戻す・描画の再起動・モーションの再生と繰り返しは即時操作で、フォームの編集中の変更を保存しません。未保存の設定を残したまま終了する場合は、破棄するか画面へ戻るか選択します。

「コンパニオン」で名前と口調、「AIサービス」でプロバイダー・モデル・資格情報、「送信許可」で用途ごとの送信許可を管理します。音声・フォルダ作業はまだ利用できません。設定の所有者と将来の音声操作については[デスクトップUI設計](docs/design/desktop-ui.md)を参照してください。

ホストに複数のコンパニオンがある場合だけ、広い会話画面では左側に相手の一覧、狭い画面では上部に選択欄を表示します。ホスト設定内にも相手の選択欄を表示します。会話履歴・入力中の文章・モデルの割り当て・送信許可を相手ごとに分け、選択は一覧順ではなくIDで維持します。サービスとAPIキーはホスト内で共通です。未保存のホスト設定・APIキー、処理中の要求がある間は相手を切り替えず、編集を保存するか元に戻してから切り替えます。現在のデスクトップ描画は1体で、複数体の作成や同時表示は未実装です。

## VRMキャラクターの常駐表示

KDE/KWinなどのWayland layer-shell対応環境で利用できます。XWaylandへの切替はありません。

```sh
just fetch-assets  # 固定コミット・SHA-256を検証して開発用VRM・VRMAをキャッシュへ取得
just run-desktop  # ene-avatarもビルドし、会話画面とキャラクターを起動
```

Seed-san model by VirtualCast, Inc. — [取得元](https://github.com/vrm-c/vrm-specification/tree/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san)／[VRM Public License 1.0](https://vrm.dev/licenses/1.0/)。モデル内のライセンス設定に従い、クレジットを表示します。モデルはGitへ含めず、`$XDG_CACHE_HOME/ene/models/Seed-san.vrm`（既定`~/.cache/ene/models/Seed-san.vrm`）に置きます。設定の「VRMを選ぶ」で手持ちの埋め込み形式のVRM 1.0を選択できます。ファイル選択はOS標準ダイアログを使います。Linuxでは`xdg-desktop-portal`とデスクトップ用バックエンド（KDEでは`xdg-desktop-portal-kde`）が必要です。キャンセル時は設定を変更せず、ポータルの失敗は設定画面に表示します。パス欄への直接入力も利用できます。

輪郭内のクリックは会話表示、ドラッグは配置変更です。透明部分はクリック透過し、キーボードフォーカスと作業領域を占有しません。初期値30fps、設定で60fpsへ変更できます。大きさは静止時の全身を基準に、100%で画面の高さの約40%、200%で約80%とします。設定で50〜200%を直接入力し、「保存」で反映・保存できます。ドラッグは画面の端を越えて移動でき、負の配置も保存します。見失ったときは設定またはトレイの「キャラクター → 画面中央に戻す」で再表示・配置できます。モデル・モーション・サイズ・fps・配置の変更とトレイでの表示切替は再起動不要です。非表示中は描画・アニメーション更新を止め、GPUとモデルをメモリに保持して再表示時に再開します。出力変更では必要なWaylandウィンドウだけを更新します。モデルの読込失敗も設定から選び直せます。「再起動」は描画エラー時の復旧操作です。

`just run-desktop`・`just run-demo`・`just run-avatar`はdebugビルドで起動します。開発中のコードは未最適化のまま、Bevyなどの外部依存ライブラリだけdevプロファイルで最適化します。表示画像はGPUからWaylandへ渡し、入力領域もGPUで最終画像から生成します。通常動作で画像のCPU読み戻しは行いません。[描画設計](docs/design/avatar-gpu-rendering.md)を参照してください。

テーマ（システム・ライト・ダーク）・フォント・文字サイズ・VRM・大きさ・表示・fps・出力名・位置は`$XDG_CONFIG_HOME/ene/client.json`（既定`~/.config/ene/client.json`）へ保存します。終了時は描画プロセスへ停止を要求し、3秒以内に応答しなければ強制終了して回収します。`ENE_AVATAR_BIN`で描画バイナリの場所を指定できます。ハードウェアGPUのない試験環境だけで`ENE_AVATAR_CPU_GPU=1`を指定するとWGPUのソフトウェアアダプターを使用します。

`just run-avatar`で描画を単独起動できます。通常の静的検査は`just verify`で実行します。

初期設定では`just fetch-assets`で取得したthree-vrmのサンプルVRMAを自動で繰り返し再生します。保存済みのモーション設定（静止姿勢を含む）は優先します。VRMA 1.0は設定の「VRMAを選ぶ」から読み込み、再生・一時停止・繰り返しを設定できます。「モーションを外す」で静止姿勢へ戻す下書きを作り、保存して反映します。再生・一時停止と「繰り返す／1回だけ」は選択欄に状態を表示し、変更をすぐに反映・保存します。実際の再生状態・読み込み・失敗も文字で表示します。モーションの切替・解除では描画環境とモデルを維持します。足の接地を崩さないよう、腰の水平・上下移動と回転を保持し、画面上の配置とカメラの基準は固定します。表示フレームとクリック領域は同時に更新します。

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

UIアイコンには`lucide-slint`をMITライセンスで使用しています。本体とアイコンの必要な表記は[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)にまとめ、デスクトップのバイナリ配布時にもこのファイルを同梱します。

```sh
cargo run --locked -p ene-avatar -- --standalone --model /path/to/avatar.vrm --motion /path/to/motion.vrma
# 一度だけ再生する場合は --no-loop を追加
```
