# ene システムアーキテクチャ

状態：Alpha採用設計（2026-11-11策定）。[構想](vision.md)、[機能一覧](features/README.md)、[技術スタック](design/tech-stack.md)、[心と自動記憶](design/mind.md)、[作業相手API](design/work.md)、[コンテキスト圧縮](design/compaction.md)、[知覚・音声](design/voice-avatar.md)、[権限・Sandbox](design/permissions.md)、[プラグイン](design/plugins.md)を統合する全体構造を定義する。

---

## 1. 全体アーキテクチャ概要

eneは、デスクトップに常駐するキャラクターとの対話から、PC内の実務作業を自然に任せられるAIコンパニオンシステムである。
システムは **「頭」「手」「目」「体」の4つの直交する責務境界** に基づいて構築される。

```text
                     ┌───────────────────────────────────┐
                     │         ユーザー / デスクトップ      │
                     └───────┬───────────────────▲───────┘
          OSイベント / 画面知覚 │                   │ 音声出力 / 透過アバター表示
                             ▼                   │
  ┌──────────────────────────────────────────────┴─────────────────────────────┐
  │ 【クライアント層】                                                          │
  │   ・アバター描画（Body）: ene-avatar（Bevy + ene-vrm、透過ウィンドウ）       │
  │   ・リップシンク: クライアント側RMS音量解析（フレーム連動口パク）           │
  │   ・知覚センサー（Eyes）: OSイベントフック ＋ アイドルサンプリング（15〜30秒） │
  │   ・音声入出力: ストリーミングSTT同報配信 / 音声Server Streaming受信       │
  └──────────────────────────────┬─────────────────────────────────────────────┘
                                 │ Connect-RPC
  ┌──────────────────────────────▼─────────────────────────────────────────────┐
  │ 【ホスト層（ene-host）】                                                    │
  │                                                                            │
  │   ┌────────────────────────────────────────────────────────────────────┐   │
  │   │ 「頭」（Brain / 親エージェント）：rig-ecs Bevy ECS 単一ループ       │   │
  │   │   ・エージェント実行・ツール反復（Rig 基盤: rig-ecs）              │   │
  │   │   ・会話ログ正本（rig-memory: DirStore 公式標準）                 │   │
  │   │   ・MCPツール動的登録（world.spawn_tool: ene設定所有）             │   │
  │   │   ・ECS制御: 先行TTS（Stage::Review StreamDelta）、承認保留（HoldOf） │   │
  │   │   ・ツール群: start_work, send_work, stop_work, body_motion, noop   │   │
  │   └───────────────┬────────────────────────────────────┬───────────────┘   │
  │                   │                                    │                   │
  │                   ▼ start_work / send_work             ▼ 記憶・設定アクセス │
  │   ┌──────────────────────────────┐     ┌───────────────────────────────┐   │
  │   │ 「手」（Hands / 作業相手）    │     │ データベース（SQLite）         │   │
  │   │   ・WorkSession と Run の分離│     │   ・rusqlite bundled (WAL)    │   │
  │   │   ・親子停止伝播の非結合     │     │   ・ECSスレッド直接同期アクセス│   │
  │   │   ・同一キャラクター一人称対話│    │   ・FTS5 + BLOBコサイン (RRF) │   │
  │   │   ・定期進捗報告と途絶検知   │     │   ・記憶関係（memory_edges）  │   │
  │   │   ・30秒タイムアウト強制終了 │     │   ・ACIDリレーショナル永続化  │   │
  │   └───────────────┬──────────────┘     └───────────────────────────────┘   │
  │                   │                                                        │
  │                   ▼ サンドボックス実行                                      │
  │   ┌────────────────────────────────────────────────────────────┐           │
  │   │ セキュリティ・実行境界                                      │           │
  │   │   ・OSネイティブ隔離（Linux Landlock/bwrap, Windows App-   │           │
  │   │     Container/Job, macOS Seatbelt, cap-std）               │           │
  │   │   ・非対応環境での明示的警告・承認による縮退実行           │           │
  │   │   ・多層判定パイプライン（機械的ルール照合 ➔ AI補助判定）   │           │
  │   │   ・マルチモーダル承認（音声承認は同時1件限定 / 画面カード） │           │
  │   └────────────────────────────────────────────────────────────┘           │
  └────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. 4つの責務境界

| 境界 | 主な構成要素 | 責務と役割 |
| :--- | :--- | :--- |
| **「頭」<br>(Brain / 親)** | `rig-ecs` Bevy ECS 単一ループ<br>（`SessionLogPlugin`） | ・ユーザーとの会話、ロールプレイ、感情・関係性の維持<br>・プラガブル音声パイプライン（先行並行合成、割り込み停止）<br>・アバターのモーション・表情の直接指示（`body_motion`）<br>・実務タスクの「手」への自然言語委託（`start_work`, `send_work`, `stop_work`）<br>・「目」への動的Watcher登録（`set_eye_watch`） |
| **「手」<br>(Hands / 子)** | WorkSession / Run<br>MCP / ACP / Wasm | ・自然言語指示に基づく実務タスクの自律遂行（軽量調査〜重い開発）<br>・同一キャラクターとしての一人称報告（「私が今〜しているよ」）<br>・定期的な状況報告（Activity）と進捗途絶時の自己通知<br>・中断要求（`CancellationRequested`）を受けたクリーンアップと30秒タイムアウト強制終了 |
| **「目」<br>(Eyes / 知覚)** | クライアントセンサー<br>知覚要約生成器 | ・OSイベント（フォーカス切替、ファイル保存等）の即時フック<br>・無操作時のアイドルサンプリング（15〜30秒間隔）<br>・高解像度な作業状況を簡潔なPerception Summaryへ要約<br>・動的Watcher条件合致時の優先割り込み通知（Priority Interrupt） |
| **「体」<br>(Body / 表現)** | `ene-avatar`<br>音声ストリーミング | ・透過デスクトップウィンドウ上でのVRM 1.0 3D描画（Bevy）<br>・クライアント側PCM音量（RMS）によるゼロ遅延口パク（リップシンク）<br>・文単位またはストリーミングTTS再生と、割り込み時の旧世代バッファ即時破棄（Flush） |

---

## 3. 主要サブシステムの技術アーキテクチャ

### 1. エージェントループとセッション管理
- **`rig-ecs` による単一イベント駆動エージェントループ**:
  - 外側の重複Tokioループを排し、`rig-ecs`（Bevy ECS）のステージに統一：
    - 入力受付: `world.trigger(Deliver::new(...))`
    - 先行TTS: `Stage::Review`（AgentPass 内）で `MessageReader<StreamDelta>` を購読し、`delta.agent` が親コンパニオンの Entity と一致するテキスト文字片のみをフィルタして文単位（`。`、`！`、`？`、改行）で合成キューへ送出
    - 承認待機: `Stage::Gate` でポリシー判定し、要確認操作には `commands.spawn(HoldOf(call_entity))` を投入してReActループを保留。RPC（`SettingsService.SubmitApproval`）でユーザー回答を受け取り、承認時は `Approved` コンポーネントを付与、拒否時は `Outcome::refused` を確定し、`HoldOf` を解除して処理再開。承認結果は `host.sqlite3` の `approval_requests` テーブルへ保存
  - 会話ログの正本は `SessionLogPlugin` ＋ `DirStore`（`<state_dir>/rig/`）によりディスクに保存。
  - 外側タスクは音声ストリーミング入出力、無操作アイドル監視、複数コンパニオン調停、ACP WorkSession接続維持を担当。
- **追記型線形ログ ＋ `rig-memory` によるCompaction**:
  - 会話の生ログおよびCompaction要約（`Compacted`）、受理レシート（`Conversation::add_aside`）は公式の `DirStore`（`SessionLogPlugin`）に完全保存。長期エピソード記憶および心は SQLite（`host.sqlite3`）に永続化し、2相復旧を原理的に排除。
  - アイドル検知（ユーザー無操作60秒以上、独り言は除外）でバックグラウンドCompactionを実行。
  - Compaction中にユーザー発話があった場合、Compaction完了を待たず直前のSnapshotから即座に応答を返し、実行中だった要約タスクは安全にキャンセル・破棄して次回アイドル時へ延期。ハード強制閾値到達時も同期要約による会話停止は避け、緊急窓切り詰めを優先適用して応答テンポを維持。
- **作業エージェントのセッション接続維持とライフサイクル分離**:
  - `WorkSession ≒ ACP Session` としてプロセス接続を維持。Run（指示ごとの実行）の中断はACP標準の `session/cancel` で行い、プロセス強制終了はセッション終了時や暴走時のみ実施。
  - **停止伝播の非結合**: 継続稼働する `WorkSession` は親の `SpawnedBy` ではなく独立エンティティとしてWorld内に生成し、ユーザーの音声割り込み（Barge-in）による会話ターンの停止が作業セッションへ伝播して道連れ終了する事故を防止。
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
  - `owner`（作成者コンパニオン不変保持）、`audiences`（共有リスト）、`importance`（0〜10: 想起優先度）、`occurred_at`、`last_accessed_at`（未引用時はnull）、`memory_revision`、`content_revision`、`embedding` を格納。
  - 1つの発話から複数事実を独立レコードとして分割保存。忘却時は物理削除や除外フラグではなく想起優先度（`importance`）の引き下げとして扱い、想起スコアを自然に低下させる（過去ログの客観記録は保持）。
  - ハイブリッド検索: FTS5 BM25（部分トークンOR結合）と ベクトル検索（Rust内スライスコサイン類似度）をRust側でReciprocal Rank Fusion（RRF, $k=60$）統合し、スコアを $0.0 \sim 1.0$ に正規化。`embedding_model` 複合識別子により適合する埋め込み空間のみを照合。
  - **増分自動整理**: 単一フライトガード（高々1件）のもと、`consolidation_state.last_processed_turn_id` より後の新規ターンのみを単一方向パイプラインで増分走査し、既存記憶の照合（更新 vs 新規分割作成）と開始カーソル一致検証（`WHERE last_processed_turn_id = :job_start_turn_id`）を伴う原子的コミットにより、同一ターン範囲の二重コミットを完全に排除。
  - 想起スコア計算式：$\text{Score} = \min(1.0, \text{Importance}/0.3) \times (0.5 \cdot \text{Relevance} + 0.3 \cdot \text{Importance} + 0.2 \cdot e^{-0.05 \Delta t})$（$\text{Importance} = \text{importance}/10.0$、$\Delta t$ は `COALESCE(last_accessed_at, occurred_at, recorded_at)` からの経過日数。自動想起は `importance >= 2` フィルタと連動）
  - **引用ガード**: 単に検索されただけでは `last_accessed_at` を更新せず、明示操作（`cited_in`）または整理モデル抽出後にホストがプロンプト注入実績と照合検証した推定利用（`estimated_cited_in`）の場合に限り `last_accessed_at` を更新し、想起自己汚染を低減。
  - **Pinned Memory制約**: 重要度10であっても、訂正状態や優先度引き下げ状態を必ず検証し、専用トークン予算（1,000トークン）内で注入。

### 3. 音声対話パイプライン
- **プラガブルエンジン適応**:
  - VOICEVOX等の文単位HTTP API（文ごとに非同期合成し再生キューへ投入）、ストリーミングエンジン、双方向Live APIの各特性に合わせたアダプター構成。標準音声形式は 24kHz 16-bit mono PCM。
- **世代管理と順序保証**:
  - `generation_id` による割り込み時の旧世代チャンク破棄（FlushBuffer）。
  - クライアント側での単一音声ストリーム物理保証（新発話チャンク受信時の旧音声即時停止、旧発話IDの完全失効による逆戻り防止、実再生進捗報告による未伝達部分の記録整合、再生中音声フレーム除外と再生停止後新規発話束縛によるエコー誤承認防止）。
  - 文単位TTSの並行合成時における `sequence_no` 整列再生、合成失敗時の明示的 `SentenceSkipped` 通知による待機解除、および発話送信完了（`UtteranceFinished`）とクライアント再生完了（`ReportPlaybackStatus(COMPLETED)`）の二段階完了契約による再生途中打ち切り・ハング防止。
- **複数コンパニオン調停**:
  - 全員同報配信 ＋ 自律該当性判断。日常対話の譲り合いはLLM自律協調。
  - 不可逆操作を伴う同一タスクの作業委託（`start_work`）は、ホストの原子的受付登録（`try_claim_delegation`）により先着1件のみが実行権を獲得し、多重実行を決定論的に防止。
  - 音声フロアはホスト上での発話テキスト生成（文ストリーム）先着順。会話リレー（`speech_completed`）は最大5往復で自動打ち切り。クライアント再生完了（`ReportPlaybackStatus`）またはタイムアウトによる安全なフロア解放（遅延完了報告は破棄）。

### 4. セキュリティとSandbox
- **OSネイティブ隔離**: Linux（Landlock/bwrap）、Windows（AppContainer/Job Objects）、macOS（Seatbelt）によるプロセス隔離。
- **非対応環境の縮退**: Sandboxが使えない環境では、非隔離実行である旨を警告・ユーザー承認を得てから実行。
- **判定パイプライン**: ハード禁止/毎回確認 ➔ 機械的ルール照合 ➔ LLM補助判定 ➔ ユーザー承認 ➔ 実行直前検証の多層防御。
- **承認要求の不変束縛**: 引数ハッシュを束縛し、引数改変時は承認失効。音声承認は同時に1件のみ有効。
