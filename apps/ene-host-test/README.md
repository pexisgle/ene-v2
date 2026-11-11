# ene-host-test

ene-host から ene 固有の機能（キャラクターの心・記憶、送信許可、端末境界）を除いた、技術検証用の最小ホストです。契約はこのパッケージ内の [chat.proto](proto/chat/v1/chat.proto) と [settings.proto](proto/settings/v1/settings.proto) で完結し、`ene-rpc` は共有しません。

外部ライブラリの使い方は、各クレートの公式ドキュメントと example の形に合わせています。以下は Cargo.lock で固定している版に対する対応です。アプリ固有の判定（受理レシート、承認、窓の刈り込み）はこのホストの設計で、ライブラリの例そのものではありません。

## 構成

| パス | 役割 |
| --- | --- |
| `server/mod.rs` | ループバック待受、`connection.json` の公開、停止 |
| `server/access.rs` | 0700/0600 のディレクトリとロック、発見ファイル |
| `server/rpc.rs` | 生成された Connect サービス、Owner Bearer |
| `client.rs` | 発見ファイルを読む認証付きクライアント |
| `host/mod.rs` | Rig App スレッド、コマンド送信、通知、復旧 |
| `host/kernel.rs` | Prepare / Fit / Gate / Review / SessionLog、ストリーム、通知、作業の報告 |
| `host/conversation.rs` | 設定、入力受理、履歴、スナップショット |
| `host/tools.rs` | ツールの登録とロール別カタログ、実行時ツール（`start_work`、状態更新、承認付き更新） |
| `host/model.rs` | OpenAI Responses への接続と送信制限 |
| `host/store.rs` | SQLite のドキュメント、レシート、設定、作業、質問 |
| `tests.rs`（`host/mod.rs` 内） | 受理・復旧・作業・承認・窓・ストリームの挙動テスト |

## 公式ドキュメントとの対応

### 契約の生成

| 実装 | 参照 |
| --- | --- |
| [build.rs](build.rs) の `connectrpc_build::Config::new().files(..).use_buf().include_file(..).compile()` | connectrpc-build 0.9 クレートドキュメントの "Example"（`build.rs` と `lib.rs`）と `Config::use_buf` |
| [lib.rs](src/lib.rs) の `connectrpc::include_generated!()` | 同上（"Example" の `lib.rs`） |
| [chat.proto](proto/chat/v1/chat.proto) / [settings.proto](proto/settings/v1/settings.proto) | protobuf3 の message / service。生成は上の build.rs |

### サーバーと RPC

| 実装 | 参照 |
| --- | --- |
| `axum::serve(listener, app).with_graceful_shutdown(..)` | axum 0.8 の `serve` / `with_graceful_shutdown` ドキュメント（`src/serve/mod.rs` の例） |
| `Router::nest_service` と `middleware::from_fn_with_state` | axum 0.8 の `Router::nest_service`、`middleware::from_fn_with_state` ドキュメント |
| サービスの実装（`impl ChatService for Services`、`ServiceRequest`、`Response::ok`） | connectrpc 0.9 README の "Defining a service" |
| `ChatServiceExt::register(..).into_axum_router()` | connectrpc 0.9 の生成コード（README の "With Axum (recommended)" と同じ Router 登録） |
| Bearer の比較（`subtle::ConstantTimeEq`） | subtle 2 の `ConstantTimeEq` ドキュメント |
| `Http2Connection::connect_plaintext(..).shared(..)` と `ClientConfig` | connectrpc 0.9 の `src/client/mod.rs` 冒頭のクライアント例 |

### ファイルとプロセス

| 実装 | 参照 |
| --- | --- |
| `PrivateDir::ensure` / `open_file_with` / `write_atomic_durable` / `remove_file` | private-fs 0.2 README の "Guarantees"、"Manage application state"、"Atomic writes" |
| `PrivateFile::as_filelike_view::<File>().try_lock()` | std の `File::try_lock`、io-lifetimes の `AsFilelike` |
| `ProjectDirs::from("", "", "ene-host-test")` | directories 6 の `ProjectDirs` ドキュメント |
| `#[tokio::main]` と `ctrlc::set_handler` | tokio の macro ドキュメント、ctrlc 3.5 クレートドキュメントの例 |
| `CancellationToken` | tokio-util の `CancellationToken` ドキュメント |

### Rig カーネル

| 実装 | 参照 |
| --- | --- |
| `App` + `AgentPlugin` + `SessionLogPlugin` + `SessionStore`/`open_session` | rig-ecs 0.46 README と `src/lib.rs` のクレートドキュメント、`session_log::open_session` |
| ヘッドレスループ（`TaskPoolPlugin`、`Wake`、`Time<Virtual>::set_max_delta`） | rig-ecs 0.46 の `tests/kernel_only.rs` |
| `AgentPass` の `PrepareSystems` / `Stage` / `ReviewSystems`、`WriteLogs` | rig-ecs 0.46 の `src/stage.rs` |
| `add_open_tool` と `ToolOptions`（`host/tools.rs`） | rig-ecs 0.46 の `src/tools.rs` |
| `WorldToolsExt::spawn_tool`（実行時に見つかったツールの登録） | rig-ecs 0.46 の `src/tools.rs` の `WorldToolsExt`（MCP サーバーの例がそのまま書かれています） |
| `ToolAccess::Only` と `deliver`/`DeliveryMode::Inform`（ツールのロールと会話への差し込み） | rig-ecs 0.46 の `src/agent.rs`、`src/inbox.rs` |
| `Deliver` / `DeliveryMode::{Steer, Inform}` | rig-ecs 0.46 の `src/inbox.rs` |
| `SystemPrompt` / `StopTurn` / `Conversation` | rig-ecs 0.46 の `src/agent.rs` |
| 窓の刈り込み（`fit`）: `HeuristicTokenCounter`、`TokenBudget`、`recent::view` | rig-memory 0.46 の `src/recent.rs` と `TokenCounter` ドキュメント |

### モデルと保存

| 実装 | 参照 |
| --- | --- |
| `OpenAIConfig::new(key).with_base_url(..).connect(http).responses(model)` | rig-core 0.46 の `providers/openai` クレートドキュメント（`OpenAI::from_env` / `responses` / `chat`） |
| `Catalog::builtin().resolve(..)` と `ModelSpec` | rig-core 0.46 の `catalog` ドキュメント |
| `ErasedHandler` + `ModelAdapter::<Completion>` | rig-core 0.46 の `serve::adapters` ドキュメント |
| `HttpClientExt` の 3 メソッド実装（送信先と許可の検査） | rig-http 0.46 の `src/http_client/mod.rs` |
| `ReqwestClient` | rig-reqwest 0.46 の `src/lib.rs` |
| `Connection` / `transaction` / `execute` / `query_row` / `OptionalExtension` | rusqlite 0.40 のドキュメント |
| `MockCompletionModel` / `MockStreamEvent`（テスト） | rig-core 0.46 の `test_utils` |

## 拡張点

ene 固有の機能を足すときに触る場所は 3 つに閉じています。

| 足したいもの | 追加する場所 | 仕組み |
| --- | --- | --- |
| ツールを増やす | `host/tools.rs` に `open_tool(..)` を 1 回（ホスト束縛） | `Role` を記録し、`ToolCatalog` から `ToolAccess` を導出。ロールの一覧を別に書き換えない |
| MCP などの外部ツール | `host/tools.rs::spawn_tool(world, roles, definition, handler)` | rig-ecs の `WorldToolsExt::spawn_tool`。発見した `ToolDefinition` と `ErasedHandler` を渡すだけで、次のフレームからモデルに提示される |
| 心・記憶・自発 | `conversation::interject(world, target, source, text)` | rig-ecs の `Deliver` + `DeliveryMode::Inform`。ターンを起こさずに会話へ文脈を差し込み、履歴は `role = "context"`。心はただの system / observer として追加できる |

実行バックエンド（ACP などの作業ハーネス）だけはまだ切り口を作っていません。現在の work は内蔵 Rig のエージェントだけで、`start_work` が直接それを起動します。

## 起動とテスト

```sh
cargo run --locked -p ene-host-test
cargo test -p ene-host-test --locked
```

起動すると `127.0.0.1:8080` で待ち受け、ランタイムディレクトリの `connection.json` に endpoint と token を書きます（開発シェルの `buf` が build.rs に必要です）。モデルは `OPENAI_API_KEY` を環境変数から読みます。これは rig/OpenAI の `from_env` の形で、keyring は使いません。

## 入れていないもの

- 心・記憶・キャラクター（`docs/features` の Character / Memory）。ホストは会話とモデル呼び出しだけを持ちます。
- 送信許可（grant）と端末境界（client / device）。製品固有の境界として `ene-host` 側に残しています。
- keyring 資格情報、移行（migration）、作業サービス RPC、実行バックエンド（ACP など）。最小構成では環境変数の鍵、単一スキーマ（`CREATE TABLE IF NOT EXISTS`）、`start_work` ツール、内蔵 Rig の作業エージェントで足ります。
