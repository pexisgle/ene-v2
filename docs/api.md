# ホストAPI設計

状態：採用設計。本仕様書（§7 カノニカル Protobuf 定義）および [全体アーキテクチャ](architecture.md) を正本（Canonical Source of Truth）とします。既存クレート（`crates/ene-rpc/proto/host.proto`）は本設計に基づくクリーンルーム実装によって更新されます。Bearer認証付きConnect RPCと購読（`Subscribe`）・状態再取得を基本とします。将来の接続アクセス制御の委譲については [ホスト接続の設計](design/host-access.md) を参照します。

## 1. 共通の意味

IDはホスト内で安定した不透明な値とする。親、バックエンド設定、作業session、run、入力、質問、操作受付、通知カーソルを混同しない。ホスト／端末／外部サービスの所在を明示し、ある環境のパスが別端末でも開けると仮定しない。

現行実装の状態変更は、認証主体・操作・対象・要求キー・内容を束縛する。将来の外部アクセス制御への移行では、端末IDが当然に得られるとは仮定せず、要求キーの重複排除・操作対象の検証を接続方式に依存せず保持する。同じ要求の再送は元の受付結果を返し、同じキーで異なる内容なら競合とする。更新はrevisionを検証する。秘密情報を再送比較用の本文やログへ複製しない。

受付は完了ではない。長時間処理の受付には追跡IDと対象IDを返し、配送・実行・終了は取得と通知で確認できる。停止も受付と停止完了・不明を分ける。APIの接続が切れても、受理済みの保存や作業を無言に取り消さない。

会話の受理は`request_id`を返し、Rigが入力を受け付け、そのフレームのセッションログ書込みが成功した後に返す。保存できない入力は受理として返さない。同じ`request_key`の再送は同じ`request_id`を返し、配送を確認できなかった受理は成功として返さない。再起動後に未確定の会話レシートが残った場合は、同じ`request_key`の再送時にRigの保存済み会話・受信箱・ターン記録を`request_id`で照合する。対応する入力またはターン記録を持つ有効なレシートがあれば元の`request_id`を返して再配送せず、同じキーで内容が異なれば競合とする。`ene.accepted` Asideだけが残り、対応する入力・ターン記録を確認できない孤立レシートは未確定として除外し、新しい受理を試みる。孤立レシートのハッシュは内容束縛に使わない。新しい受理の保存に失敗した場合は成功として返さない。ユーザー発言と返答は`MessageCreated`、実行の開始と終了は`TurnEvent`（`streaming` / `answered` / `stopped` / `failed`）、文字片は`MessageDelta`（`stream_id`）で届く。確定本文は`MessageCreated`と`GetHistory`/`GetMessage`で取得し、文字片を確定内容として扱わない。停止は要求の受付を返し、停止完了は`TurnEvent`で確認する。失敗したターンが入力を会話から取り出した場合は`RecalledInput`で本文を返し、クライアントは入力欄へ戻せる。会話メッセージとしては保存しない。

知らない状態、壊れた応答、未知の終了理由は正常完了にしない。未対応、許可不足、競合、通信失敗、保存失敗、結果不明を区別する。エラーの詳細に資格情報・非公開記憶を含めない。

## 2. クライアントから操作できる範囲

| 領域 | 操作の意味 |
| --- | --- |
| 会話 | 発言の受付、追加入力、履歴・メッセージ取得、実行イベントと文字片の購読、生成の停止 |
| キャラクター | 初期プロファイルの取得・編集と現在の心の取得（`GetMind` / `UpdateMindProfile`）。作者設定と交流で育った状態を分ける |
| 記憶 | 親の`mind_recall` / `mind_read` / `mind_update`、ホストの自動整理・自動想起、`ControlMemory`（forget/remember/share/unshare）。クライアント向けの記憶一覧UIは未実装 |
| 作業 | WorkSession一覧・状態取得（`ListWorks`）、未決質問への回答（`AnswerInquiry`）、進捗イベント購読（`HostService.Subscribe` の `WorkEvent`）、UIからの緊急直接停止（`StopWork`）。通常の作業指示は親の会話を通じて行われる |
| 音声・身体 | マイク入力ストリーム（`StreamAudioInput`）、合成音声出力ストリーム（`SubscribeAudioOutput`）、クライアント再生進捗報告（`ReportPlaybackStatus`）。音声再生デバイスはデスクトップクライアントが単一所有する |
| 接続・ツール | プロバイダー・バックエンド・ツール一覧、能力、設定、認証の開始・状態・解除 |
| 権限・承認 | ツール利用許可ルール（パス・コマンド等）やセキュリティ設定の取得・更新、未決の承認要求（`ApprovalRequest`）の取得とユーザー回答（承認/拒否） |
| 通知 | 会話・心・作業・許可・ホスト状態の変更と再同期 |

日常の記憶訂正・忘却は対話で行える。調査用の一覧を編集しなければ記憶が育たない構成にしない。開発者向けの読取操作と、他のコンパニオンや外部プラグインに許す能力は別にする。

クライアントに親のオーケストレーションを実装しない。通常の作業依頼・追加指示・相談回答は親へ出所付き入力として届く。特定の作業へ向けたユーザーの入力も、親が文脈と対象を理解できる形にする。作業バックエンドの検証用直接呼び出し（`WorkService.StartWork` / `SendWork` 等）は別能力（開発・自動化用）として扱い、通常の会話と混ぜない。

## 3. 公開状態

### 心

`GetMind`は`MindView`を返し、`UpdateMindProfile`は著者プロファイルの追加材料を版検証付きで更新する。核は既存の`UpdateCompanion.mind`で編集する。MindViewはcompanion_id、核と側面・会話例・背景・表現語彙、profile revision、現在の心のrevisionと更新時刻、現在感情（短い説明・対象・理由・表現信号）、関係の理解、経験してきた傾向、気にかけていることを返す。感情・関係・傾向・懸念はキャラクター固有のJSONで、全員共通の数値尺度を強制しない。全私的記憶や内部推論全文を、アバター表示のためだけに送らない。現在状態と特定発話の表現指示を区別する。

MindViewは索引状態（FTS/vectorの処理待ち、埋め込みmodel・呼び出し数・トークン数）と整理状態（last_sequence、next_offset、skipped、jobs、model_calls、input_chars、error/unconfigured）も返す。索引・整理が未完了なことと、該当記憶がないことを混同しない。

`ControlMemory`は所有者だけが記憶の`importance`（想起優先度: 0〜10、忘却時は重要度引き下げ）と`audience`（share/unshare）を変更できる。忘却は物理削除や二値の除外フラグではなく、想起優先度（`importance`）の引き下げとして扱い、想起スコアを自然に低下させる（二値の`include_forgotten`フラグは設けない）。共有記憶は他コンパニオンの想起対象になるが、所有者の私的な入力出所は共有先へ渡さない。共有解除後はcanonicalのaudience再検証で、古い索引から返さない。同じ値への操作はrevisionを増やさない。

変更はSQLiteコミット後にだけ`MindUpdated`で通知し、revisionを含める。クライアントは古いrevisionの通知で現在状態を戻さない。`mind_update`は親のRig実行内のツールであり、クライアントRPCではない。所有者・操作ID・入力出所はホストが束縛し、著者設定・権限・他コンパニオンの記憶は変更できない。同じ操作ID・同じ内容の再試行は元の結果、異なる内容は競合、古いrevisionは競合とする。変更なしも成功である。

MemoryViewは本文、短い説明、出所・取得方法、時刻、共有範囲、訂正関係、想起の扱い、revisionを返す。取得側の権限に応じて情報を制限する。「忘れて」に対して物理削除済みのフラグや成功文を返さない。検索の能力不足・索引更新待ち・外部検索の失敗も、該当なしと区別できる。

### 作業

WorkViewは親、WorkSession、進行中RunInfo、未決質問（`pending_inquiries`）を持つ。runには配送状態、実行状態、終了理由と失敗・不明の情報がある。
未決質問（`WorkInquiry`）への回答経路は2通り存在する：
1. **対話仲介経路（通常）**: 子エージェントからの質問を親エージェントがユーザーに相談し、ユーザーの返答を受けた親が `send_work` ツール（`request_id` に質問IDを束縛）で回答する。
2. **UI直接回答経路（クライアントUI）**: クライアントが `WorkView.pending_inquiries` から質問カードを表示し、ユーザーの選択・入力を `WorkService.AnswerInquiry(work_id, request_id, answer)` RPCで直接回答する。

権限承認要求（`ApprovalRequest`）は `SettingsService.SubmitApproval` で回答し、通常の質問（`WorkInquiry`）と明確に区別する。

### 知覚（目）と動的Watcher

クライアント側知覚センサー（OSウィンドウフック・サンプリング）は `ClientService.ReportPerception` RPCを通じてホストへ知覚イベント（`PerceptionEvent`）を送信する。
ホストはこれを受信して簡潔な要約（`PerceptionSummary`）を生成し、親エージェントの動的文脈（Dynamic Tail）へインメモリ注入する。
親エージェントは以下の正規ツールを通じて知覚センサーに動的監視ルールを設定できる：

- `set_eye_watch(target_pattern: String, match_kind: String, notify_on: String, enabled: bool)`:
  - `target_pattern`: 監視対象のウィンドウタイトルまたはアプリ識別子の部分一致・正規表現パターン（例: `"GitHub"`, `".*\\.rs"`）
  - `match_kind`: `"title"`（ウィンドウタイトル）または `"app_id"`（プロセス/アプリケーション名）
  - `notify_on`: 発火契機（`"focus"`: アクティブ化時、`"blur"`: 非アクティブ化時、`"title_change"`: タイトル変更時）
  - `enabled`: ルールの有効化（`true`）または解除（`false`）
  合致時は定期サンプリングを待たずに即時優先割り込み（Priority Interrupt）が親エージェントへ通知される。
- **ホスト ➔ クライアントへの監視ルール伝達経路**:
  親エージェントが `set_eye_watch` を実行した際、ホストは接続中のデスクトップクライアントに対し、`ClientService.DeviceStream` 上の `DeviceOperation { capability: "eye.watch", input: <ルールJSON> }` を送出して監視条件を設定・解除する。クライアントはこれを受けてOSネイティブウィンドウフックのフィルタ条件を動的更新する。

### 能力

機能の存在と、設定・認証・許可が整って実行可能かを区別する。入力形式、途中介入、再開、モデル設定、MCP、停止の実効範囲などを実装ごとに返す。利用できない機能の理由がクライアントと親の双方に分かるようにする。

## 4. 通知と再接続

通知にはホストの起動epochと順序を持たせ、対象IDとrevisionを含める。保存済み状態を正本とし、通知から取りこぼした結果も再取得できる。会話の文字片、意味のある子の説明、心の変更、runの終端は区別する。

購読開始と状態スナップショットの境界を定め、取得中の変更を取りこぼさない。遅延・溢れ・不明なカーソルでは再同期を要求し、重い通知送信が停止制御や保存を塞がない。成功だけでなく失敗・取消・不明も反映する。

文字片の再送と確定本文の取得、進捗の間引きと質問・終了の保持を分ける。クライアントは古いrevisionの通知で現在状態を戻さない。別端末へのファイル同期やプッシュ通知の完成を、この通知契約だけで保証しない。

## 5. 停止・取消

会話の生成停止、指定runの停止、コンパニオン配下の実行停止、ホスト終了を明確な対象で区別する。自然言語の「それを止めて」は親が対象を理解するが、UIの明示的停止はモデルを待たない制御経路を使う。

権限解除は許可のrevisionと実行側の適用状態を返す。ホスト管理下の新しい操作を禁止し、外部ハーネスの取消不能な処理や送信済みデータの扱いを隠さない。クライアント切断は停止要求ではない。

## 6. wire契約とサービス構成

wire契約は以下の6つのConnect-RPCサービスで構成される：
- **`HostService`**: システム状態スナップショット、単一イベントストリーム（`Subscribe`）、コンパニオン・心・記憶の取得・操作（`ListMemories` 含む）。
- **`ChatService`**: 会話メッセージの送信（`SendMessage`）、停止（`Stop`）、会話履歴取得（`GetHistory`, `GetMessage`）。
- **`SettingsService`**: 設定、プロバイダー/モデル一覧、ポリシー・承認ルール（`PolicyRule`）、未決承認要求の確認と回答（`SubmitApproval`）。
- **`WorkService`**: 作業セッションの一覧（`ListWorks`）、詳細取得（`GetWork`）、直接指示（`SendWork`）、緊急停止（`StopWork`）、クローズ（`CloseWork`）。
- **`VoiceService`**: 双方向マイク音声入力ストリーム（`StreamAudioInput` / 要HTTP/2）、音声ストリーム購読（`SubscribeAudioOutput`）、再生完了・進捗報告（`ReportPlaybackStatus`）。
- **`ClientService`**: クライアント端末の登録、有効化、端末操作ストリーム。

---

## 7. カノニカル Protobuf 定義 (`host.proto`)

過剰なマトリクス型 Grant（purpose/destination/data_kind/Consent/Availability）を全廃し、純粋な `ApprovalRequest` / `PolicyRule`、7つの役割モデル割当、および音声サービスを統合した正本スキーマ。

```protobuf
syntax = "proto3";
package ene.host.v1;

message Empty {}
message Id { string id = 1; }
message StringList { repeated string values = 1; }
message Cursor { string epoch = 1; uint64 seq = 2; }
message Failure {
  string code = 1;
  string message = 2;
  reserved 3;
  reserved "details";
}

message HostHealth {
  bool ready = 1;
  bool recovery_required = 2;
  bool stopping = 3;
  reserved 4;
  reserved "credential_cleanup_pending";
}

message HostCapabilities {
  string limits = 1; // JSON object text
  repeated string features = 2;
  repeated string adapters = 3;
  string transport = 4;
}

message Snapshot {
  string server_version = 1;
  HostHealth health = 2;
  HostCapabilities capabilities = 3;
  Settings settings = 4;
  repeated Companion companions = 5;
  repeated WorkSession work_sessions = 6;
  optional string active_companion_id = 7;
  repeated ApprovalRequest pending_approvals = 8;
  repeated Client clients = 9;
  Cursor cursor = 10;
  repeated PolicyRule policy_rules = 11;
}

message SubscribeRequest {
  string client_id = 1;
  optional Cursor cursor = 2;
}

message Notification {
  Cursor cursor = 1;
  string kind = 2;
  optional string client_id = 17;     // 特定クライアント宛て通知の場合（省略時は全端末同報）
  optional string companion_id = 18;  // コンパニオン関連通知の場合
  oneof event {
    HostHealth health = 3;
    Companion companion = 4;
    MindUpdated mind_updated = 5;
    MessageCreated message_created = 6;
    MessageDelta message_delta = 7;
    TurnEvent turn_event = 8;
    WorkEvent work_event = 9;
    ApprovalRequested approval_requested = 10;
    PolicyRulesUpdated policy_rules_updated = 11;
    SettingsUpdated settings_updated = 12;
    ClientUpdated client_updated = 13;
    DeviceOperation device_operation = 14;
    DeviceOperationResult device_operation_result = 15;
    HostEvent generic = 16;
    Snapshot initial_state = 19;
    ResyncRequired resync_required = 20;
  }
}

message ResyncRequired {
  string reason = 1;
  Cursor target_cursor = 2;
}

// Model Assignments --------------------------------------------------------

message ModelAssignment { string connection_id = 1; string model_id = 2; }
message ModelAssignments {
  ModelAssignment companion = 1;              // 頭（対話・ロールプレイ）: 必須
  ModelAssignment task = 2;                   // 手（実務タスク）: 必須
  optional ModelAssignment perception = 3;     // 目（知覚要約、未指定時は companion フォールバック）
  optional ModelAssignment compaction = 4;     // Compaction要約（未指定時は companion フォールバック）
  optional ModelAssignment consolidation = 5;  // 記憶整理（未指定時は companion フォールバック）
  optional ModelAssignment decision = 6;       // 権限判定（未指定時は task フォールバック）
  optional ModelAssignment embedding = 7;      // 埋め込みモデル
}

message Companion {
  string id = 1;
  string name = 2;
  string mind = 3;
  string body = 4;  // JSON object text
  string voice = 5; // JSON object text
  ModelAssignments assignments = 6;
  uint64 revision = 7;
  string created_at = 8;
  string updated_at = 9;
  optional string persona = 10;
  optional string voice_identifier = 11;
}
message Companions { repeated Companion items = 1; }
message AssignmentPatch {
  oneof change {
    ModelAssignment set = 1;
    Empty clear = 2;
  }
}
message CreateCompanionRequest {
  string name = 1;
  string mind = 2;
  string body = 3;  // JSON object text
  string voice = 4; // JSON object text
  ModelAssignments assignments = 5;
  MindProfile profile = 6;
  optional string persona = 7;
  optional string voice_identifier = 8;
}
message UpdateCompanionRequest {
  string id = 1;
  uint64 revision = 2;
  optional string name = 3;
  optional string mind = 4;
  optional string body = 5;
  optional string voice = 6;
  AssignmentPatch companion_assignment = 7;
  AssignmentPatch task_assignment = 8;
  AssignmentPatch perception_assignment = 9;
  AssignmentPatch compaction_assignment = 10;
  AssignmentPatch consolidation_assignment = 11;
  AssignmentPatch decision_assignment = 12;
  AssignmentPatch embedding_assignment = 13;
  optional string persona = 14;
  optional string voice_identifier = 15;
}

// Mind and Memories --------------------------------------------------------

message MindProfile {
  string core = 1;
  uint64 revision = 2;
  repeated string facets = 3;
  repeated string examples = 4;
  repeated string background = 5;
  repeated string expression = 6;
}
message MindState {
  uint64 revision = 1;
  string emotion = 2;       // JSON text
  string relationships = 3; // JSON array text
  string tendencies = 4;    // JSON array text
  string concerns = 5;      // JSON array text
  string updated_at = 6;
}
message MindIndexStatus {
  uint32 pending_fts_count = 1;
  uint32 pending_vector_count = 2;
  string embedding_model = 3;
  uint64 model_calls = 4;
  uint64 total_tokens = 5;
}

message MindConsolidationStatus {
  string last_processed_turn_id = 1;
  uint64 jobs_completed = 2;
  uint64 model_calls = 3;
  uint64 input_chars = 4;
  optional string last_error = 5;
}

message MindView {
  string companion_id = 1;
  MindProfile profile = 2;
  MindState state = 3;
  optional MindIndexStatus index = 4;
  optional MindConsolidationStatus consolidation = 5;
}
message MindUpdated {
  string companion_id = 1;
  uint64 profile_revision = 2;
  uint64 state_revision = 3;
  string updated_at = 4;
}
message UpdateMindProfileRequest {
  string companion_id = 1;
  uint64 expected_revision = 2;
  MindProfile profile = 3;
}
message MemoryControlRequest {
  string companion_id = 1;
  string memory_id = 2;
  // "forget" | "remember" | "share" | "unshare"
  string action = 3;
}
message MemoryControlResult {
  string memory_id = 1;
  uint32 importance = 2;
  repeated string audiences = 3;
  uint64 revision = 4;
}

message ListMemoriesRequest {
  string companion_id = 1;
  optional string cursor = 2;
  optional uint32 limit = 3;
}
message MemoryItem {
  string id = 1;
  string owner = 2;
  repeated string audiences = 3;
  string content = 4;
  uint32 importance = 5;
  optional string occurred_at = 6;
  string recorded_at = 7;
  optional string last_accessed_at = 8;
  uint64 revision = 9;
}
message Memories {
  repeated MemoryItem items = 1;
  optional string next_cursor = 2;
}

// Chat ---------------------------------------------------------------------

message SendMessageRequest {
  string companion_id = 1;
  string request_key = 2;
  string text = 3;
  string client_id = 4;
}
message Acceptance { string request_id = 1; }
message StopRequest { string companion_id = 1; }
message Message {
  string id = 1;
  string companion_id = 2;
  string work_id = 3;
  string role = 4; // "user" | "assistant" | "agent" | "context"
  string text = 5;
  string created_at = 7;
  uint64 sequence = 8;
  optional string request_id = 9;
  string author = 12;
}
message MessageCreated { Message message = 1; }
message MessageDelta {
  string companion_id = 1;
  string stream_id = 2;
  string delta = 3;
}
message TurnEvent {
  string companion_id = 1;
  string turn_id = 2;
  string status = 3; // "streaming" | "answered" | "stopped" | "failed"
  optional Failure failure = 4;
}
message HistoryRequest {
  string companion_id = 1;
  optional string cursor = 2;
  optional uint32 limit = 3;
  optional bool include_context = 4;
}
message History { repeated Message items = 1; }
message MessageRequest { string companion_id = 1; string message_id = 2; }

// Work ---------------------------------------------------------------------

message WorkSession {
  string id = 1;
  string companion_id = 2;
  string title = 3;
  string status = 4; // "idle" | "active" | "closed"
  string created_at = 5;
  string updated_at = 6;
}
message RunInfo {
  string run_id = 1;
  string work_id = 2;
  string state = 3;
  Failure failure = 4;
}
message WorkInquiry {
  string request_id = 1;
  string prompt = 2;
  repeated string choices = 3;
}
message WorkView {
  WorkSession session = 1;
  repeated RunInfo runs = 2;
  repeated WorkInquiry pending_inquiries = 3;
}
message WorkViews { repeated WorkView items = 1; }
message ListWorksRequest { string companion_id = 1; }
message WorkRequest { string work_id = 1; }
message StartWorkRequest {
  string companion_id = 1;
  string title = 2;
  string instruction = 3;
  string client_id = 4;
}
message SendWorkRequest {
  string work_id = 1;
  string request_key = 2;
  string text = 3;
  string client_id = 4;
  optional string request_id = 5; // 質問への回答束縛用 (会話ツール経由等)
}
message AnswerInquiryRequest {
  string work_id = 1;
  string request_id = 2; // WorkInquiry.request_id
  string answer = 3;     // ユーザーの回答テキスト
  string client_id = 4;
}
message WorkAcceptance { string request_id = 1; }
message StopWorkRequest { string work_id = 1; }
message WorkEvent {
  string work_id = 1;
  string run_id = 2;
  string kind = 3; // "status_changed" | "run_started" | "run_finished" | "inquiry_created"
  string detail = 4; // JSON object text
}

// Permissions and Approvals ------------------------------------------------

message ApprovalRequest {
  string request_id = 1;
  string session_id = 2;
  string run_id = 3;
  bytes action_payload_hash = 4;
  string tool_name = 5;
  string normalized_arguments = 6;
  string status = 7; // "pending" | "approved" | "denied" | "expired"
  string created_at = 8;
}
message PendingApprovals { repeated ApprovalRequest items = 1; }
message SubmitApprovalRequest {
  string request_id = 1;
  string decision = 2; // "approve" | "always_approve" | "deny"
  optional string reason = 3;
}
message SubmitApprovalResponse {
  bool acknowledged = 1;
  string status = 2;
}
message ApprovalRequested { ApprovalRequest request = 1; }

message PolicyRule {
  string id = 1;
  string tool_name = 2;
  string pattern = 3;
  string effect = 4; // "allow" | "deny" | "confirm"
  string created_at = 5;
}
message PolicyRules { repeated PolicyRule items = 1; }
message UpdatePolicyRulesRequest {
  repeated PolicyRule add_or_update = 1;
  repeated string remove_ids = 2;
}
message PolicyRulesUpdated { repeated PolicyRule rules = 1; }

// Settings & Connections ---------------------------------------------------

message Connection {
  string id = 1;
  string kind = 2;
  string adapter = 3;
  string endpoint = 4;
  string options = 5; // JSON object text
  string credential_state = 6;
  string last_check = 8; // JSON object text
  bool enabled = 9;
}
message Settings {
  uint64 revision = 1;
  repeated Connection connections = 2;
  string updated_at = 3;
}
message ConnectionInput {
  string id = 1;
  bool enabled = 2;
  string kind = 3;
  string adapter = 4;
  string endpoint = 5;
  string options = 6; // JSON object text
}
message UpdateSettingsRequest {
  uint64 expected_revision = 1;
  repeated ConnectionInput connections = 2;
}
message SettingsUpdated { Settings settings = 1; }

message CredentialRequest {
  string id = 1;
  optional string secret = 2;
  optional string source = 3;
}
message CredentialResult { string credential_state = 1; }
message CheckRequest {
  string id = 1;
  string request_key = 2;
  string companion_id = 3;
  optional string model_id = 4;
}
message CheckResult {
  string request_id = 1;
  string capabilities = 2; // JSON object text
  bool success = 3;
  string model_id = 4;
}
message FieldSource {
  string key = 1;
  string value = 2;
}
message ModelMetadata {
  string source = 1;
  string scope = 2;
  optional uint64 context_window = 3;
  optional uint64 max_input_tokens = 4;
  optional uint64 max_output_tokens = 5;
  StringList input_modalities = 6;
  StringList output_modalities = 7;
  StringList supported_parameters = 8;
  repeated FieldSource field_sources = 9;
}
message Model {
  string id = 1;
  string name = 2;
  ModelMetadata metadata = 3;
}
message Models { repeated Model items = 1; }
message ModelsRequest {
  string id = 1;
  optional string companion_id = 2;
}
message SettingField {
  string key = 1;
  string label = 2;
  string kind = 3;
  bool required = 4;
  repeated string choices = 5;
}
message ProviderCapabilities {
  bool streaming_text = 1;
  bool model_list = 2;
}
message Provider {
  string id = 1;
  string name = 2;
  string kind = 3;
  string endpoint = 4;
  string options = 5; // JSON object text
  repeated SettingField settings = 6;
  bool requires_credentials = 7;
  optional string environment_variable = 8;
}
message Providers { repeated Provider items = 1; }

// Voice Service ------------------------------------------------------------

message AudioInputChunk {
  string client_id = 1;
  bytes pcm_data = 2; // 24kHz / 16kHz 16-bit mono PCM
  uint32 sample_rate = 3;
  bool is_companion_speaking = 4;
}
message TranscriptionEvent {
  string transcription_id = 1;
  string text = 2;
  bool is_final = 3;
  bool is_speaking = 4;
}
message SubscribeAudioRequest { string client_id = 1; }
message AudioOutputEvent {
  oneof event {
    AudioChunkEnvelope chunk = 1;
    FlushControl flush = 2;
    SentenceSkipped skipped = 3;
    UtteranceFinished finished = 4;
  }
}
message AudioChunkEnvelope {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 sequence_no = 3;
  bytes pcm_data = 4;
  uint32 sample_rate = 5;
  bool is_final_chunk = 6;
}
message FlushControl {
  string utterance_id = 1;
  uint64 flush_generation_id = 2;
  string reason = 3;
}
message SentenceSkipped {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 sequence_no = 3;
  string reason = 4;
}
message UtteranceFinished {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 total_sentences = 3;
}
message PlaybackStatusReport {
  string utterance_id = 1;
  uint64 generation_id = 2;
  enum Status {
    STATUS_UNSPECIFIED = 0;
    PLAYBACK_COMPLETED = 1;
    PLAYBACK_INTERRUPTED = 2;
    PLAYBACK_FAILED = 3;
  }
  Status status = 3;
  uint32 completed_sentence_count = 4;
  repeated uint32 played_sequence_nos = 5;
  repeated uint32 skipped_sequence_nos = 6;
  uint64 completed_pcm_bytes = 7;
  string client_id = 8;
}
message ReportPlaybackStatusResponse { bool acknowledged = 1; }

// Client Service -----------------------------------------------------------

message Client {
  string id = 1;
  string name = 2;
  string platform = 3;
  repeated string capabilities = 4;
  repeated string granted_capabilities = 5;
  uint64 revision = 6;
  string last_seen = 7;
  bool connected = 8;
  repeated string active_companions = 9;
}
message Clients { repeated Client items = 1; }
message RegisterClientRequest {
  string id = 1;
  string name = 2;
  string platform = 3;
  repeated string capabilities = 4;
  uint64 expected_revision = 5;
}
message SetCapabilitiesRequest {
  string client_id = 1;
  repeated string capabilities = 2;
  repeated string granted_capabilities = 3;
  uint64 revision = 4;
}
message ActivationRequest {
  string companion_id = 1;
  string client_id = 2;
  uint64 expected_revision = 3;
  bool release = 4;
}
message Activation {
  string companion_id = 1;
  optional string client_id = 2;
  uint64 revision = 3;
}
message ClientUpdated { Client client = 1; }
message DeviceOperation {
  string operation_id = 1;
  string capability = 2;
  string input = 3; // JSON object text
  string subscription_id = 4;
}
message DeviceReady { string subscription_id = 1; }
message DeviceEvent {
  oneof event {
    DeviceReady ready = 1;
    DeviceOperation operation = 2;
  }
}
message DeviceOperationResult {
  string client_id = 1;
  string operation_id = 2;
  bool success = 3;
  string output = 4; // JSON object text
  optional Failure failure = 5;
}
message DeviceOperationState {
  string operation_id = 1;
  string client_id = 2;
  string status = 3;
  optional DeviceOperationResult result = 4;
}
message HostEvent {
  string kind = 1;
  string payload = 2;
}

// Perception (Eyes) --------------------------------------------------------

message PerceptionEvent {
  string client_id = 1;
  string timestamp = 2;
  oneof event {
    ActiveWindowChanged active_window = 3;
    UserActivityState user_activity = 4;
    PerceptionSummary perception_summary = 5;
  }
}

message ActiveWindowChanged {
  string title = 1;
  string app_id = 2;
  optional string url = 3;
}

message UserActivityState {
  bool is_active = 1;
  uint64 idle_seconds = 2;
}

message PerceptionSummary {
  string summary = 1;
  repeated string tags = 2;
}

// Service RPCs -------------------------------------------------------------

service HostService {
  rpc GetSnapshot(Empty) returns (Snapshot);
  rpc Subscribe(SubscribeRequest) returns (stream Notification);
  rpc ListCompanions(Empty) returns (Companions);
  rpc GetCompanion(Id) returns (Companion);
  rpc CreateCompanion(CreateCompanionRequest) returns (Companion);
  rpc UpdateCompanion(UpdateCompanionRequest) returns (Companion);
  rpc DeleteCompanion(Id) returns (Empty);
  rpc GetMind(Id) returns (MindView);
  rpc UpdateMindProfile(UpdateMindProfileRequest) returns (MindView);
  rpc ControlMemory(MemoryControlRequest) returns (MemoryControlResult);
  rpc ListMemories(ListMemoriesRequest) returns (Memories);
}

service ChatService {
  rpc SendMessage(SendMessageRequest) returns (Acceptance);
  rpc Stop(StopRequest) returns (Empty);
  rpc GetHistory(HistoryRequest) returns (History);
  rpc GetMessage(MessageRequest) returns (Message);
}

service SettingsService {
  rpc GetSettings(Empty) returns (Settings);
  rpc UpdateSettings(UpdateSettingsRequest) returns (Settings);
  rpc ListPendingApprovals(Empty) returns (PendingApprovals);
  rpc SubmitApproval(SubmitApprovalRequest) returns (SubmitApprovalResponse);
  rpc ListPolicyRules(Empty) returns (PolicyRules);
  rpc UpdatePolicyRules(UpdatePolicyRulesRequest) returns (PolicyRules);
  rpc ListProviders(Empty) returns (Providers);
  rpc ListModels(ModelsRequest) returns (Models);
  rpc RegisterCredentials(CredentialRequest) returns (CredentialResult);
  rpc RevokeCredentials(Id) returns (Empty);
  rpc CheckConnection(CheckRequest) returns (CheckResult);
}

service WorkService {
  rpc ListWorks(ListWorksRequest) returns (WorkViews);
  rpc GetWork(WorkRequest) returns (WorkView);
  rpc StartWork(StartWorkRequest) returns (WorkSession);
  rpc SendWork(SendWorkRequest) returns (WorkAcceptance);
  rpc AnswerInquiry(AnswerInquiryRequest) returns (WorkAcceptance);
  rpc StopWork(StopWorkRequest) returns (Empty);
  rpc CloseWork(WorkRequest) returns (Empty);
}

service VoiceService {
  rpc StreamAudioInput(stream AudioInputChunk) returns (stream TranscriptionEvent); // HTTP/2
  rpc SubscribeAudioOutput(SubscribeAudioRequest) returns (stream AudioOutputEvent);
  rpc ReportPlaybackStatus(PlaybackStatusReport) returns (ReportPlaybackStatusResponse);
}

service ClientService {
  rpc RegisterClient(RegisterClientRequest) returns (Client);
  rpc List(Empty) returns (Clients);
  rpc Activate(ActivationRequest) returns (Activation);
  rpc SetCapabilities(SetCapabilitiesRequest) returns (Client);
  rpc DeviceStream(Id) returns (stream DeviceEvent);
  rpc ReportPerception(PerceptionEvent) returns (Empty);
  rpc CompleteOperation(DeviceOperationResult) returns (DeviceOperationState);
  rpc GetOperation(Id) returns (DeviceOperationState);
}
```
