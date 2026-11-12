# スケジュール実行

指定した日時や定期的なスケジュール、および一定時間経過した内部タイマーに合わせて、キャラクターが自発的に声をかけたり、任された作業を進めたりします。スケジュールの管理と実行状態の保持は常時稼働する[ホスト](../hosting/README.md)が担います。

## 具体的な振る舞い

1. **スケジュール理由のコンテキスト注入**:
   - スケジュール時刻になると、ホストが「頭」（キャラクターAgent）に対して**「スケジュール実行の理由（例: 定期リマインド、予定の時刻到来）」をコンテキストとして直接注入**します。これにより、独立した別ルーチンを作ることなく、キャラクター本来の思考回路の中でスケジュール実行が処理されます。
2. **定期Tick（前回の活動からの時間経過）**:
   - 明示的な特定予定がない場合でも、**「前回『頭』が動いてから一定時間が経過したとき」**に定期的なハートビート（Tick）として「頭」が呼び出されます（AITuberの独り言ループと同様）。
3. **「頭」による自律判断（行動選択）**:
   - スケジュールや定期Tickが発火した際、具体的に何を行うか（ユーザーにボイスで話しかけるのか、「手」に作業を発注するのか、今は何もしない/NOOPか）は、**「頭」に完全に一任**されます。
4. **`noop()` による空ターンの自動巻き戻し不変条件（段階的昇格方式）**:
   - 自律tickや見守り判定で親が `noop()` のみを呼び出して沈黙した場合、その空ターン（`[Inform: tick] -> [ToolCall: noop] -> [ToolResult: ok]`）をそのまま `SessionLog` に残すと、長時間の放置で何百ターンもの空履歴が蓄積し、コンテキスト窓と Prefix Cache を破壊します。
   - `rig-ecs` の `SessionLogPlugin` は `Deliver` が呼ばれたフレーム末尾で即座に追記専用ファイル（`DirStore`）へ書き込むため、ディスクI/O発生後に末尾を消すことはできません。
   - したがって、**自律tickの推論はメインの `Conversation` ではなく一時ビュー（またはクローン）を用いて実行し、モデルが `noop()` 以外（ユーザーへの発話テキスト、または `start_work` 等の実アクション）を返した場合にのみメインの `Conversation` 正本へ `Deliver` / 記録する「段階的昇格方式（Staged Promotion）」** を採用します。これにより、不要なディスクI/Oとセッションログの肥大化をゼロオーバーヘッドで完全に防止します。
5. **クライアント切断時の継続**:
   - クライアントが切断中であってもホスト上で自立して進められる作業は継続され、再接続時にまとめて「頭」から報告・通知されます。未決事項を重複して何度も通知することはありません。

## データモデルと管理ツール

### スケジュールデータモデル (`host.sqlite3`)

```sql
CREATE TABLE IF NOT EXISTS schedules (
    id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL,
    cron_expr TEXT,            -- 繰り返しスケジュール (5-field cron, 例: "0 9 * * 1-5")
    run_at TEXT,               -- 一度限りの実行日時 (ISO 8601, 例: "2026-11-13T10:00:00Z")
    title TEXT NOT NULL,       -- 予定・リマインドのタイトル
    instruction TEXT NOT NULL, -- 発火時に親エージェントへ投入するプロンプト
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    last_triggered_at TEXT
);
```

### 親エージェント向け Rig ツール

親エージェント（頭）は日常会話の中でスケジュールを自律的に登録・確認・解除できる：
- `schedule_create(title: String, instruction: String, run_at: Option<String>, cron_expr: Option<String>)`: 予定やリマインドを登録
- `schedule_list()`: 登録中スケジュール一覧を取得
- `schedule_cancel(schedule_id: String)`: スケジュールを解除

参考：[Hermes Agent](../../references.md#hermes-agent) のスケジュール実行、および [Grok Bot](../../references.md#grok-bot) の継続タスク・通知機能。
