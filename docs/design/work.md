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
| **BackendProfile** | モデル割当、認証情報、実行環境（ホスト内Rig） | 静的設定 |
| **WorkSession** | 親コンパニオンに属する、特定のタスク文脈・状態を保持する枠組み。ワークエージェントを1体だけ保持する | 複数Runにわたり継続 |
| **Run** | ユーザーや親からの1回の指示・追加入力によって開始される単一の実行ターン | 指示受付から完了/失敗まで |

### 2.1 実行モデル（プラグインAPI経由での統一）
- **ACPエージェント（Agent Client Protocol）**: ホストはACPプロセスを起動しない。ACPはプラグインが包み、ホストは`CONNECTION_KIND_ACP`の接続設定だけを保持してプラグインへ渡す。接続の維持と`session/prompt`・`session/cancel`の対応はプラグインの内側で完結し、ホストのWorkSessionとRunの状態機械には現れない。
- **内蔵Rigエージェント**:
  - **WorkSessionごとに1体**: WorkSessionごとにホスト内でワークエージェントを1体だけ生成し、Runを開始するたびにそのrun_idを束縛し直す。エージェントを引退させるのはセッションのクローズ、開始失敗、ホスト再起動だけで、通常のRun終了では残す。
  - **停止伝播の分離**: ワークエージェントは親のターンに連なる子ではなく、独立したエージェントエンティティとしてWorld内に生成する。ユーザーが音声対話を「ちょっと話止めて。作業は続けて！」と割り込んでも、親のターンの停止が作業のターンへ伝播しない。
  - ツール権限は `ToolAccess::Only(["report", "device_request"])` に限る。Runの開始は `DeliveryMode::Queue` の `Deliver`、中断は `StopTurn`、終了はターン結果を受け取るObserverが `settle_run` で確定する。コマンド実行子プロセスのみを `process-wrap` で管理する。

> [!IMPORTANT]
> **WorkSession と Run の厳格な分離**:
> `WorkSession`（文脈の器）と `Run`（1回の実行ターン）を同一視してはならない。1つのWorkSession内で追加指示により複数のRunが順次実行される。すべてのイベントや通知には `(session_id, run_id)` を付与し、遅れて到着した過去のRun Aの完了通知が新しいRun Bの状態を上書きする事故を防ぐ。

---

## 3. データ構造・状態遷移（Rust）

### 3.1 セッションとRunの状態定義

```rust
/// WorkSession（文脈の器）の公開状態
pub enum WorkStatus {
    /// 待機中（実行中のRunなし）
    Idle,
    /// Runが実行中
    Active,
    /// セッション終了・破棄済み
    Closed,
}

/// 単一Run（実行ターン）の状態
pub enum RunState {
    Starting,
    Running,
    /// 承認待ち。承認はターン内で待つため、実運用では Running のまま
    WaitingInput,
    /// 正常完了
    Completed,
    /// 停止が確定した中断
    Cancelled,
    /// 失敗
    Failed,
}

pub struct WorkSession {
    pub id: String,
    pub companion_id: String,
    pub title: String,
    pub status: WorkStatus,
    /// 発注元クライアント。発注時にアクティブクライアントがなければ空
    pub client_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct Run {
    pub run_id: String,
    pub work_id: String,
    pub instruction: String,
    pub state: RunState,
    /// WaitingInput 中の承認要求
    pub pending_request_id: Option<String>,
    pub failure: Option<Failure>,
    pub started_at: String,
    pub updated_at: String,
}

/// 途絶検知のランタイム状態（Run行とは別に保持し、状態を二重管理しない）
struct Live {
    last: Instant,
    warned: bool,
}
```

プロトコルでは`RunInfo.state`と`WorkRunFinished.state`を`RunStatus` enumで表す。承認要求は`pending_request_id`、失敗の理由は`Failure`に載せる。旧ビルドが保存した`cancelling`と`cancellation_unconfirmed`の行は、読み込み時に`Cancelled`として解釈する。

### 3.2 状態遷移契約（State Machine Transitions）

| 現在の状態 | イベント / トリガー | 次の状態 | 確定点とホストの動作 |
| :--- | :--- | :--- | :--- |
| （なし） | `start_work` / `StartWork` によるRun開始 | `Running` | run行を`Running`で作り、セッションを`Active`にして`RunStarted`を配信する。開始に失敗したら`Failed`へ落とし、セッションを`Idle`へ戻す |
| `Running` | ターンが回答で終了 | `Completed` | `report(kind=completed)`の内容、なければ最終返答を親へ渡す |
| `Running` | `report(kind=failed)` またはターン失敗 | `Failed` | `Failure`を保存し、親へ失敗を渡す |
| `Running` | `stop_work` / `StopWork` | `Cancelled` | ワークエージェントへ`StopTurn`を送り、その場で`Cancelled`を保存する（§5） |
| `Completed` / `Cancelled` / `Failed` | 再度の停止・遅延到着した報告 | 変化なし | 終端。状態を書き換えない |

`Starting`はRun行の作成前を表す予約値で、ホストは`Running`で行を作る。`WaitingInput`も予約で、承認待ちは`Running`のままターン内で待つ。

### 3.2 共通イベント包み（WorkEvent）

子（手）の報告は`report`ツールが受け取り、ホストがセッションとRunを束縛して親へ渡す。UI向けの`WorkEvent`は状態とRunの変化だけを運ぶ。

```rust
pub struct WorkEvent {
    pub work_id: String,
    pub run_id: String,
    pub kind: WorkEventKind,
    pub detail: Detail,
}

pub enum WorkEventKind {
    StatusChanged,
    RunStarted,
    RunFinished,
}

pub enum Detail {
    StatusChanged(WorkStatusChanged),
    RunStarted(WorkRunStarted),
    RunFinished(WorkRunFinished),
}

/// `report` ツール。`session_id`と`run_id`はホストが実行中のRunから束縛する
pub struct ReportArgs {
    /// activity | message | completed | failed
    pub kind: String,
    pub content: String,
}
```

`WorkEvent.kind`は`oneof detail`の種別と一致させる。

---

## 4. 進捗途絶検知と割り込み処理

作業が長時間に及ぶ場合、「手」は困ったときだけでなく、数十秒〜数分間隔で定期的に進捗（`report(kind=activity)` または `report(kind=message)`）を報告する。

### 4.1 途絶検知と親へのプロンプト注入
- 子からの活動報告が閾値（デフォルト: **90秒**）を超えて途絶した場合、ホストは親（頭）へ**コンテキスト注入イベント（Prompt Injection / `DeliveryMode::Inform`）**を行う。
- **通知と返答要求の区別**: `DeliveryMode::Inform` は返答を強制しない非ブロッキング通知である。途絶通知は親のコンテキストに事実情報として追加され、親が対話中であれば次ターンの思考材料となる。
- **1回のみの警告（`stall_warned`）**:
  - `now - last_activity_at > 90s` かつ `stall_warned == false` のときに限り、ホストは通知を1回だけ送出し、`stall_warned = true` に設定する。これにより、親エージェントへの通知スパムを防止する。
  - 子から新たな活動報告が到着した時点で `last_activity_at` は更新され、`stall_warned = false` にリセットされる。
  ```text
  [System Notification: 作業「手」(run_id: run_123) からの報告が90秒間途絶しています。現在の状況を確認してください。]
  ```
- 親（頭）はこの通知を受けて、ユーザーに「ちょっと様子を見てみるね」と声をかけたり、子に進捗確認メッセージを送出できる。

### 4.2 実行中の追加入力の配送

`send_work`は常にワークエージェントへ`DeliveryMode::Queue`で入力を渡す。実行中なら現在のターンに続けて届き、実行がなければ同じ`work_id`で新しいRunを開始する。注入・保留・拒否の分岐は持たない。

同じ`request_key`の再送は、内容が同じなら元の受付（`request_id`）を返し、内容が異なれば競合として拒否する。受付レシートはワークエージェントの会話ログ（`ene.work_accepted` Aside）に残すため、再起動後も照合できる。

---

## 5. 安全な中断プロトコル（Cancellation Protocol）

### 5.1 中断シーケンス

```text
[ユーザー / 親] ──→ stop_work(work_id) / StopWork(work_id)
                            │
                            ▼
                  ホスト ──→ ワークエージェントへ StopTurn
                            │
                            ▼
                  同じパスで run を Cancelled に確定
                  ・pending_request_id を消す
                  ・セッションを Idle に戻す
                  ・親へ中断報告を Queue で渡す
```

1. **停止は即時に確定する**: `stop_work`と`StopWork`は同じ経路を通る。ホストはワークエージェントへ`StopTurn`を送り、その場でrunを`Cancelled`として保存し、親へ「作業を中断しました」（理由があればその理由）を渡す。エージェントのターンが片付くのを待たない。
2. **待機も強制終了もしない**: `Cancelling`の待機状態、30秒の猶予、プロセス群の強制終了、停止未確認の区分は持たない。コマンド実行子プロセスの管理は`process-wrap`に閉じる。
3. **二重停止とクローズ**: 停止済みのrunへの再度の`stop_work`は何もしない。`CloseWork`は停止の後にセッションを`Closed`にし、ワークエージェントを引退させる。
4. **過去Runの遅延イベントの扱い**: Run Aの中断後に開始されたRun Bの実行中に、遅れてRun Aの報告が届いても、**Run Bのアクティブ状態を破壊・上書きしない**。Run Aの報告はRun A自身の監査記録としてのみ残す。

### 5.2 外部副作用の現実的な扱い（Undoの限界の明示）
- ホストは中断時にローカルの後片付けを保証しない。ワークエージェントのターン停止だけを行い、成果物の巻き戻しは約束しない。
- **「送信済みメール」「外部APIで作成したクラウド資源」「リモートGit Push」などの物理的に巻き戻せない外部操作**については、「安全にUndoできる」と偽らない。親（頭）はユーザーへ正確な顛末を報告する。成否不明な外部操作は自動再送しない。

---

## 6. 親（頭）と子（手）が利用する操作ツール群

主体・権限はホストがセッション文脈に基づき自動束縛する。

### 6.1 親（頭）向け操作ツール（`Rig` ツール）

| ツール | 引数 | 役割 |
| :--- | :--- | :--- |
| **`start_work`** | `instruction: String`<br>`work_id: Option<String>` | 「手」に実務を委託。`work_id` 未指定時は新規WorkSessionを作成し、指定時は既存セッションで次のRunを開始。音声入力の文字起こしIDを委託キーとし、同じ発話では最初に実行したコンパニオンだけが着手する |
| **`send_work`** | `work_id: String`<br>`text: String` | 「手」に対する追加指示や方針変更を送信。常にキューイングし、実行中のRunがなければ同じセッションで開始する |
| **`stop_work`** | `work_id: String`<br>`reason: Option<String>` | 実行中のRunを停止し、その場で`Cancelled`として記録する（アクティブRunをホストが自動特定） |

### 6.2 子（手）向け報告ツール（内蔵Rig / カスタムエージェント用）

| ツール | 引数 | 役割 |
| :--- | :--- | :--- |
| **`report`** | `kind: String`（`activity` / `message` / `completed` / `failed`）<br>`content: String` | ホストおよび親へ進捗、または完了/失敗を通知。**`session_id` と `run_id` はホスト実行環境がコンテキストから自動束縛**し、LLMに引数として書かせない。報告は状態の正本である実行中Runに照合し、終了済みのRunからの報告は状態を変えない |
| **`device_request`** | `capability: String`<br>`input: Object` | アクティブな端末の機能を実行する。ホストが許可済みcapabilityと判定パイプラインを照合する |

`device_request`の対象端末はLLMが引数で指定せず、現在アクティブなクライアントに対して実行する。端末能力の実行にも判定パイプラインを通す。

---

## 7. Connect-RPC サービス定義 (`work.proto`)

### 7.1 クライアント向けRPCと親（頭）内部ツールの責務分離
- **通常のユーザー対話（親エージェント経由）**:
  ユーザーはチャット（`ChatService.SendMessage`）や音声入力（`VoiceService`）を通じて親コンパニオンへ実務や調査を依頼する。親エージェント（頭）が文脈を解釈し、内部ツール（`start_work`, `send_work`, `stop_work`）を実行して自律的に進行する。クライアント自身が親のオーケストレーションを肩代わりすることはない。
- **Connect-RPC `WorkService` の用途区分**:
  1. **UI一覧表示・状態取得（通常利用）**:
     - `ListWorks`: 実行中および直近の WorkSession 一覧取得（UIサイドバーやタスクパネル表示用）。
     - `GetWork`: 特定 WorkSession の詳細状態（`WorkSession`）、実行履歴（`runs`）、`WaitingInput`中の承認要求（`pending_request_id`）の取得。再接続時に未決の承認要求を再取得できる。
     - **単一イベントストリームへの統合**: 個別のストリーム（`SubscribeWorkEvents`）は設けず、リアルタイムの作業状態とRunのイベントはすべて `HostService.Subscribe`（単一の `HostEvent` ストリーム）内の `WorkEvent` として集約配信される。
  2. **ユーザー主導の緊急直接停止（即時制御）**:
     - `StopWork`: ユーザーがクライアントUI上の「作業中断」ボタンを押下した際、親LLMの思考待ちを介さず即座に停止要求を発行する。
     - `CloseWork`: 完了・中断済みセッションの明示的クローズ。
  3. **開発・検証・自動化直接呼び出し（特権・開発用）**:
     - `SendWork`: 親コンパニオンの会話文脈を介さず、開発者コンソールやスクリプト連携等で直接指示を投入して検証するためのルート。冪等性キー（`request_key`）による重複実行防止を備える。

> [!NOTE]
> **カノニカル RPC 定義**:
> `WorkService` の完全な Protobuf サービス定義は、[ホストAPI設計 カノニカル Protobuf 定義](../api.md#7-カノニカル-protobuf-定義-hostproto) を正本とする。
> プロトコル上の`WorkEvent`はUI向けの状態とRunのイベントを`WorkEventKind`と型付き`oneof detail`で表す。`report`が受けた進捗・完了・失敗はホスト内の報告パイプラインで処理し、必要に応じて親へのメッセージや上記のイベントへ変換する。

---

## 8. 検証条件と受け入れ基準 (Alpha Work Invariants)

| 不変条件 | 検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **同一ペルソナ一人称対話** | 「手」の作業報告を受けた親の会話出力を検査 | 「裏で作業者が〜」等の第三者表現が排除され、一人称（「私が今〜したよ」）で報告されること |
| **Run完了の独立性** | Run A中断後にRun Bを開始し、遅れてRun Aの完了イベントをシミュレート到着 | Run Bのアクティブ状態が破壊・上書きされず、遅延到着したRun AのイベントはRun A自身の過去監査記録へ安全に反映されること |
| **停止の即時確定** | 実行中Runに対して`stop_work`または`StopWork`を発行 | エージェントのターン終了を待たずにrunが`Cancelled`として保存され、セッションが`Idle`に戻り、親へ中断報告が届くこと |
| **受付の冪等性** | 同じ`request_key`と同じ内容で`send_work`を再送 | 同じ`request_id`が返り、ワークエージェントへ二重配送されないこと |
| **再起動後の復元** | 実行中Runを残してホストを再起動 | 開いているセッションごとにワークエージェントが1体だけ残り、途絶の計時が再開されること |
| **非可逆操作の誠実な報告** | 外部送信済み操作を含むRunを中断 | 「Undo完了」と偽らず、巻き戻せない外部操作をユーザーへ報告すること |
| **途絶検知プロンプト注入** | 子の活動報告を90秒間意図的に停止 | 親エージェントへ進捗確認プロンプトが1回だけ注入され、親が状況確認を試みること |
