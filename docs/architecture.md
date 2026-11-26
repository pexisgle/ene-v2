# ene システムアーキテクチャ

状態：Alpha採用設計（2026-11-11策定）。[構想](vision.md)、[機能一覧](features/README.md)、[技術スタック](design/tech-stack.md)、[心と自動記憶](design/mind.md)、[作業相手API](design/work.md)、[コンテキスト圧縮](design/compaction.md)、[知覚・音声の共通仕様](design/voice-avatar.md)、[デスクトップクライアント](design/desktop-client.md)、[権限・Sandbox](design/permissions.md)、[プラグイン](design/plugins.md)を統合する全体構造を定義する。

---

## 1. 全体アーキテクチャ概要

eneは、デスクトップに常駐するキャラクターとの対話から、PC内の実務作業を自然に任せられるAIコンパニオンシステムである。
システムは **「頭」「手」「目」「体」の4つの直交する責務境界** に基づいて構築される。

```text
                     ┌───────────────────────────────────┐
                     │     ユーザー / クライアント環境      │
                     └───────┬───────────────────▲───────┘
        知覚イベント / 環境情報 │                   │ 音声出力 / アバター表示
                             ▼                   │
  ┌──────────────────────────────────────────────┴─────────────────────────────┐
  │ 【クライアント層】                                                          │
  │   ・表現（Body）: クライアント固有の描画・モーション・リップシンク         │
  │   ・知覚（Eyes）: プラットフォーム固有のイベント収集と知覚イベント送信     │
  │   ・音声入出力: クライアント所有のマイク・スピーカーと発話の再生           │
  │   ・能力宣言: RegisterClient / capability_specs による自己申告             │
  └──────────────────────────────┬─────────────────────────────────────────────┘
                                 │ Connect-RPC（能力宣言・DeviceStream・音声）
  ┌──────────────────────────────▼─────────────────────────────────────────────┐
  │ 【ホスト層（ene-host）】                                                    │
  │                                                                            │
  │   ┌────────────────────────────────────────────────────────────────────┐   │
  │   │ 「頭」（Brain / 親エージェント）：rig-ecs Bevy ECS 単一ループ       │   │
  │   │   ・エージェント実行・ツール反復（Rig 基盤: rig-ecs）              │   │
  │   │   ・会話ログ正本（rig-memory: DirStore 公式標準）                 │   │
  │   │   ・MCPツール動的登録（world.spawn_tool: ene設定所有）             │   │
  │   │   ・ECS制御: 先行TTS（Stage::Review StreamDelta）、承認保留（HoldOf） │   │
  │   │   ・ツール群: start_work, send_work, stop_work, device_request, noop │   │
  │   └───────────────┬────────────────────────────────────┬───────────────┘   │
  │                   │                                    │                   │
  │                   ▼ start_work / send_work             ▼ 記憶・設定アクセス │
  │   ┌──────────────────────────────┐     ┌───────────────────────────────┐   │
  │   │ 「手」（Hands / 作業）       │     │ データベース（SQLite）        │   │
  │   │ ・WorkSessionごとに1体       │     │ ・rusqlite bundled (WAL)      │   │
  │   │ ・ホスト内のRigエージェント  │     │ ・ECSスレッド直接同期アクセス │   │
  │   │ ・ACPはプラグイン側でラップ  │     │ ・FTS5 + BLOBコサイン (RRF)   │   │
  │   │ ・reportで進捗と完了を報告   │     │ ・記憶関係（memory_edges）    │   │
  │   │ ・Hermes方式スキル蓄積       │     │ ・ACIDリレーショナル永続化    │   │
  │   │ ・同一キャラクター一人称     │     │                               │   │
  │   └───────────────┬──────────────┘     └───────────────────────────────┘   │
  │                   │                                                        │
  │                   ▼ 判定パイプライン                                        │
  │   ┌────────────────────────────────────────────────────────────┐           │
  │   │ セキュリティ・実行境界                                      │           │
  │   │   ・3段階判定（機械的許可 ➔ Decision API＋自由文ポリシー） │           │
  │   │     Sandbox非必須・自由文ポリシー基準で判定                │           │
  │   │   ・承認の最小化（会話から「頭」が代理承認可能）           │           │
  │   │   ・機械的ルール ➔ Decision API ➔ ユーザー承認の3段階   │           │
  │   │   ・画面承認カード＋「頭」による代理承認ツール              │           │
  │   └────────────────────────────────────────────────────────────┘           │
  └────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. 4つの責務境界

| 境界 | 主な構成要素 | 責務と役割 |
| :--- | :--- | :--- |
| **「頭」<br>(Brain / 親)** | `rig-ecs` Bevy ECS 単一ループ<br>（`SessionLogPlugin` ＋ `DirStore`） | ・ユーザーとの会話、ロールプレイ、感情・関係性の維持<br>・プラガブル音声パイプライン（先行並行合成、割り込み停止）<br>・クライアントが宣言した能力に応じたツール公開（例: `body_motion`, クライアント固有ツール）<br>・`device_request`による端末能力の実行委譲<br>・実務タスクの「手」（WorkSession）への自然言語委託（`start_work`, `send_work`, `stop_work`）<br>・自律スケジュール管理（`schedule_create`, `schedule_list`, `schedule_cancel`） |
| **「手」<br>(Hands / ワークエージェント)** | ホスト内のRigエージェント（`WorkSession`ごとに1体）<br>MCP（`rig-rmcp`）<br>（ACPはプラグイン側でラップ） | ・`WorkSession`ごとの実務タスクの非同期実行<br>・`report`による途中報告・完了報告（ホストがセッションとRunを束縛）<br>・Hermes方式（`SKILL.md`）による手順学習 |
| **「目」<br>(Eyes / 知覚評価)** | 軽量高速モデル<br>＋クライアントセンサー | ・入力状況・ウィンドウ情報・時間経過の常時観測<br>・価値のある変化だけを選別して「頭」へ通知 |
| **「体」<br>(Body / 表現)** | クライアント表現エンジン<br>（デスクトップ実装例: `apps/ene-avatar`）<br>音声ストリーミング | ・クライアント固有の表現エンジンによるアバター描画（デスクトップ実装例: 透過ウィンドウ + Bevy + VRM）<br>・クライアント側PCM音量（RMS）によるゼロ遅延口パク（リップシンク）<br>・文単位またはストリーミングTTS再生と、割り込み時の旧世代バッファ即時破棄（Flush） |

> [!NOTE]
> `apps/ene-avatar`、OSネイティブウィンドウフック、Bevy/VRM描画はデスクトップクライアントのリファレンス実装である。ホストはそれらの実装に依存せず、`RegisterClient`で宣言された能力名・入力スキーマ・プリセットだけを保持して、`DeviceOperation`でクライアントへ委譲する。デスクトップ実装の詳細は[デスクトップクライアント設計](design/desktop-client.md)を参照する。

### Rust クレート間の依存

| 層 | 責務 |
| --- | --- |
| L0 ドメイン | `ene-domain`、`ene-policy`、`ene-mind`、`ene-catalog` が型と不変条件を所有する |
| L1 アプリケーション | `ene-client-core` がクライアントの状態遷移を所有する |
| L2 インフラ | `ene-rpc` が Connect の生成型と変換を、`ene-store` が SQLite のトランザクションを、`ene-plugin-runtime` が Wasm の実行境界を所有する |
| L3 プロセス | `apps/*` が起動・I/O・描画を組み立てる |

依存は内側へ向ける。デスクトップ UI はドメイン型とクライアントの更新を読み、Connect 型への変換は RPC 境界で行う。会話復元では Rig が復元したエージェントに安定 ID で役割を付与する。プラグインの事前フックが非同期で判定する間、Rig は対象の呼び出しを保留する。判定後は現行のポリシーを適用し、承認は実行対象の世代に結び付ける。

### 親エージェント（頭）のツールセット（クライアント能力に応じて動的化）

| カテゴリ | ツール名とシグネチャ | 役割と責務 |
| :--- | :--- | :--- |
| **記憶 (Mind)** | `mind_recall(query: String, limit: Option<u32>)` | 過去の関連エピソード記憶を検索（明示的想起） |
| | `mind_read(memory_id: String)` | 特定記憶の詳細と由来コンテキストを閲覧 |
| | `mind_update(action: MindUpdateAction)` | 記憶の追加・訂正・忘却（想起優先度の引き下げ） |
| **作業委譲 (Hands)** | `start_work(instruction: String, work_id: Option<String>)` | 「手」（WorkSession）へ実務タスクを発注（新規または既存継続） |
| | `send_work(work_id: String, text: String)` | 進行中タスクへの追加入力や方針変更の伝達 |
| | `stop_work(work_id: String)` | 進行中タスクの中断を要求 |
| **承認 (Approval)** | `resolve_approval(request_id: String, decision: String)` | ユーザーの発言から「頭」が代理で承認を確定 |
| **端末・身体・知覚 (Client capabilities)** | `device_request(capability: String, input: Object)` | 宣言・許可された端末能力を実行する。対象端末はホストが受付時に束縛し、操作単位の`Stage::Gate`と承認を通す。結果は後続のモデル呼び出しで受け取る |
| | `body_motion(motion: String, expression: Option<String>)` | `body.motion`を宣言し許可されたアクティブクライアントの表現を指示（スキーマとプリセットはクライアントが定義） |

| **共通** | `noop()` | 何も行動しない（沈黙・見守り） |
| **自律スケジュール** | `schedule_create(title: String, instruction: String, run_at: Option<String>, cron_expr: Option<String>)` | タイマーや定期リマインドを登録 |
| | `schedule_list()` | 登録済みスケジュール一覧を取得 |
| | `schedule_cancel(schedule_id: String)` | スケジュールを解除 |

> [!NOTE]
> `device_request`は汎用の能力ディスパッチャとして常時登録される。`body_motion`等のクライアント固有ツールは、アクティブクライアントが`RegisterClient`で宣言した`capability_specs`に基づいて動的に登録・解除される。ホストはクライアント実装の詳細を知らず、現在アクティブな端末へ配送する。

> [!NOTE]
> 現在日時は `PrepareSystems::Edit` により毎ターン最新が Dynamic Tail としてインメモリ注入されるためツール呼び出しは不要。また、外部Web検索は「手」（WorkSession）のツールまたは外部MCPサーバーへ委譲する。

---

## 3. 主要サブシステムの技術アーキテクチャ

### 1. エージェントループとセッション管理
- **`rig-ecs` による単一イベント駆動エージェントループ**:
  - 外側の重複Tokioループを排し、`rig-ecs`（Bevy ECS）のステージに統一：
    - 入力受付: `world.trigger(Deliver::new(...))`
    - 先行TTS: `Stage::Review`（AgentPass 内）で `MessageReader<StreamDelta>` を購読し、`Query<(&Effect, &CallOf), With<TurnCall>>` により通常ターン呼び出しをフィルタし、`matches!(delta.event, StreamEvent::Text { .. })` で思考トークン（Reasoning）を除外した上で、親コンパニオンの発話テキストのみを文単位（`。`、`！`、`？`、改行）で合成キューへ送出
    - 承認待機: `Stage::Gate` でポリシー判定し、要確認操作には `commands.spawn(HoldOf(call_entity))` を投入してReActループを保留。RPC（`SettingsService.SubmitApproval`）でユーザー回答を受け取り、承認時はホスト定義の `Approved` マーカーを付与し、拒否時は `Decider::Person` で `Outcome::refused` を確定し、`HoldOf` を解除して処理再開。承認結果は `host.sqlite3` の `approval_requests` テーブルへ保存
  - 会話ログの正本は `SessionLogPlugin` ＋ `DirStore`（`<state_dir>/rig/`）によりディスクに保存。
  - 外側タスクは音声ストリーミング入出力、途絶検知、複数コンパニオン調停を担当。
- **追記型線形ログ ＋ `rig-memory` によるCompaction**:
  - 会話の生ログおよび要約スナップショット（Aside `ene.summary`）、受理レシート（`Conversation::add_aside`）は公式の `DirStore`（`SessionLogPlugin`）に完全保存。長期エピソード記憶および心は SQLite（`host.sqlite3`）に永続化し、2相復旧を原理的に排除。
  - トークン使用率が閾値を超えたタイミング（ターン境界・区切り）でバックグラウンドCompactionを実行し、同時に記憶整理・関係性更新・感情/成長の振り返りをまとめて処理する。
- **作業エージェントのライフサイクルと停止の分離**:
  - `WorkSession`ごとにホスト内でワークエージェントを1体だけ生成し、Runのたびにrun_idを束縛し直す。エージェントを引退させるのはセッションのクローズ、開始失敗、ホスト再起動だけ。
  - 停止は`StopTurn`を送り、その場でrunを`Cancelled`として記録する。待機状態やプロセス群の強制終了は持たない。
  - **停止伝播の非結合**: ワークエージェントは親の子ではなく独立エンティティとしてWorld内に生成し、ユーザーの音声割り込み（Barge-in）による会話ターンの停止が作業セッションへ伝播して道連れ終了する事故を防止。
- **安全マージンを設けたコンテキスト上限検証と縮退**:
  - 送信直前に `next_request_tokens + OUTPUT_MARGIN <= Context Window` を安全マージンを考慮して機械的に検証。
  - 要約失敗時は古いログをアクティブプロンプト窓から一時的に切り詰めて対話を継続。
- **Prefix Caching最適化順序と非永続注入**:
  - プロンプトを `System Prompt`（静的ツール定義 + キャラクター設定・ペルソナ + ユーザープロファイル） ➔ `Conversation History`（`Summary` + 過去ターン生ログ） ➔ `Dynamic Tail Context in User_N`（Pinned Memory + 検索想起記憶 + 現在の心境 + Perception Summary + 現在時刻 + 最新ユーザー入力）の順で配置し、高いキャッシュヒット率（ターゲット: >90%）を達成。
  - 動的コンテキストは `SessionLog`（永続ログ）へは書き込まず、`PrepareSystems::Edit` で一時注入し、`PrepareSystems::Fit` 時に動的文脈を含む要求全体に対してFitを実行。

### 2. 心と自動記憶（SQLite）
- **組み込みSQLite（`rusqlite` bundled, WALモード ＋ FTS5 trigram ＋ Rustコサイン類似度）**:
  - C言語SQLiteを静的リンクして単一ファイル完結。ECSループスレッドで同期接続を直接保持し、1ms未満のPK検索・FTS5・BLOBコサイン類似度・ルール照合を遅延なく実行。重い記憶整理や埋め込み生成のみTokioバックグラウンドタスクへ分離。
- **エピソード記憶（`MemoryRecord`）**:
  - `rowid` を明示的 INTEGER PRIMARY KEY とし、FTS5外部コンテンツテーブルとの整数参照関係を不変に保証。
  - `scope`（共有スコープまたはコンパニオンID）、`importance`（0〜10: 想起優先度）、`occurred_at`、`last_accessed_at`（未引用時はnull）、`memory_revision`、`content_revision`、`embedding` を格納。
  - 1つの発話から複数事実を独立レコードとして分割保存。忘却時は物理削除や除外フラグではなく想起優先度（`importance`）の引き下げとして扱い、想起スコアを自然に低下させる（過去ログの客観記録は保持）。
  - ハイブリッド検索: FTS5 BM25（部分トークンOR結合）と ベクトル検索（Rust内スライスコサイン類似度）をRust側でReciprocal Rank Fusion（RRF, $k=60$）統合し、スコアを $0.0 \sim 1.0$ に正規化。`embedding_model` 複合識別子により適合する埋め込み空間のみを照合。
  - **Compaction連動の自動整理**: Compaction実行時に同じ会話ログを振り返り、エピソード記憶の抽出・統合、`memory_edges` の更新、感情・成長の更新を一括で処理する。
  - 想起スコア計算式：$\text{Score} = \min(1.0, \text{Importance}/0.3) \times (0.5 \cdot \text{Relevance} + 0.3 \cdot \text{Importance} + 0.2 \cdot e^{-0.05 \Delta t})$（$\text{Importance} = \text{importance}/10.0$、$\Delta t$ は `COALESCE(last_accessed_at, occurred_at, recorded_at)` からの経過日数。自動想起は `importance >= 2` フィルタと連動）
  - **引用ガード**: 単に検索されただけでは `last_accessed_at` を更新せず、明示操作（`cited_in`）または整理モデル抽出後にホストがプロンプト注入実績と照合検証した推定利用（`estimated_cited_in`）の場合に限り `last_accessed_at` を更新し、想起自己汚染を低減。
  - **Pinned Memory制約**: 重要度10であっても、訂正状態や優先度引き下げ状態を必ず検証し、専用トークン予算（1,000トークン）内で注入。

### 3. 音声対話パイプライン
- **プラガブルエンジン適応**:
  - VOICEVOX等の文単位HTTP API（文ごとに非同期合成し再生キューへ投入）、ストリーミングエンジン、双方向Live APIの各特性に合わせたアダプター構成。標準音声形式は 24kHz 16-bit mono PCM。
- **世代管理と送出順序**:
  - `generation_id` による割り込み時の旧世代チャンク破棄（`FlushControl`）。
  - クライアント側での単一音声ストリーム物理保証（新発話チャンク受信時の旧音声即時停止、旧発話IDの完全失効による逆戻り防止）。
  - `UtteranceFinished`を発話の最後の音声イベントとし、ターン終了と全文章の確定後に送る。クライアントは再生完了を報告しない。
- **複数コンパニオン調停**:
  - 全員同報配信 ＋ 自律該当性判断。日常対話の譲り合いはLLM自律協調。
  - 不可逆操作を伴う同一タスクの作業委託（`start_work`）は、ホストの原子的受付登録（`delegation::try_claim`）により先着1件のみが実行権を獲得し、多重実行を決定論的に防止。
  - 音声フロアはホスト上での発話生成先着順とし、ターン終了で解放する。複数コンパニオンが同一画面に常駐して自然に掛け合う。

### 4. セキュリティと承認
- **3段階判定**: 機械的な許可ルール ➔ Decision API（安全確認＋自由文ポリシー判定）➔ ユーザーの承認、の順でユーザー承認を最小限に抑える。
- **「頭」による代理承認**: 会話文脈でユーザーが許可したと「頭」が判断した場合、`resolve_approval` ツールで承認を確定できる。
- **OSネイティブSandboxは持たない**: `process-wrap`は子プロセスの起動・終了とプロセスグループの管理だけを行い、隔離境界を持たない。安全性は判定パイプラインと承認で担保する。
