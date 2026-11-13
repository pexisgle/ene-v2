# ene-host

AIとの会話、コンパニオンの心と記憶、作業セッション、接続・設定・承認、クライアント能力と端末操作をまとめる常駐ホストです。会話の正本は rig v0.46.0 のセッションログ（`rig-ecs` + `rig-memory::DirStore`）に任せ、SQLite はコンパニオン・設定・承認・受付・作業・クライアント・記憶だけを持ちます。

> このパッケージは [ホストAPI設計](../../docs/api.md)（§7 のカノニカル Protobuf 定義が正本）と [全体アーキテクチャ](../../docs/architecture.md) に沿って書き直した実装です。製品としての振る舞いは [docs/features](../../docs/features/README.md)、知覚・身体・音声の契約は [共通仕様](../../docs/design/voice-avatar.md) を参照してください。**接続経路の認証はOwner Bearerのみ**です（[ホスト接続の設計](../../docs/design/host-access.md) の移行は未完了）。`client_id` は接続端末の区別であり、認証情報ではありません。

## 起動

```sh
nix develop
just run-host
# または
cargo run --locked -p ene-host
```

ホストは `127.0.0.1` の空きポートで待ち受け、ランタイムディレクトリの `connection.json` に `endpoint`（`/rpc` 付き）と `token` を公開します。ディレクトリは0700、ファイルは0600で、終了時に削除します。同じ状態またはランタイムディレクトリでの二重起動はロックで拒否します。SIGINT/SIGTERMで購読ストリームを閉じ、保存を終えてから終了します。

環境変数:

| 変数 | 用途 |
| --- | --- |
| `ENE_HOST_STATE_DIR` | `host.sqlite3` とRigセッションログ（`<state>/rig/`）の保存先 |
| `ENE_HOST_RUNTIME_DIR` | `connection.json` とロックの保存先 |
| `ENE_HOST_SESSION_CREDENTIALS` | 設定時、登録した資格情報をOSキーリングではなくメモリだけに保持する |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` | 登録済み資格情報がない場合のフォールバック |

`connection.json` のendpointとtokenを `ene_rpc::connect` に渡し、生成された `HostServiceClient` / `ChatServiceClient` / `SettingsServiceClient` / `WorkServiceClient` / `ClientServiceClient` を使います。

## 設計との対応

- **1つのヘッドレスRig App** を専用スレッドで動かします。APIからの操作は `Runtime::call`（変更。1フレーム進めて通知を配信）と `Runtime::read`（読み取りのみ。時間を進めない）でこのスレッドへ直列化します。
- **会話ログが正本**: 会話・受信箱・ターン・モデル/ツール呼び出しは `rig-ecs` が所有し、`<state>/rig/` の `DirStore` が正本です。`GetHistory` / `GetMessage` はこのログからの読み取り専用投影です。受理レシート（`ene.accepted`）と要約スナップショット（`ene.summary`）も `Conversation::add_aside` でログ内に置きます。
- **静的な人格と動的な文脈の分離**: 人格の核・作者プロフィールは `SystemPrompt` に固定します。現在時刻・現在の心・Pinned Memory・想起記憶・知覚要約は `PrepareSystems::Edit` で最新ユーザー発話の `ModelRequest.history` にだけ結合し、`SessionLog` には書きません。ターン開始時に一度だけ生成してReAct反復で使い回します。
- **文脈窓の管理**: `PrepareSystems::Fit` で要求全体のトークン数を見積もり、古い完全ターンから要求ビューを切り詰めます。固定部分だけで上限を超える場合は送信せず `context_exceeded` で拒否します。
- **承認ゲート**: `Stage::Gate`（排他システム）で、ホスト管理ツール以外の端末操作をクライアント世代に束縛し、ポリシーと能力宣言（`AUTO` / `CONFIRM` / 未指定）から `ApprovalRequest` を作るか拒否します。保留は `HoldOf` で表現し、`SettingsService.SubmitApproval` が `Approved` マーカーを付けて解放します。要求の引数ハッシュが変わっていれば承認は失効します。
- **クライアント能力**: `RegisterClient` / `SetCapabilities` の宣言（能力名・JSON Schema・プリセット・ツール名・承認要否・承認対象テンプレート）を検証し、ホストが許可を決めます。`tool_name` を持つ許可済み能力だけが親エージェント向けのツールとして公開され、未使用スロットは `Disabled` で隠れます。`tool_name` のない能力は汎用 `device_request` からのみ呼べます。能力の許可は操作単位の承認を省略しません。
- **端末操作**: `ClientService.DeviceStream` 上の `DeviceOperation` として配送します。対象端末はモデルが指定せず、親は受付時のアクティブクライアント、手は `StartWork` 時に保存した `client_id` と `activation_revision` に束縛します。受付後に移動した場合は `client_moved` で失敗させ、旧世代の結果・知覚・承認は破棄します。切断では未応答の操作を `uncertain` とし、再接続時に未配送分だけ再送します。
- **作業（手）**: `WorkSession`（文脈）と `Run`（1実行）を分け、`RunInfo` は `RunStatus` と `pending_request_id` / `cancellation_*` / `unverified_pids` を返します。内蔵Rigの子エージェントとして `report` ツール（activity / message / question / completed / failed）を持ち、質問は `pending_inquiries` と `AnswerInquiry` で解決し、完了・失敗は一人称の報告として親へ配送します。`StopWork` は `Cancelling` を経て `StopTurn` で停止し、`CloseWork` は明示的なクローズです。
- **心と記憶**: SQLite（WAL、`busy_timeout=5000`、`synchronous=NORMAL`）に保存します。FTS5 trigram（3文字未満はLIKE）と順位融合で候補を取り、`Score = min(1, Importance/0.3) * (0.5*Relevance + 0.3*Importance + 0.2*Recency)` で並べます。忘却は `importance` の引き下げであり、削除や除外フラグではありません。`mind_read` などの明示利用だけが `last_accessed_at` を更新します（引用ガード）。埋め込み接続が未設定のため、意味検索は未実装です（語彙検索のみ）。
- **設定と接続**: `Connection` は共通ヘッダー（name / enabled / kind / revision / last_check）と種別ごとの config（LLM / TTS / STT / Embedding / MCP / ACP）を持ちます。LLMはrig-coreのカタログからモデル仕様を引き、`ConnectOptions` で資格情報・base URL・HTTPクライアントを渡してエージェントの `Connection` を組み立てます。資格情報はOSキーリング（`ENE_HOST_SESSION_CREDENTIALS` でメモリのみ）に保存し、読み出しAPIは状態だけを返します。
- **知覚（目）**: `ClientService.ReportPerception` を受信し、アクティブウィンドウ・活動状態・クライアント要約を短い要約にして動的文脈へ渡します。古い `activation_revision` のイベントは破棄します。`set_eye_watch` は `eye.watch` を宣言・許可されたアクティブクライアントがあるときだけ公開され、ルールは解釈せず `DeviceOperation` として配送します。
- **通知**: 1つのアウトボックスに溜め、フレームごとに起動epochと連番カーソルを付けて配信します。購読開始時は `initial_state`、取りこぼしでは `resync_required` を送ります。保存済み状態が正本です。

## 内部構成

| パス | 責務 |
| --- | --- |
| `src/server/` | 起動、プライベートディスカバリ、Owner Bearer、Connectサービス |
| `src/host/mod.rs` | Appスレッド、`Runtime`、スナップショット、購読、失敗時の停止 |
| `src/host/agent.rs` | コンパニオン/作業エージェントの作成、モデル束縛、プロンプト、ターン・文字片の投影 |
| `src/host/chat.rs` | 受理レシート、停止、履歴、メッセージ |
| `src/host/mind/` | コンパニオンCRUD、記憶、想起スコア、動的文脈とFit、`mind_*` ツール |
| `src/host/permission/` | `Stage::Gate`、承認保留と解決、ポリシー判定 |
| `src/host/client/` | クライアント登録、能力ツールの公開、有効化世代、知覚、`DeviceStream` と操作台帳 |
| `src/host/work/` | WorkSession/Run、`start_work` などのツール、`report`、質問と停止 |
| `src/host/model.rs` | 資格情報、LLM接続、モデル一覧、接続チェック |
| `src/host/db/` | typed SQLite store とbaseline schema |
| `src/host/tests.rs` | ECSループを含む挙動テスト（受理、ゲート、端末束縛、作業報告） |

## データベース

`host.sqlite3` は `PRAGMA user_version = 1` の単一baselineです。主なテーブルは `companions` / `connections` / `settings_state` / `policy_rules` / `approval_requests` / `clients` / `activations` / `work_sessions` / `work_runs` / `work_inquiries` / `device_operations` / `memories` / `memories_fts` / `memory_edges` / `consolidation_state` / `mind_operations` / `schedules` です。未知の新しい版のDBは起動時に拒否します。開発用のstateディレクトリを削除して作り直してください。

- `activations` がコンパニオンごとのアクティブクライアントと世代（`revision`）を持ちます。移動はCASで世代を進め、旧世代の未完了操作を `client_moved` にします。
- 承認は `approval_requests` に残し、起動時に未解決分を失効させます。端末能力の承認は `client_id` / `activation_revision` / `capability` も保存します。
- `memory_edges` は `remembers` / `cited_in` / `estimated_cited_in` / `supersedes` の高々1ホップ関係です。

## API契約

`crates/ene-rpc/proto/host.proto` が正本で、`docs/api.md` §7 と一致します。有限の状態はenum、イベント詳細は `oneof`、自由形式の値は `JsonDocument`（schema + JSON text）で運びます。`Subscribe` は `Notification` の単一ストリームで、`activation_updated` や `device_operation` もここを流れます。実際の端末操作の配送は `ClientService.DeviceStream` だけです。

## 現時点で実装していないもの

- `VoiceService`（STT/TTS/再生進捗）は接続エンジン未実装のため、すべて明示的な `unimplemented` を返します。音声の世代管理・Flush・フロア制御は未着手です。
- 自動整理（Consolidation）のバックグラウンドLLMジョブ、Compaction要約の生成、埋め込み検索、MCP/ACPバックエンド、sandbox実行、自律Tick/スケジュール発火、スキルファイルは未実装です。`schedule_create` / `list` / `cancel` は保存のみで、発火機構は未実装です。
- 知覚の優先割り込みによる親ターンの自動起床、音声承認、マルチコンパニオンの音声調停は未実装です。

## 検証

```sh
cargo test -p ene-host --locked
cargo clippy -p ene-host --all-targets --all-features --locked -- -D warnings
```

実プロバイダーを呼ぶ受入テストは通常のテストに含めません。必要な場合は一時stateディレクトリと資格情報を用意し、手動でホストを起動して確認します。
