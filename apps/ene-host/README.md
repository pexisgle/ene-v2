# ene-host

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

プラグイン契約はv2です。Characterのバインドには `device_id` と `companion_id` を明示し、呼び出しと返された操作の対象が一致することを検証します。旧バインドにはデバイスを自動補完せず、再設定が必要です。ウィンドウ監視はデスクトップ専用のため、ホストでは実行しません。

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
cargo build --release --locked -p ene-reference-wasm --target wasm32-unknown-unknown
cargo test -p ene-host -p ene-platform -p ene-plugin-runtime --locked
cargo clippy -p ene-host -p ene-platform -p ene-plugin-runtime --all-targets --all-features --locked -- -D warnings
```

実際のRust製Wasmプラグインも検証する場合は、次を追加で実行します。

```sh
cargo build --release --locked -p ene-openrouter-wasm --target wasm32-unknown-unknown
ENE_TEST_PDK_WASM="$PWD/target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm" cargo test --locked -p ene-plugin-runtime --test pdk -- --ignored
```

会話状態・許可判定に加え、実Connectクライアント、SQLiteの再読み込み、組み込みExtism、ローカルHTTPサーバーを使って認証・取り消し・停止・サイズ制限を検証します。Linuxの実ホストプロセスのテストでは起動、二重起動拒否、不正トークン拒否、変更の保存、購読中のSIGTERM、再起動後の復元を確認します。有料の外部APIや実機音声エンジンはテストから呼びません。

旧Luauプラグインは非対応です。Rust製プラグインは `ene-plugin-sdk` への依存を `ene-plugin` に変更します。共通契約はバージョン3です。manifestの `runtime` と `code_files` を削除し、`contract_version` を3に更新してください。操作・イベントのJSON形式とエクスポート関数名は維持しています。revisionが変わるため、再有効化とバインディングの再設定が必要です。保存データは削除しません。旧 `ENE_PLUGIN_RUNNER_BIN` / `ENE_PLUGIN_CACHE_DIR` は不要で、旧コードキャッシュを実行時に読みません。
