# 作業相手APIと「手」（WorkSession）協調プロトコル

状態：Alphaの採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[技術スタック](tech-stack.md)、[権限・Sandbox](permissions.md)、[Alpha実装計画](../alpha.md)、[タスク機能仕様](../features/task/README.md)を参照する。

---

## 1. 責任の分担とキャラクターの同一性

### 1.1 同一キャラクター人格の維持
「頭」（Brain / 親コンパニオン）と「手」（Hands / 作業エージェント）は内部実装上の技術的な役割分担であり、**ユーザーから見れば完全に同一の一人のキャラクター（人格）**である。
- **一人称対話の原則**:
  - 親（頭）のプロンプトにおいて、「作業エージェントは自分自身の『手』であり、別人物ではない。報告時は第三者表現（『裏で作業してくれているみたい』）を固く禁じ、必ず一人称（『私が今〜しているよ』『〜まで終わったよ』）で対話すること」を厳格に指定する。
  - ユーザーにとっての一貫した愛着・パートナーシップを保つため、親と子の分担は内部アーキテクチャに閉じ、対外的な人格を分裂させない。

### 1.2 柔軟なタスク委託（軽量作業から重い実務まで）
- 親（頭）から「手」への指示は、「作業ゴール」「制約条件」といった大掛かりな枠組みを強制しない。
- 自然言語1行の指示（「このフォルダのファイル一覧を出して」「このWebページを要約して」）から、複数ステップのプログラミング実務まで、自然言語テキスト（`instruction: String`）で柔軟に委託できる。
- 実務スキルの詳細プレイブック（`skills/<name>/SKILL.md`）は子（手）のみがオンデマンドで読み込み、親（頭）のプロンプトを圧迫させない。

---

## 2. 概念モデルと三層の識別

| 概念 | 責務 | ライフサイクル |
| :--- | :--- | :--- |
| **BackendProfile** | モデル割当、認証情報、実行環境（ローカルRig / 外部ACP / Wasm）、Sandbox設定 | 静的設定 |
| **WorkSession** | 親コンパニオンに属する、特定のタスク文脈・作業ディレクトリ・状態を保持する枠組み | 複数Runにわたり継続 |
| **Run** | ユーザーや親からの1回の指示・追加入力によって開始される単一の実行ターン | 指示受付から完了/失敗まで |

> [!IMPORTANT]
> **WorkSession と Run の厳格な分離**:
> `WorkSession`（文脈の器）と `Run`（1回の実行ターン）を同一視してはならない。1つのWorkSession内で追加指示により複数のRunが順次実行される。すべてのイベントや通知には `(session_id, run_id)` を付与し、遅れて到着した過去のRun Aの完了通知が新しいRun Bの状態を上書きする事故を防ぐ。

---

## 3. データ構造・状態遷移（Rust）

### 3.1 セッションとRunの状態定義

```rust
use std::time::Instant;
use serde::{Deserialize, Serialize};

/// WorkSession（文脈の器）の状態
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkSessionStatus {
    /// 待機中（実行中のRunなし）
    Idle,
    /// Runが実行中
    Active { current_run_id: String },
    /// セッション終了・破棄済み
    Terminated,
}

/// 単一Run（実行ターン）の状態
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Starting,
    Running,
    /// ユーザーへの確認・権限承認待ち
    WaitingApproval { request_id: String },
    /// 中断要求を受け、後片付け・停止処理中
    Cancelling { requested_at: chrono::DateTime<chrono::Utc> },
    /// 正常完了
    Completed,
    /// 中断完了
    Cancelled,
    /// 失敗
    Failed { error_code: String, message: String },
}

/// セッションの管理構造体
pub struct ActiveWorkSession {
    pub session_id: String,
    pub companion_id: String,
    pub status: WorkSessionStatus,
    pub active_run: Option<ActiveRunTracker>,
}

/// 実行中Runのトラッキング
pub struct ActiveRunTracker {
    pub run_id: String,
    pub instruction: String,
    pub status: RunStatus,
    pub started_at: Instant,
    pub last_activity_at: Instant,
    pub stall_warned: bool,
}
```

### 3.2 共通イベント包み（WorkEventEnvelope）

すべての子（手）からのイベントは、対象セッションとRunを不変に識別する共通ヘッダーで包まれる。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkEventEnvelope {
    pub session_id: String,
    pub run_id: String,
    pub sequence_no: u64,
    pub timestamp_ms: u64,
    pub payload: WorkEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkEventPayload {
    /// 実行中の詳細活動ログ（例: "Cargo test実行中..."）
    Activity { message: String },
    /// 親へ伝えるべき意味のある進捗メッセージ
    Message { content: String },
    /// ユーザーへの確認・承認要求
    ApprovalRequest {
        request_id: String,
        action_summary: String,
        candidate_action: CandidateAction,
    },
    /// Runの正常完了報告
    Completed {
        summary: String,
        artifacts: Vec<String>,
    },
    /// 中断完了報告
    CancellationReport {
        progress_summary: String,
        local_cleanup_done: bool,
        /// 物理的に取り消せない外部副作用（例: 送信済みメール、Push済みコミット）
        irreversible_effects: Vec<String>,
        /// 成否が確認できなかった外部操作
        uncertain_effects: Vec<String>,
        forced_killed: bool,
    },
    /// Runの失敗報告
    Failed { error_code: String, message: String },
}
```

---

## 4. 進捗途絶検知と割り込み処理

作業が長時間に及ぶ場合、「手」は困ったときだけでなく、数十秒〜数分間隔で定期的に進捗（`Activity` または `Message`）を報告する。

### 4.1 途絶検知と親へのプロンプト注入
- 子からの活動報告が閾値（デフォルト: **90秒**）を超えて途絶した場合、ホストは親（頭）へ**コンテキスト注入イベント（Prompt Injection）**を行う。
  ```text
  [System Notification: 作業「手」(run_id: run_123) からの報告が90秒間途絶しています。現在の状況を確認してください。]
  ```
- 親（頭）はこの通知を受けて、ユーザーに「ちょっと様子を見てみるね」と声をかけたり、子に進捗確認メッセージを送出できる。

### 4.2 ターミナル待ちの割り込み
- 子がターミナルコマンドや長時間プロセスの出力を待機している間に、親やユーザーから追加指示や確認が届いた場合、ホストはその待機を安全に中断（Interrupt）し、新しい指示をプロンプト注入して処理を再開させる。

---

## 5. 安全な中断プロトコル（Cancellation Protocol）

### 5.1 中断シーケンスとタイムアウト強制終了

```text
[ユーザー / 親] ──→ cancel_work(session_id, run_id)
                           │
                           ▼
                 RunStatus を Cancelling へ更新
                 ホスト ──→ 子へ CancellationRequested を送信
                           │
                           ├─────────────────────────────────────────┐
                           ▼ (< 30秒以内)                             ▼ (30秒経過・ハング)
                    [子が協調的に後片付け]                      [外部プロセス応答なし]
                    ・一時ファイル削除                                │
                    ・ローカルロック解放                              ▼
                           │                                   ホストが外部プロセスを
                           ▼                                   強制終了 (SIGKILL)
                    CancellationReport 送信                           │
                           │                                          ▼
                           └─────────────────────────────────→  RunStatus: Cancelled
                                                                親へ顛末を一人称で報告
```

1. **協調的中断要求**: ホストは子へ `CancellationRequested` を通知し、`RunStatus` を `Cancelling` へ更新する。
2. **寛大な猶予期間（30秒タイムアウト）**:
   - 外部プロセスやビルド処理が正常に一時ファイルを片付ける猶予として、最大 **30秒** 待機する。
3. **強制終了（Forced Kill）**:
   - 外部ACPハーネスやサブプロセスが30秒経過しても応答しない場合、ホストはOSレベルでプロセスを強制終了（Linux: `SIGKILL`、Windows: `TerminateProcess`）する。
   - `CancellationReport { forced_killed: true, ... }` を生成し、セッションを安全に `Cancelled` 状態へ収束させる。

### 5.2 外部副作用の現実的な扱い（Undoの限界の明示）
- 一時ファイルの削除などのローカルな後片付けは `local_cleanup_done: true` として処理する。
- 一方、**「送信済みメール」「外部APIで作成したクラウド資源」「リモートGit Push」などの物理的に巻き戻せない外部操作**については、「安全にUndoできる」とは偽らない。
- `CancellationReport.irreversible_effects` に「送信済みメールは取り消せません」のように明示記録し、親がユーザーへ正確な顛末を報告する。成否不明な外部操作は自動再送しない。

---

## 6. 親（頭）が利用する操作ツール群

親エージェントには以下のツールが提供される（主体・権限はホストが自動束縛）：

| ツール | 引数 | 役割 |
| :--- | :--- | :--- |
| **`instruct_hand`** | `session_id: Option<String>`<br>`instruction: String` | 「手」に実務を委託。`session_id` 未指定時は新規WorkSessionを作成し、Runを開始して `(session_id, run_id)` を返す |
| **`respond_hand`** | `session_id: String`<br>`run_id: String`<br>`request_id: String`<br>`answer: String` | 「手」からの質問（Request）に対するユーザーの回答や追加情報を返答 |
| **`cancel_work`** | `session_id: String`<br>`run_id: String` | 実行中のRunに対する中断要求を発行 |

---

## 7. Connect-RPC サービス定義 (`work.proto`)

```protobuf
syntax = "proto3";
package ene.host.v1;

service WorkService {
  rpc StartWork(StartWorkRequest) returns (StartWorkResponse);
  rpc SendWork(SendWorkRequest) returns (SendWorkResponse);
  rpc RespondWork(RespondWorkRequest) returns (RespondWorkResponse);
  rpc CancelWork(CancelWorkRequest) returns (CancelWorkResponse);
  rpc SubscribeWorkEvents(SubscribeWorkEventsRequest) returns (stream WorkEventEnvelope);
}

message StartWorkRequest {
  optional string session_id = 1;
  string instruction = 2;
}

message StartWorkResponse {
  string session_id = 1;
  string run_id = 2;
}

message CancelWorkRequest {
  string session_id = 1;
  string run_id = 2;
}

message CancelWorkResponse {
  string session_id = 1;
  string run_id = 2;
  bool accepted = 3;
}
```

---

## 8. 検証条件と受け入れ基準 (Alpha Work Invariants)

| 不変条件 | 検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **同一ペルソナ一人称対話** | 「手」の作業報告を受けた親の会話出力を検査 | 「裏で作業者が〜」等の第三者表現が排除され、一人称（「私が今〜したよ」）で報告されること |
| **Run完了の独立性** | Run A中断後にRun Bを開始し、遅れてRun Aの完了イベントをシミュレート到着 | Run Bの実行状態が破壊・上書きされず、Run Aの遅延イベントが無視されること |
| **強制終了タイムアウト** | 中断要求に応答しないモック外部プロセスを起動 | 30秒後にホストがプロセスを強制終了し、`forced_killed = true` としてCancelledで終了すること |
| **非可逆操作の誠実な報告** | 外部送信済み操作を含むRunを中断 | 「Undo完了」と偽らず、`irreversible_effects` に記録されてユーザーへ報告されること |
| **途絶検知プロンプト注入** | 子の活動報告を90秒間意図的に停止 | 親エージェントへ進捗確認プロンプトが自律注入され、親が状況確認を試みること |
