# ene

デスクトップに常駐するキャラクターとの対話から、PC内の作業を任せるAIコンパニオンです。
[構想](docs/product/vision.md)、[MVP](docs/product/mvp.md)、[アーキテクチャ](docs/design/architecture.md)、
[文書案内](docs/README.md)に方針をまとめています。

## 起動と動作確認

LinuxのWaylandセッションで、Slintの単一ウィンドウ、表示設定、トレイ、VRM 1.0とVRMAを利用できます。
デモは日本語・改行・絵文字の入力に固定の仮応答を返し、外部へ送信しません。
実ホストとの対話はConnect v1で逐次表示し、会話と設定をSQLiteへ保存します。
外観は即時反映し、変更から300ms後と終了時に端末設定を原子的に保存します。
不正なサイズ入力は他の設定・配置の保存を妨げません。保存失敗は画面から再試行できます。

```sh
nix develop
just verify
just test
just fetch-assets
just run-demo
# 実ホストへ接続する場合
just run-desktop
```

Nixを使わない場合は `rust-toolchain.toml` のnightlyとClang・LLDを用意します。
Debian系では `pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers fonts-noto-cjk bubblewrap` も必要です。
開発用サンプルは固定URL・SHA-256・出典・ライセンスを [取得スクリプト](scripts/fetch-assets.py) で管理し、キャッシュへ保存します。
Slint等のライブラリ通知は [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) を参照してください。

非公開の `XDG_RUNTIME_DIR` を用意してください。起動画面の「起動」でローカルホストを起動し、
「接続」で利用します。既存ホストは接続情報から検出し、アプリ終了でホストを停止しません。
独立起動は `just run-host` です。接続情報は `$XDG_RUNTIME_DIR/ene-host/connection.json`、
DBは `$XDG_STATE_HOME/ene/host.sqlite3`（既定 `~/.local/state/ene/host.sqlite3`）です。
`ENE_HOST_STATE_DIR` で保存先を変更できます。systemd用の [テンプレート](packaging/systemd/ene-host.service) もあります。

## ホスト設定と対話

設定の「コンパニオン」「AIサービス」「送信許可」を順に設定します。
組込OpenAIは公式 `https://api.openai.com/v1` のResponses APIを使います。
接続先とモデルを保存し、APIキーを登録します。ホストの `OPENAI_API_KEY` を使う操作もあります。
環境変数のキーは起動中だけ保持し、手入力のキーはOSのSecret Serviceへ保存します。
ストアが使えない場合は起動中だけ保持する状態を画面へ表示します。
キーをクライアント設定・DB・取得API・ログへ保存しません。

会話送信と疎通確認・モデル情報の送受信は別々に許可します。
「モデル一覧を取得」で選択し、許可後の「疎通確認」で実モデルの逐次応答を確認できます。
会話は生成途中から表示し、再接続とクライアント再起動後の履歴取得に対応します。
受付が不明な操作は同じ入力とrequest keyで確認し、別ホストへ再送しません。
許可・資格情報を解除すると生成を中断して保存します。再起動時に未完了の生成を自動再実行しません。

## パッケージと通信

| パッケージ | 役割 |
| --- | --- |
| `apps/ene-desktop` | 共通UI、ホスト管理・対話、端末設定と描画プロセスの管理 |
| `apps/ene-avatar` | ID付きの複数Waylandウィンドウ、VRM/VRMA、GPU画像と入力領域 |
| `apps/ene-host` | 認証付きConnectサービス、SQLite、プロバイダー、端末・音声セッション |
| `crates/ene-ipc` | 描画チャネルとプロバイダーRTC・HTTP brokerのRemoc契約 |
| `crates/ene-python` | PyO3製Python provider SDK、asyncioとRemocの接続 |
| `crates/ene-api-types` | Protobuf生成契約、保存用Serde型、RPC変換、容量と通知同期規則 |
| `crates/ene-api-client` | Connect操作、snapshot・通知の同期、設定保存と端末・音声のSDK |
| `crates/ene-core` | 会話・設定・認可・取消・保存の処理手順とport |
| `crates/ene-outbound` | 認可済み送信先へのHTTP、資格情報付与、本文・SSE容量制限 |

公開APIの正本は [`host.proto`](crates/ene-api-types/proto/host.proto) です。
Cargoが `ene.host.v1.HostService` と `DeviceService` を生成します。
SDKは `/rpc` のConnect v1と初期snapshot付き通知ストリームを利用します。
旧REST・OpenAPI・旧通知WebSocketは提供しません。音声は生成AudioFrameの `/audio` WebSocketです。
利用方法は [Connect契約](docs/design/connect-rpc.md)、機能追加時の不変条件は
[共通境界](docs/design/framework-boundaries.md) を参照してください。

描画プロセスは空一覧から複数ウィンドウを追加・削除できます。UIは1体を操作します。
非表示は削除、再表示は保持した設定と最新配置で再追加します。
個別設定は左上基準の配置を上書きせず、FPSは全体のRendererConfigで指定します。
GPU画像とその入力領域は同じWayland commitで提示します。

## プロバイダープラグイン

[OpenRouterプラグイン](providers/openrouter/README.md) をRustで提供します。
プラグインの標準入出力は`ene-ipc::provider`のRemoc接続です。モデル一覧と生成はRTC、HTTP応答と生成テキストはremote MPSCで渡します。
HTTPと資格情報はホストのbrokerが扱い、bubblewrapで隔離します。
OpenRouterの取得と生成はOpenAI配信・フォールバックなしにそろえます。
組込OpenAIの未知のモデル情報は公開models.devカタログで補完し、キーや会話は送りません。

`$XDG_DATA_HOME/ene/providers/`（既定 `~/.local/share/ene/providers/`、`ENE_PROVIDER_DIR` で変更可能）へ配置します。
発見だけでは実行せず、設定から再走査・有効化します。有効化は現在のホスト起動中に適用されます。
実装・manifest・接続条件・送信先の変更後は送信許可を設定し直してください。

## 検証と実装範囲

`just verify` は整形とClippy、`just test` は既存の回帰テストを実行します。
SDK・ホストの結合試験は実Connect通信とfixtureで動作を確認でき、実APIキーは不要です。
デモで送信・設定変更・表示オフ／再表示・中央配置・終了後の復元を確認してください。
IMEの候補操作・実機のドラッグ・ファイル選択ポータルはWayland実機で確認してください。
Webクライアント、本番STT/TTS、ファイル作業、タスク・記憶・スケジュールは未実装です。
音声WSのセッション制御と端末・表示leaseの契約はありますが、表示権移動UIは後続の実装です。

## PythonプロバイダーSDK

Python製の隔離プラグインは、[ene-provider SDK](crates/ene-python/README.md)を使います。
PyO3拡張内のRustがRemocで通信し、Pythonからはasyncio経由でホストのHTTP brokerを呼び出せます。
モデル一覧はRPCの戻り値、生成テキストはremote MPSC、終了理由はRPCの戻り値です。

```sh
cargo test --locked -p ene-python --all-features
```

テストは実際のPython子プロセスとRustのbrokerを接続し、u64・Unicode・バイナリ本文、大きな要求、エラーとキャンセルを確認します。

隔離したPythonプラグインの結合試験は、OSのPythonとbubblewrap、OSのライブラリで動くSDK拡張を用意して実行します。Nixの動的ライブラリは隔離環境に公開しません。

```sh
cargo build --locked -p ene-python --features extension-module
ENE_PROVIDER_EXTENSION=/absolute/path/ene_provider.so cargo test --locked -p ene-host request_errors_do_not_disable_plugin -- --ignored
```
