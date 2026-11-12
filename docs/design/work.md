# 作業相手APIと「手」（WorkSession）協調プロトコル

状態：採用設計。[全体アーキテクチャ](../architecture.md)、[技術スタック](tech-stack.md)、[権限・Sandbox](permissions.md)、[タスク機能仕様](../features/task/README.md)を参照する。

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
| **WorkSession** | 親コンパニオンに属する、特定のタスク文脈・作業ディレクトリ・状態を保持する枠組み（ACP Sessionに1:1対応） | 複数Runにわたり継続 |
| **Run** | ユーザーや親からの1回の指示・追加入力によって開始される単一の実行ターン（ACPの `session/prompt` に対応） | 指示受付から完了/失敗まで |

### 2.1 バックエンド別の実行モデル（ACPと内蔵Rigの適正化）
- **外部ACPエージェント（Agent Client Protocol）**:
  - `WorkSession ≒ ACP Session`: 外部ACPプロセスとの接続はWorkSession単位で維持され、複数Runにわたってエージェントの会話文脈が継続する。
  - `Run ≒ ACP prompt turn`: 1回の指示実行は `session/prompt` に対応する。
  - **中断の扱い**: 通常のRun中断は ACP 標準の `session/cancel` を送信して安全に行う。外部プロセス自体の終了は、WorkSession自体の破棄時、またはセッション全体がハングして応答しない場合のみ実行する。文脈を保持するACPセッションでは、Run終了後もOSプロセスは生存する。
- **内蔵Rigエージェント**:
  - **ライフサイクルと停止伝播の分離**: Rig v0.46.0 において `StopTurn` は `SpawnedBy` 配下の子エージェントへ停止を自動伝播させる。ユーザーが音声対話を「ちょっと話止めて。作業は続けて！」と割り込み（Barge-in）した際に作業まで道連れで強制終了されるのを防ぐため、**継続稼働する `WorkSession` は `SpawnedBy(parent_entity)` ではなく、独立したエージェントエンティティ（`WorkSessionOf(companion_id)`）としてWorld内に生成する**（`SpawnedBy` はそのターンと運命を共にする一時的なサブタスクにのみ限定）。
  - ツール権限を `ToolAccess::Only(...)` で制限し、Run開始は `world.trigger(Deliver::new(work_entity, instruction, DeliveryMode::Queue))`、作業の中断は明示的な `world.trigger(StopTurn::new(work_entity))`、完了報告は `TurnFinished` Observer から親へ通知することで、同一World内で最小限のオーバーヘッドで実行する。コマンド実行子プロセスのみを `process-wrap` で安全に管理する。

> [!IMPORTANT]
> **WorkSession と Run の厳格な分離**:
> `WorkSession`（文脈の器）と `Run`（1回の実行ターン）を同一視してはならない。1つのWorkSession内で追加指示により複数のRunが順次実行される。すべてのイベントや通知には `(session_id, run_id)` を付与し、遅れて到着した過去のRun Aの完了通知が新しいRun Bの状態を上書きする事故を防ぐ。

---

## 3. データ構造・状態遷移（Rust）

### 3.1 セッションとRunの状態定義

```rust
use std::time::Instant;
use serde::{Deserialize, Serialize};

/// WorkSession（文脈の器）の公開状態（実体から導出され、二重管理による不整合を防ぐ）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkSessionStatus {
    /// 待機中（実行中のRunなし）
    Idle,
    /// Runが実行中
    Active { current_run_id: String },
    /// セッション終了・破棄済み
    Closed,
}

/// 単一Run（実行ターン）の状態
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Starting,
    Running,
    /// ユーザーへの確認・質問待ち（権限承認 WorkRequest::Approval または通常質問 WorkRequest::Input）
    WaitingInput {
        request_id: String,
        request: WorkRequest,
    },
    /// 中断要求を受け、後片付け・停止処理中
    Cancelling { requested_at: chrono::DateTime<chrono::Utc> },
    /// 正常完了
    Completed,
    /// 停止が完全に確認された中断
    Cancelled,
    /// 強制終了要求後に停止がOSレベルで完全に確認できなかった場合、または外部処理の停止状態が不明な場合
    CancellationUnconfirmed {
        reason: String,
        unverified_pids: Vec<u32>,
    },
    /// 失敗
    Failed { error_code: String, message: String },
}

### 3.2 状態遷移契約（State Machine Transitions）

| 現在の状態 | イベント / トリガー | 次の状態 | 確定点とホストの動作 |
| :--- | :--- | :--- | :--- |
| `Starting` | プロセス起動・タスク着手 | `Running` | Run専用のプロセスグループ/Job Objectを生成 |
| `Running` | 子からの質問または承認要求（`WorkRequest`） | `WaitingInput` | 親エージェントまたはユーザーの回答待ち（ポリシー判定が `NeedsApproval::add` を付与し、公式ApprovalPluginが承認用Askを生成） |
| `WaitingInput` | 承認回答受信（`SettingsService.SubmitApproval`） | `Running` | プラグイン生成の対象AskのDecisionへAnswer(Outcome)を確定。公式ApprovalPluginが承認時は対象ツールに `Approved` 付与、拒否時は `decision.decide(Decider::Person, Outcome::refused(...))` を反映後に質問の `HoldOf` を解除（Askは回答確定前にdespawnしない） |
| `WaitingInput` | 質問回答受信（`SendWork` with `request_id`） | `Running` | 未決質問 `request_id` に回答テキストを束縛して子へ渡し処理再開 |
| `Running` / `WaitingInput` | `StopWork` 受信（中断要求） | `Cancelling` | **即座に新規ツール実行受付を遮断**し、ACPへ `session/cancel` 送信（未決permissionはcancel応答）、最大30秒クリーンアップ待機 |
| `Cancelling` | ACP `cancelled` 応答 または Rigターン停止確認 | `Cancelled` | 正常中断確定（ACPプロセスはセッション維持のため生存） |
| `Cancelling` | 30秒超過 ➔ 強制終了後、プロセス終了未確認 | `CancellationUnconfirmed` | 停止未確認として記録 |
| `Running` | タスク正常完了 | `Completed` | 成果物を親へ返却 |
| `Running` / `Cancelling` | 回復不能エラー発生 | `Failed` | エラー報告 |

/// 子（手）からの問い合わせ種別
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkRequest {
    /// 権限・Sandbox外操作の承認要求
    Approval {
        action_summary: String,
        candidate_action: CandidateAction,
    },
    /// 通常の質問・選択肢の確認（例: 「PDFとMarkdownのどちらで出力しますか？」）
    Input {
        prompt: String,
        choices: Option<Vec<String>>,
    },
}

/// セッションの管理構造体（状態の正本は active_run と closed のみとし、公開statusは導出）
pub struct ActiveWorkSession {
    pub session_id: String,
    pub companion_id: String,
    pub active_run: Option<ActiveRunTracker>,
    pub closed: bool,
}

impl ActiveWorkSession {
    pub fn status(&self) -> WorkSessionStatus {
        if self.closed {
            WorkSessionStatus::Closed
        } else if let Some(run) = &self.active_run {
            WorkSessionStatus::Active {
                current_run_id: run.run_id.clone(),
            }
        } else {
            WorkSessionStatus::Idle
        }
    }
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
    /// ユーザーへの確認・質問要求（権限承認または通常質問）
    Request {
        request_id: String,
        request: WorkRequest,
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
- 子からの活動報告が閾値（デフォルト: **90秒**）を超えて途絶した場合、ホストは親（頭）へ**コンテキスト注入イベント（Prompt Injection / `DeliveryMode::Inform`）**を行う。
- **通知と返答要求の区別**: `DeliveryMode::Inform` は返答を強制しない非ブロッキング通知である。途絶通知は親のコンテキストに事実情報として追加され、親が対話中であれば次ターンの思考材料となる。即時に親からユーザーへ声かけを行わせる必要がある緊急ケースでは、ホストが `DeliveryMode::Queue` で能動的な確認ターンをディスパッチする。
- **1回のみの警告（`stall_warned`）**:
  - `now - last_activity_at > 90s` かつ `stall_warned == false` のときに限り、ホストは通知を1回だけ送出し、`stall_warned = true` に設定する。これにより、親エージェントへの通知スパムを防止する。
  - 子から新たな活動報告（`Activity` や `Message`）が到着した時点で `last_activity_at` は更新され、`stall_warned = false` にリセットされる。
  ```text
  [System Notification: 作業「手」(run_id: run_123) からの報告が90秒間途絶しています。現在の状況を確認してください。]
  ```
- 親（頭）はこの通知を受けて、ユーザーに「ちょっと様子を見てみるね」と声をかけたり、子に進捗確認メッセージを送出できる。

### 4.2 実行中の追加入力の配送（Steer / Queue / Reject）
Runが実行中にユーザーや親から追加指示が届いた場合、バックエンドの能力に応じてホストが以下のように制御する：
1. **Steer（実行中注入）**: 外部ハーネスやローカルRigが実行中入力の割り込み注入をサポートしている場合、待機中のターミナル等を安全に中断して現在のRunへ新指示を注入。
2. **Queue（キューイング）**: 実行中割り込みが非対応の場合、現在のRunが完了した直後に開始される次期Runとしてキューへ保持。
3. **Reject（非対応通知）**: キューイングも不可な場合、待機するか現在のRunを中断するかを親（頭）が選択。

---

## 5. 安全な中断プロトコル（Cancellation Protocol）

### 5.1 中断シーケンスと停止確認（Cancelled vs CancellationUnconfirmed）

```text
[ユーザー / 親] ──→ stop_work(session_id, run_id)
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
                           │                                   ホストがプロセス群を強制終了
                           ▼                                   (Linux: kill(-pgid, SIGKILL)
                    CancellationReport 送信                    Windows: TerminateJobObject)
                           │                                          │
                           ▼                                          ▼
                    【停止の完全確認】                          【停止が確認できたか？】
                    RunStatus: Cancelled                         ├─ 完了確認 ➔ Cancelled
                                                                 └─ 未確認・不明 ➔ CancellationUnconfirmed
```

1. **協調的中断要求と新規副作用の即時遮断**:
   - ホストは子へ中断を通知（ACPエージェントへは `session/cancel`、内蔵Rigへはタスク中断シグナル）し、`RunStatus` を `Cancelling` へ更新する。
   - **`Cancelling` 移行と同時に、ホストはそのRunからの新規ツール実行要求（ファイル書き込み、外部通信等）を即座に遮断（Reject）**し、クリーンアップ以外の新たな副作用の発生を防止する。
2. **プロセス終了と中断の責務分離（停止確認の3分類）**:
   - **内蔵RigのRun停止**: 対象ターンの `StopTurn` とホスト管理下の関連タスク停止を確認（`RunStatus::Cancelled`）。
   - **ACPのRun停止**: `session/cancel` を送信し、対応する `session/prompt` の `cancelled` 終了応答（および未決のpermission requestへのcancel回答）を受信した時点で中断を確定（`RunStatus::Cancelled`）。ACPエージェントのOSプロセスは会話文脈を維持するため生存させる。
   - **OSプロセスの強制終了**: 強制終了（`kill(-pgid, SIGKILL)` や `TerminateJobObject`）の対象は、**そのRunが直接起動して応答しないシェルコマンド子プロセス**、または**WorkSession破棄時（`CloseWork`）に30秒経過しても終了しないエージェントプロセス**に限定する。
3. **寛大な猶予期間（30秒タイムアウト）**:
   - サブプロセスやビルド処理が正常に一時ファイルを片付ける猶予として、最大 **30秒** 待機する。
4. **プロセス群の強制終了と停止確認**:
   - 30秒経過しても応答しない場合、ホストは対象プロセス群を強制終了する。
   - **状態の厳格な区分**:
     - プロセス群の完全終了またはACPのキャンセル応答が正常に確認された場合: `RunStatus::Cancelled`
     - プロセスの残存が疑われる場合や外部クラウド処理の停止が確認できない場合: **`RunStatus::CancellationUnconfirmed`（停止未確認 / 結果不明）** として記録し、ユーザーにも「処理を完全に停止できたか確認できませんでした」と誠実に通知する。
5. **過去Runの遅延イベントの扱い**:
   - Run Aの中断後に開始されたRun Bの実行中に、遅れてRun Aのイベントが到着した場合、**Run Bのアクティブ状態を破壊・上書きしない**。ただし、Run A自身の過去の監査記録としては正確に追記保存する。

### 5.2 外部副作用の現実的な扱い（Undoの限界の明示）
- 一時ファイルの削除などのローカルな後片付けは `local_cleanup_done: true` として処理する。
- 一方、**「送信済みメール」「外部APIで作成したクラウド資源」「リモートGit Push」などの物理的に巻き戻せない外部操作**については、「安全にUndoできる」とは偽らない。
- `CancellationReport.irreversible_effects` に「送信済みメールは取り消せません」のように明示記録し、親がユーザーへ正確な顛末を報告する。成否不明な外部操作は自動再送しない。

---

## 6. 親（頭）と子（手）が利用する操作ツール群

主体・権限はホストがセッション文脈に基づき自動束縛する。

### 6.1 親（頭）向け操作ツール（`Rig` ツール）

| ツール | 引数 | 役割 |
| :--- | :--- | :--- |
| **`start_work`** | `instruction: String`<br>`session_id: Option<String>` | 「手」に実務を委託。`session_id` 未指定時は新規WorkSessionを作成し、Runを開始して `(session_id, run_id)` を返す |
| **`send_work`** | `session_id: String`<br>`run_id: String`<br>`message: String`<br>`request_id: Option<String>` | 「手」に対する追加指示（Steering）や質問（Request）への回答を送信 |
| **`stop_work`** | `session_id: String`<br>`run_id: String`<br>`reason: Option<String>` | 実行中のRunに対する中断要求を発行 |
| **`list_work`** | なし | 実行中および直近のWorkSession・Runの状態一覧を取得 |

### 6.2 子（手）向け報告ツール（内蔵Rig / カスタムエージェント用）

| ツール | 引数 | 役割 |
| :--- | :--- | :--- |
| **`report`** | `kind: String`（`activity` / `message` / `question` / `completed` / `failed`）<br>`content: String`<br>`artifacts: Option<Vec<String>>` | ホストおよび親へ進捗、質問、または完了/失敗を通知。**`session_id` と `run_id` はホスト実行環境がコンテキストから自動束縛**し、LLMに引数として書かせない。ACP外部プロセスの場合は `agent-client-protocol-schema` の通知へ相互変換される |

---

## 7. Connect-RPC サービス定義 (`work.proto`)

### 7.1 クライアント向けRPCと親（頭）内部ツールの責務分離
- **通常のユーザー対話（親エージェント経由）**:
  ユーザーはチャット（`ChatService.SendMessage`）や音声入力（`VoiceService`）を通じて親コンパニオンへ実務や調査を依頼する。親エージェント（頭）が文脈を解釈し、内部ツール（`start_work`, `send_work`, `stop_work`, `list_work`）を実行して自律的に進行する。クライアント自身が親のオーケストレーションを肩代わりすることはない。
- **Connect-RPC `WorkService` の用途区分**:
  1. **UI一覧表示・状態取得（通常利用）**:
     - `ListWorks`: 実行中および直近の WorkSession 一覧取得（UIサイドバーやタスクパネル表示用）。
     - `GetWork`: 特定 WorkSession の詳細状態（`WorkSession`）、実行履歴（`runs`）、未決要求（`pending_requests`）、および成果物の取得。再接続時に未決の承認要求や質問を再取得できる。
     - **単一イベントストリームへの統合**: 個別のストリーム（`SubscribeWorkEvents`）は設けず、リアルタイムの作業進捗ログ・質問・完了イベントはすべて `HostService.Subscribe`（単一の `HostEvent` ストリーム）内の `HostEvent::WorkEvent`（`WorkEventEnvelope`）として集約配信される。
  2. **ユーザー主導の緊急直接停止（即時制御）**:
     - `StopWork`: ユーザーがクライアントUI上の「作業中断」ボタンを押下した際、親LLMの思考待ちを介さず即座に停止要求を発行する。
     - `CloseWork`: 完了・中断済みセッションの明示的クローズ。
  3. **開発・検証・自動化直接呼び出し（特権・開発用）**:
     - `SendWork`: 親コンパニオンの会話文脈を介さず、開発者コンソールやスクリプト連携等で直接指示を投入して検証するためのルート。冪等性キー（`request_key`）による重複実行防止を備える。

```protobuf
syntax = "proto3";
package ene.host.v1;

service WorkService {
  rpc ListWorks(ListWorksRequest) returns (WorkSessionList);
  rpc GetWork(WorkRequest) returns (WorkView);
  rpc SendWork(SendWorkRequest) returns (WorkAcceptance);
  rpc StopWork(StopWorkRequest) returns (Empty);
  rpc CloseWork(CloseWorkRequest) returns (Empty);
}

message ListWorksRequest {
  string companion_id = 1;
}

message WorkSession {
  string id = 1;
  string companion_id = 2;
  string backend = 3;
  string status = 4;
  optional string last_run_id = 5;
  optional string last_report = 6;
  string created_at = 7;
  string updated_at = 8;
  bool closed = 9;
}

message WorkSessionList {
  repeated WorkSession items = 1;
}

message RunInfo {
  string run_id = 1;
  string input_id = 2;
  string state = 3;
  optional string failure_message = 4;
}

message WorkRequestView {
  string request_id = 1;
  string kind = 2; // "approval" | "input"
  string prompt = 3;
  repeated string choices = 4;
}

message WorkView {
  WorkSession session = 1;
  repeated RunInfo runs = 2;
  repeated WorkRequestView pending_requests = 3; // 再接続時に未決の質問・承認を取得可能
}

message WorkRequest {
  string work_id = 1;
}

message SendWorkRequest {
  string work_id = 1;
  string request_key = 2; // 冪等性重複排除キー
  string text = 3;
  optional string request_id = 4; // 未決質問(WorkRequest::Input)への回答時にそのIDを明示束縛
  string client_id = 5;
}

message WorkAcceptance {
  string request_id = 1;
}

message StopWorkRequest {
  string work_id = 1;
  optional string run_id = 2;
}

message CloseWorkRequest {
  string work_id = 1;
}
```

---

## 8. 検証条件と受け入れ基準 (Alpha Work Invariants)

| 不変条件 | 検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **同一ペルソナ一人称対話** | 「手」の作業報告を受けた親の会話出力を検査 | 「裏で作業者が〜」等の第三者表現が排除され、一人称（「私が今〜したよ」）で報告されること |
| **Run完了の独立性** | Run A中断後にRun Bを開始し、遅れてRun Aの完了イベントをシミュレート到着 | Run Bのアクティブ状態が破壊・上書きされず、遅延到着したRun AのイベントはRun A自身の過去監査記録へ安全に反映されること |
| **ACPプロンプト中断とプロセス生存** | 実行中Runに対して中断要求を発行 | ACPの `session/cancel` が送信され、promptの `cancelled` 応答でRunが `Cancelled` になり、エージェントプロセス自体はセッション維持のため生存すること |
| **強制終了タイムアウトと停止確認** | 中断要求に応答しないモック外部プロセスを起動 | 30秒後にホストがプロセス群を強制終了し、停止を確認できた場合は `Cancelled`、停止が確認できない場合は `CancellationUnconfirmed`（停止未確認）として記録・報告されること |
| **非可逆操作の誠実な報告** | 外部送信済み操作を含むRunを中断 | 「Undo完了」と偽らず、`irreversible_effects` に記録されてユーザーへ報告されること |
| **途絶検知プロンプト注入** | 子の活動報告を90秒間意図的に停止 | 親エージェントへ進捗確認プロンプトが自律注入され、親が状況確認を試みること |
