# Tools

AIが外部から最新情報を収集したり、実行環境や各種サービスを安全に操作したりする際に用いるツール群を管理する領域です。ツールは将来的に順次追加・拡張される枠組みであり、本領域はその基盤的な役割を示します。

## 役割とツールの分担

### 親エージェント（頭）のツールセット（クライアント能力に応じて動的化）

親エージェント（頭）は会話テンポを維持するため、軽量・即時なツールのみを保持し、重い思考や外部操作は「手」（WorkSession）へ委譲します：

| カテゴリ | ツール名 | 役割 |
| :--- | :--- | :--- |
| **記憶 (Mind)** | `mind_recall(query, limit)` | 過去の関連エピソード記憶を検索（明示的想起） |
| | `mind_read(memory_id)` | 特定記憶の詳細と由来コンテキストを閲覧 |
| | `mind_update(action)` | 記憶の追加・訂正・忘却（想起優先度の引き下げ） |
| **作業委譲 (Hands)** | `start_work(instruction, work_id)` | 「手」（WorkSession）へ実務タスクを発注（新規または既存継続） |
| | `send_work(work_id, text, request_id)` | 進行中タスクへの追加入力や未決質問（`WorkInquiry`）への回答 |
| | `list_works()` | 担当するWorkSessionの一覧・進捗状況を取得 |
| | `stop_work(work_id, reason)` | 進行中タスクの中断を要求 |
| **端末・身体・知覚 (Client capabilities)** | `device_request(capability, input)` | 宣言・許可された端末能力を実行する。対象端末はモデルが指定せず、ホストが受付時に束縛する。高リスク操作は操作単位の承認を通る |
| | `body_motion(motion, expression)` | `body.motion`を宣言し許可されたクライアントのモーション・表情を指示（スキーマとプリセットはクライアントが定義） |
| | `set_eye_watch(target_pattern, match_kind, notify_on, enabled)` | `eye.watch`を宣言し許可されたクライアントの知覚センサーに動的監視ルールを設定（スキーマはクライアントが定義） |
| **共通** | `noop()` | 何も行動しない（沈黙・見守り。自律tickの空ターンはログから自動巻き戻し） |
| **自律スケジュール** | `schedule_create(title, instruction, run_at, cron_expr)` | タイマーや定期リマインドを登録 |
| | `schedule_list()` | 登録済みスケジュール一覧を取得 |
| | `schedule_cancel(schedule_id)` | スケジュールを解除 |

> [!NOTE]
> 身体・知覚・端末操作に関するツールは、接続中のアクティブクライアントが`RegisterClient`で宣言した`capabilities` / `capability_specs`に応じて動的に利用可能になります。宣言または許可がない場合、`body_motion`や`set_eye_watch`はモデルへ公開されません。ホストはクライアント固有の実装（描画エンジン、OSフック、ギミック）を知らず、`DeviceOperation`で委譲します。`device_request`の対象端末はモデルが引数で指定せず、ホストが受付時の`client_id`と`activation_revision`へ束縛します。能力の許可は操作単位の承認を省略せず、必要な操作は`Stage::Gate`と`ApprovalRequest`を通します。詳細は[ホストAPI](../../api.md)と[全体アーキテクチャ](../../architecture.md)、[デスクトップクライアント実装](../../design/desktop-client.md)を参照してください。

> [!NOTE]
> 現在日時は `PrepareSystems::Edit` により毎ターン最新が Dynamic Tail としてインメモリ注入されるためツール呼び出しは不要です。

タスク用Agent（手）は、サンドボックス内のファイルI/O、シェル実行、コード編集、外部MCPサーバーから動的に提供されるツール群を駆使して実務を遂行します。

本ソフトウェアはチャットUIではなくボイス対話をメインとするため、ツール実行中を示すリッチなUI表示や、キャラクターによる形式的な「調べてみるね」といった前置き発話は必須とせず、自然な会話の流れの中でツール結果を反映します。

## 機能一覧

| 機能 | 概要 |
| :--- | :--- |
| [Web検索](search.md) | 「手」（WorkSession）や外部MCPツールを通じて必要な外部情報を検索・参照する |
| [作業の依頼](../task/request.md) | 「手」（タスク用Agent）を通じて思考や作業を要する実務を実行する |
| [機能の拡張](../extensions/README.md) | 外部ツールやMCPサーバー（`rig-rmcp`）を新たに追加・接続し、ツールの幅を広げる |
