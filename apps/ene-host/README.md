# ene-host

> このREADMEは現行実装と起動方法を説明します。今後のRig中心のAlpha設計は[アーキテクチャ](../../docs/architecture.md)と[Alpha計画](../../docs/alpha.md)を参照してください。設計文書の追加では依存・API・実装は更新していません。

ホストの会話・設定・権限・生成処理、Connect API、SQLite保存、資格情報管理、プラグイン実行を持つパッケージです。`src/lib.rs` に実装をまとめ、`src/main.rs` から `ene_host::run()` を呼び出します。

## 起動と接続

```sh
nix develop
just run-host
```

ExtismのHost SDKをホストプロセス内に組み込んで実行します。専用runner、自己再起動、bubblewrapは不要です。WASIを無効にし、プラグインの通信や状態変更は認可済みのホスト関数を経由させます。

ホストは `127.0.0.1` の空きポートで待ち受け、Linuxでは `$XDG_RUNTIME_DIR/ene-host/connection.json` に `endpoint` と `token` を書きます。ディレクトリは0700、ファイルは0600です。トークンは起動ごとに更新し、終了時に接続ファイルを削除します。同じランタイムディレクトリでの二重起動は拒否します。

クライアントは `ene_rpc::connect(endpoint, token)` で接続し、生成された `HostServiceClient`、`DeviceServiceClient`、`PluginServiceClient` を利用します。音声の双方向通信は `/audio` のWebSocketです。通常のRPC・通知、デバイス認可・操作、プラグイン管理はConnectサービスで扱います。

| 環境変数 | 用途 |
| --- | --- |
| `ENE_HOST_STATE_DIR` | SQLite保存先。Linuxの既定値はXDG stateディレクトリ内の `ene` |
| `ENE_HOST_SESSION_CREDENTIALS` | 設定するとAPIキーをセッション中だけ保持 |
| `ENE_PLUGIN_DIR` | プラグインの検索ディレクトリ。既定値はアプリのdataディレクトリ内の `plugins` |
| `ENE_WEB_ORIGIN` | ブラウザーセッションで許可するorigin。省略時はホスト自身のorigin |

初期コンパニオンと空の接続設定をSQLiteへ作成します。生成するには接続・資格情報・モデル割当・データ送信許可をAPIで登録します。組み込みプロバイダーはOpenAI Responses APIです。OpenRouter用Wasmは `examples/plugins/openrouter-wasm` をビルドし、manifestと `provider.wasm` をプラグインディレクトリのパッケージに配置します。資格情報はホスト側で付与し、ゲストには渡しません。

SQLiteには設定・会話・操作の受付記録・プラグイン状態を保存します。OSの資格情報ストアへのアクセス失敗を永続保存成功として扱いません。停止時は新しい処理を受け付けず、実行中の生成・プラグイン・通知を終了し、保存処理を待ちます。

プラグインの変更・呼び出しとゲストからの状態保存は、既存の `MutationExecutor` が所有します。RPCの切断やゲストのキャンセル後も、受理済みの状態保存は完了まで継続し、保存障害はホスト側に通知します。ホスト終了時は受付を閉じ、プラグインへ停止を通知してから、受理済み処理の保存完了を待ちます。

HarnessはACP v1相当のセッションをbindingごとに1件保持し、`running` / `waiting_for_input` / `completed` / `failed` / `cancelled`を保存します。`session/update`相当のイベントは受信ごとに永続化し、長期のターンでも進捗を失いません。`session/request_permission`相当の相談ではターンを中断し、`Respond`でゲストを再開します。Cancelはゲストを再開せず終端状態を保存し、再送時は保存だけを再試行します。再バインド・無効化・再読み込み・終了では`plugin_stopped`、起動時は`host_interrupted`として未完了タスクを終了します。旧版の保存形式は再開せず置換し、復旧保存に失敗した場合は起動を成功扱いにしません。長いターンでは `PluginResult.events` は最新256件に限り、正本はジャーナルです。ハーネスは権限相談の境界を除き1ターンが1回の呼び出しになるため、インスタンスとサービス待ちの実行上限を30分に延ばしています（CPUはfuelで制限）。

### ワークスペースとfsツール

`PluginBinding.workspace_root` に絶対パスを指定すると、`tools` を許可したハーネスが `fs.read_text_file` / `fs.write_text_file` を呼べます。ルートはバインド時にcanonical化し、ファイルシステムのルートは拒否します。パスはルート相対のみで、絶対パス・`..`・`.`・シンボリックリンク・通常ファイル以外を拒否し、読み書きは256KiB、書き込みはatomic rename、親ディレクトリは自動作成しません。端末実行は別途オプトインのcapabilityとして扱います。

### 端末（明示オプトイン）

ACPバインディングの`terminal`ポリシーを有効にした場合だけ、ACPの`terminal/create`・`output`・`wait_for_exit`・`kill`・`release`を処理します。コマンドは`allowed_commands`（実行ファイル名または絶対パスの完全一致）に限定し、シェルを介さず、`workspace_root`配下の`cwd`だけを許可します。直前の`session/request_permission`でユーザーがallowを選んだ直後の1コマンドだけを受け付けます。出力は`max_output_bytes`で打ち切り、`max_seconds`で強制終了します。**サンドボックスはなく、ホストのOSユーザー権限で実行されます**（コンテナ・seccomp・ネットワーク制限なし）。信頼できるエージェントとコマンドだけを許可してください。

### ハーネス状態の取得と通知

`PluginService.GetHarness` はバインディングIDから保存済みのタスクジャーナル（`running` / `waiting_for_input` / `completed` / `failed` / `cancelled`、`pending`、`tool_calls`、`plan`、`report`、`failure`、`stop_reason`）を返します。ハーネスの呼び出しが成功するたびに `Notification.harness_task`（`binding_id`、`companion_id`、`task_id`、`status`、`stop_reason`）を配信します。これは再取得のヒントであり、正本は `GetHarness` のジャーナルです。失敗した呼び出しはRPCエラーになるため、状態は `GetHarness` で確認します。これはTask APIの最小読み取り面で、コンパニオン単位の一覧・質問・報告APIは未実装です。

### タスク推論（`models` サービス）

`models` を宣言・許可したハーネスは、ホスト経由でタスク用のモデルを呼べます。ホストはコンパニオンの `model_assignments.task` とホスト設定の接続・モデルを使い、`purpose: "task"`・`data_kinds: ["task"]` のデータ送信Grantを検証します。ゲストが指定した endpoint・options・model・tools は採用せず、`max_tokens` はホスト上限（8192）に丸め、出力は `task` 操作の上限（256,000バイト）で打ち切ります。タスク割当が未設定なら `model_unset`、Grantがなければ `permission_required` を返します。推論はキャンセル可能で、ホスト終了時も実行中の送信を止めます。

### 外部ACPエージェント

`target: "acp"` の entrypoint を持つパッケージは、`command` の子プロセスをACPエージェントとして起動します。シェルを介さず、環境変数は許可リスト（`PATH`・`HOME`・`USERPROFILE`・`LANG`・`LC_ALL`・`TERM`・`SYSTEMROOT`）だけを渡し、ホストの資格情報は渡しません。`workspace_root` が必須で、fs client capability はそのルートに限定し、terminal はバインディングのポリシーが有効な場合だけ advertise します。`initialize`・`session/new`・`session/prompt`・`session/update`・`session/request_permission` を共通ジャーナルへ写像し、`GetHarness` と通知はWasmハーネスと共通です。Cancel・無効化・再バインド・終了ではプロセスを停止して終端化し、initialize等の失敗は `acp_failed` としてタスクを終了します。旧ホストはACP entrypointを無効なmanifestとして拒否します。

LLM実行は無効化・生成キャンセル・実行失敗で出力転送も終了し、受信が止まっていても実行管理枠を解放します。完了イベントなしで閉じたストリームは失敗として扱います。

プラグイン契約はv4です。Harnessの入力・イベントはACP v1の語彙（`Start` / `Respond` / `Cancel`、`Update` / `RequestPermission` / `Stop`）に変わりました。Characterのバインドには `device_id` と `companion_id` を明示し、呼び出しと返された操作の対象が一致することを検証します。旧バインドにはデバイスを自動補完せず、再設定が必要です。ウィンドウ監視はデスクトップ専用のため、ホストでは実行しません。

音声通信の認可・セッション管理は実装済みですが、本番用の音声認識・合成エンジンは接続していません。エンジンがない状態では音声利用可能と通知しません。デスクトップUI、ファイル操作ワーカー、完成版の自律タスクループはこのホストの実装完了範囲に含みません。

## パッケージの境界

旧 `ene-core` と `ene-outbound` をこのパッケージへ統合しました。旧 core はホスト固有の状態と処理を持ち、旧 outbound は core の通信を Rig に接続する薄いラッパーでした。独立した利用者がなく、認可と通信も一体で変更するため、クレートを分ける必要はありません。

Rig の `HttpClientExt` はホストの `AuthorizedTransport` に直接実装します。クレート間の制約を満たすためだけのラッパーと、その委譲メソッドは不要になりました。ホストから独立した利用者やビルド要件が生まれた時点で、実際に共有する責務を切り出します。

## 内部構成

| モジュール | 責務 |
| --- | --- |
| `application` | メッセージ受付、設定変更、生成の開始・停止、保存と通知の調整 |
| `domain` | ホストの状態、入力検証、許可と実行可否のルール |
| `ports` | 永続化、資格情報ストア、プラグイン管理のインターフェース |
| `llm` | プロバイダー契約、生成入力、権限付き実行、HTTP通信、Rigとの接続 |
| `api` | 認証、Connectサービス、ブラウザーセッション、通知 |
| `infrastructure` | SQLite、資格情報ストア、OpenAI・プラグイン接続、接続情報の公開 |
| `devices` / `audio` | デバイス操作と音声セッションの認可・寿命管理 |
| `bootstrap` / `server` | 依存の組み立て、起動、シグナルによる停止 |

`llm::invocation` は非公開モジュールです。認証情報を持つ `Invocation` と、その子モジュールの通信実装を同じ境界に置きます。APIキーをリクエストに付与するメソッドは非公開のまま、送信先検証・認証情報の付与・送信を一続きで実行します。送信前と受信中の権限確認、リダイレクト拒否、サイズ制限もここで適用します。

Rig との接続は通信実装の子モジュールに置き、ストリーミングイベントのサイズ制限もそこで扱います。保存先などを差し替えるインターフェースは維持し、SQLや資格情報ストアの実装はアプリケーション処理へ埋め込みません。

テスト用の `Invocation` 生成は `cfg(test)` の内部補助コードです。認可済み実行を任意に生成するための公開 feature は提供しません。

## 検証

```sh
cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown
cargo test -p ene-host -p ene-platform -p ene-plugin-runtime --locked
cargo clippy -p ene-host -p ene-platform -p ene-plugin-runtime --all-targets --all-features --locked -- -D warnings
```

通常のホストテストに、実際のPDK製WasmとHTTPブローカーの結合検証を含みます。
別のWasm出力先を使う場合は `ENE_TEST_REFERENCE_WASM` / `ENE_TEST_PDK_WASM` で指定します。

会話状態・許可判定に加え、実Connectクライアント、SQLiteの再読み込み、組み込みExtism、ローカルHTTPサーバーを使って認証・取り消し・停止・サイズ制限を検証します。Linuxの実ホストプロセスのテストでは起動、二重起動拒否、不正トークン拒否、変更の保存、購読中のSIGTERM、再起動後の復元を確認します。有料の外部APIや実機音声エンジンはテストから呼びません。

旧Luauプラグインは非対応です。Rust製プラグインは `ene-plugin-sdk` への依存を `ene-plugin` に変更します。共通契約はバージョン4です。ハーネスはACP v1の入力・イベントへ移行し、`contract_version` を4に更新してください。旧v3の保存ジャーナルは起動時に`host_interrupted`として終端化され、再開しません。revisionが変わるため、再有効化とバインディングの再設定が必要です。保存データは削除しません。旧 `ENE_PLUGIN_RUNNER_BIN` / `ENE_PLUGIN_CACHE_DIR` は不要で、旧コードキャッシュを実行時に読みません。
