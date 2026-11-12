# ene-host

AIとの会話、コンパニオンの心と記憶、作業セッション、接続・設定・承認、端末操作をまとめる常駐ホストです。会話の正本は Rig v0.46.0 のセッションログ（`rig-ecs` + `rig-memory::DirStore`）に任せ、SQLite はコンパニオン・設定・承認・受付・作業・端末・記憶だけを持ちます。

> 現行実装の説明です。製品としての振る舞いは [docs/features](../../docs/features/README.md)、採用設計は [docs/architecture](../../docs/architecture.md) を参照してください。**接続経路の認証はOwner Bearerのみ**です。外部アクセス制御への移行が完了するまで、この認証を外さないでください。`client_id` は接続端末の区別であり、認証情報ではありません。

## 起動

```sh
nix develop
just run-host
# または
cargo run --locked -p ene-host
```

ホストは `127.0.0.1` の空きポートで待ち受け、ランタイムディレクトリの `connection.json` に `endpoint`（`/rpc` 付き）と `token` を公開します。ディレクトリは0700、ファイルは0600で、終了時に削除します。同じ状態またはランタイムディレクトリでの二重起動は拒否します。

環境変数:

| 変数 | 用途 |
| --- | --- |
| `ENE_HOST_STATE_DIR` | `host.sqlite3` とRigセッションログ（`<state>/rig/`）の保存先 |
| `ENE_HOST_RUNTIME_DIR` | `connection.json` とロックの保存先 |
| `ENE_HOST_SESSION_CREDENTIALS` | 設定時、登録した資格情報をメモリだけに保持する |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` | 接続に登録済み資格情報がない場合のフォールバック |

`connection.json` のendpointとtokenを `ene_rpc::connect` に渡し、生成された `HostServiceClient` / `ChatServiceClient` / `SettingsServiceClient` / `WorkServiceClient` / `ClientServiceClient` を使います。`VoiceService` は契約のみで未実装です。

## 仕組み

- **1つのヘッドレスRig App** を専用スレッドで動かします。APIからの読み書きは `Runtime::call` / `Runtime::read` でこのスレッドへ直列化します。`read` はスナップショットや履歴の取得だけで、AgentLoopと仮想時間を進めません。
- **SQLiteは同じRig Appスレッドの単一接続** で同期実行します。PK参照・FTS5・ルール照合はマイクロ秒規模なので、ECSのシステムから直接読み書きし、非同期ワーカーへの往復を作りません。
- 会話・受信箱・ターン・モデル／ツール呼び出しはRigが所有し、`<state>/rig/` のセッションログが会話の正本です。`GetHistory` / `GetMessage` はこのログからの読み取り専用投影です。
- 人格の核と作者プロファイルは安定したsystem promptに置きます。現在の心・Pinned Memory・想起記憶・現在時刻は `SessionLog` へ書かず、`ModelRequest.history` の末尾へリクエストごとに一時注入します。作業エージェントにも現在時刻だけを同じ仕組みで注入します。
- 自動記憶（Consolidation）は確定ターンだけを前向きに読み、`importance` と `audiences` を決めて、記憶と処理済みカーソルを同じSQLiteトランザクションで確定します。
- Compactionは `rig-memory` の `CompactionPolicy` / `Summarizer` を使った **View専用のローリング要約** です。`SessionLog` は不変の追記型のままにし、要約は `ModelRequest.history` の先頭へだけインメモリ注入します。スナップショットは `ene.summary` Aside としてログへ追記し、起動時に復元します。`GetHistory` には現れません。ユーザーの発話が届いた時点で実行中の要約は破棄し、次回のアイドル時へ延期します。
- モデルは `ModelAssignments` の7役割（companion/task/perception/compaction/consolidation/decision/embedding）から選び、未指定時はdocsのフォールバック順で解決します。
- 親は `mind_*`、`start_work` / `send_work` / `list_works` / `stop_work`、`body_motion` / `set_eye_watch` / `noop`、`device_request` を保持します。スケジュールツールは発火機構が完成するまで公開しません。
- 端末操作はアクティブ端末1台へ配送します。オフラインなら `pending`、配送後に切断したら `uncertain` とし、自動再送しません。成否不明を成功として扱いません。
- 同じ `request_key` の再送は元の受理結果を返し、内容が違えば競合です。対応する入力やターン記録を確認できない `ene.accepted` は未確定として無視し、新しい受理を作ります。
- ストレージまたはセッションログの書込みに失敗した場合、ホストは新規受理を止め、未保存の状態を通知しません。

## 内部構成

| パス | 責務 |
| --- | --- |
| `src/server/mod.rs` | 起動、シグナル、終了 |
| `src/server/access.rs` | プライベートディレクトリ、ロック、`connection.json` |
| `src/server/rpc.rs` | 生成されたConnectサービス、Owner Bearer、Host/Origin検証 |
| `src/host/mod.rs` | Appスレッド、`Runtime`、リソース、通知、スナップショット、失敗・復元・停止 |
| `src/host/agent.rs` | `AgentKey`、system prompt、Open tool、Prepare/Fit/Gate/Review、ストリーム、ターンイベント |
| `src/host/chat.rs` | 入力受理（冪等レシート）、停止、履歴、メッセージ |
| `src/host/companion.rs` | コンパニオンのドメイン型 |
| `src/host/mind/mod.rs` | コンパニオンCRUD、作者プロファイル、現在の心、削除 |
| `src/host/mind/memory.rs` | 記憶制御（forget/remember/share/unshare） |
| `src/host/mind/recall.rs` | 動的コンテキストの構築と `ModelRequest.history` への注入 |
| `src/host/mind/consolidate.rs` | 増分自動整理、`importance`、カーソル |
| `src/host/mind/tools.rs` | `mind_recall` / `mind_read` / `mind_update` |
| `src/host/compaction.rs` | View専用ローリング要約（SessionLog不変） |
| `src/host/work.rs` | 作業セッション、Run、StartWork/SendWork/AnswerInquiry、親への報告 |
| `src/host/body.rs` | `body_motion` / `set_eye_watch` / `noop` |
| `src/host/settings.rs` | 接続、資格情報、ポリシー、承認 |
| `src/host/client/` | 端末登録、排他アクティブ化、デバイス操作 |
| `src/host/model/` | 組み込みプロバイダー、Keyring資格情報、認可トランスポート |
| `src/host/db/` | typed SQLite store、baseline schema（schedule・work inquiry を含む） |
| `src/host/wire.rs` | ドメイン型からproto viewへの変換、JSON text、agent key |

## データベース

`host.sqlite3` は `PRAGMA user_version = 1` の単一baselineで、WAL・`busy_timeout = 5000`・`synchronous = NORMAL` を使います。主なテーブルは `companions` / `activations` / `connections` / `policy_rules` / `approval_requests` / `clients` / `work_sessions` / `work_inquiries` / `schedules` / `device_operations` / `mind_operations` / `memories` / `memories_fts` / `memory_edges` / `consolidation_state` です。

- 記憶の本文は `memories`、全文検索は `memories_fts`（FTS5 trigram、外部コンテンツ方式）、認知的関係は `memory_edges`、自動整理のカーソルは `consolidation_state` に置きます。
- `importance` は 0〜10 の想起優先度です。自動想起は `>= 2`、明示的な `mind_recall` は全件を対象にします。`forget` は 0、`remember` は最低3へ戻します。
- `audiences` は共有先companion IDのJSON配列です。共有プールは `["*"]`、private は `[]` です。
- `activations` がコンパニオンごとのアクティブ端末と版を持ちます。`Snapshot.active_companion_id` はその先頭です。
- 承認・ポリシーは `approval_requests` / `policy_rules` に保存します。`Stage::Gate` でルールを評価し、`confirm` は承認要求を発行してツール呼び出しを止めます。承認は対象の呼び出しに `Approved` マーカーを付けてから保留を解放し、次の `Stage::Gate` が要求を一度だけ消費します。拒否は `Decider::Person` で確定します。停止などで保留が消えた場合は、対応する pending 承認を失効させます。明示的な `confirm` は保存済み許可より優先し、現行の承認はすべて `confirm` 由来のため `always_approve` も単発承認として扱います。起動時に実行対象のない pending 承認は失効させます。ポリシー読取失敗は fail-closed です。ルールの `pattern` は正規化引数 JSON 全体またはその文字列値との完全一致で、部分文字列では一致しません。
- 過去のJSONドキュメント方式（`documents` テーブル）や旧baselineは移行しません。未知のDB・新しい版のDBは起動時に拒否します。開発用のstateディレクトリを削除して作り直してください。

## API契約

`crates/ene-rpc/proto/host.proto` が正本です。任意JSONはラッパー型ではなくJSON textの `string` で運びます。`UpdateCompanionRequest` は7役割すべての assignment patch と `persona` / `voice_identifier` を持ちます。`Subscribe` は `Notification` のストリームで、開始時に `initial_state`（Snapshot）、取りこぼし時に `resync_required` を送ります。`ClientService.ReportPerception` は知覚イベントを受け取り、直近の要約を動的文脈へ渡します。

## 現時点で実装していないもの

- `VoiceService`（STT/TTS/再生進捗）は未実装で、全RPCが `unimplemented` を返します。
- 知覚Watcherのクライアント側評価、スケジュールの発火、埋め込み検索、MCPツール、ACPバックエンド、sandbox実行、自発実行、外部アクセス制御は未実装です。
- `WorkService.ListWorks` / `GetWork` は会話から `runs` を投影します。作業エージェントの `report(kind="question")` は `work_inquiries` に記録され、`AnswerInquiry` で解決します。

## 検証

```sh
cargo test -p ene-host --locked
cargo test -p ene-host --test process --locked
cargo clippy -p ene-host --all-targets --all-features --locked -- -D warnings
```

実プロバイダーを呼ぶ受入テストは通常のテストに含めません。必要な場合は一時stateディレクトリと資格情報を用意し、手動でホストを起動して確認します。
