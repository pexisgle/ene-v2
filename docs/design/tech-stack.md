# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・音声](voice-avatar.md)、[デスクトップクライアント](desktop-client.md)、[権限・Sandbox](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`rusqlite` (bundled, WAL mode) ＋ FTS5 trigram ＋ Rustコサイン類似度** | C言語SQLiteを同梱（bundled）して静的コンパイル・リンクし、外部DBデーモン不要で単一SQLiteファイル（`host.sqlite3`）で完結。ECSループスレッドで同期接続を直接保持し、1ms未満のPK検索・FTS5・BLOBコサイン類似度・ルール照合を遅延なく実行。記憶整理や埋め込み生成等の重い非同期処理のみバックグラウンドタスクへ分離 |
| **会話ログ・要約正本** | **`rig_memory::DirStore` (`rig-memory` feature `fs`)** | 公式の `rig-ecs` がネイティブに要求する同期ファイルストア。プロセス間ファイルロック、破損末尾行の自動退避、画像Blob管理を公式実装に委譲し、アダプタ実装ゼロ行で完結。受理レシート（`request_key`）も `Conversation::add_aside` でセッションログ内に記録するため、SQLiteとの2相不整合は原理的に発生しない |
| **エージェント実行・推論** | **`rig-ecs`（エージェントステートマシン）＋ `rig-rmcp` ＋ Tokio（ホスト非同期I/Oランタイム）** | 親・内蔵の子の推論・ツール反復を `rig-ecs` のECSステージ・Observerに一本化。CLIハーネス用プラグイン（`rig-basics`）を排し、純粋なECSプリミティブ（`Stage::Gate` ＋ `HoldOf` ＋ ホスト定義の `Approved` マーカー）と `rig-rmcp` による公式MCPブリッジ動的登録を採用。TokioはConnect-RPCサーバーや外部非同期I/Oに徹する |
| **OSサンドボックス** | **`process-wrap` ＋ `bwrap`（任意）＋ `cap-std`** | 全OSでプロセスツリーを管理し、Linuxでは `bwrap` があれば読み取り専用ルート・専用 `/tmp`・名前空間で隔離。`bwrap` が無ければ通知付きで非隔離実行（`landlock` / `AppContainer` / `Seatbelt` / 代理プロキシは後続フェーズ） |
| **デスクトップクライアント** | **`Slint` (GUI) ＋ `cpal` + `rubato` (音声I/O・リサンプリング) ＋ OSウィンドウ監視フック** | 軽量ネイティブGUIツールキットSlint（Royalty-free規約遵守）による軽快な設定・承認UI、cpal＋rubatoによる低遅延・高精度リサンプリング音声キャプチャ・再生、OSフックによる知覚センシング（目） |
| **アバター描画** | **`apps/ene-avatar` (`bevy` + `ene-vrm`)** | 既存資産をそのまま活用。透過デスクトップウィンドウ（Wayland layer-shell / Windows Win32 layered）、VRM 1.0モデルロード・アニメーション（`ene-vrm`）、RMS音量によるローカル即時口パク、標準入出力JSON Lines IPC制御 |
| **音声（STT/TTS）** | **プラガブル抽象インターフェース (`SttEngine` / `TtsEngine`)** | VOICEVOXやStyle-Bert-VITS2等の文単位HTTP APIから、sherpa-onnx等のストリーミングエンジン、双方向Live APIまで、各エンジンの通信仕様・機能特性に柔軟に適応 |
| **通信・RPC** | **`Connect-RPC` (`connectrpc` + `buffa` + `axum`)** | gRPC/Connect/HTTP/Web互換、高速Protobuf、双方向ストリーミング（HTTP/2）対応 |

---

## 2. 各コンポーネントの詳細と実装例・リファレンス

### A. データベース：SQLite（Embedded `rusqlite` ＋ FTS5 ＋ Rustコサイン類似度）

#### 特徴と利点
- **超軽量・ゼロ外部デーモン依存**:
  - `rusqlite`（bundled）により、C言語SQLiteのソースコードをバイナリへ静的コンパイル・リンク。追加の巨大DBデーモンや外部C++依存を持たず、単一のSQLiteファイル（`host.sqlite3`）で完結。
  - WAL（Write-Ahead Logging）モードにより、高い並行読み取り性能と耐障害性を両立。
- **ECSループスレッドでの直接同期アクセス（決定論的・ゼロオーバーヘッド）**:
  - Bevy ECS のシステム（`PrepareSystems::Edit`, `Stage::Gate` 等）は同期関数 `fn system(...)` として動作する。
  - 記憶件数が数千件規模のデスクトップ利用において、プライマリキー参照、ルール照合、FTS5 trigram検索、およびインメモリBLOBコサイン類似度計算は **1ms未満（数百マイクロ秒）** で完了する。
  - したがって、ECSメインスレッドに同期の `rusqlite::Connection` を直接保持して同期実行し、非同期エグゼキュータへのタスクスイッチやチャネル往復オーバーヘッドを完全に排除する。
  - 記憶整理（Consolidation）のLLM呼び出しや埋め込みベクトル生成（Embedding API）などの長時間非同期処理のみ、専用のTokioバックグラウンドタスクおよび別コネクションへ分離する。
- **会話ログ・要約正本（`rig_memory::DirStore`）と SQLite のクリーンな責務分離**:
  - 会話履歴（`SessionLog`）および要約スナップショット（Aside `ene.summary`）は、公式 `rig-ecs` がネイティブに要求する同期ストア `rig_memory::DirStore`（`<state_dir>/rig/`）をそのまま採用。
  - プロセス間ファイルロック、破損末尾行の自動退避、画像等のバイナリBlob管理は公式実装に完全に委譲され、アダプタの自作はゼロ行（0行）。
  - クライアントからの会話受理レシート（`request_key` 重複排除）および要約スナップショットは、SQLite ではなく `Conversation::add_aside`（SessionLog 内）に直接記録するため、ファイルシステムと SQLite 間の 2相コミットやクラッシュ時の不整合復旧問題が原理的に発生しない。
  - SQLite（`host.sqlite3`）は、心・長期エピソード記憶・認知的関係性・設定・作業・承認要求・スケジュールのACID管理に専念する。

> [!NOTE]
> **SqliteSessionStore を採用しない技術的理由（公式API仕様）**:
> `rig-memory 0.46.0` では、同期の `rig_memory::Store`（スーパートレイトとして `Files` を持つ）を実装した型はブランケット実装により自動的に `AsyncStore` になるが、逆（`AsyncStore` ➔ `Store`）は成り立たない。
> `rig-sqlite 0.46.0` の `SqliteSessionStore` は `load_async` / `save_async` が用いる `AsyncStore` のみを実装しており、`rig-ecs` の `SessionLogPlugin`（`SessionStore(Arc<dyn Store>)` / `open_session`）が要求する同期 `Store` および `Files`（`logs`, `files`, `append_file`, `remove_file` 等）を実装していない。
> さらに `rig-sqlite` は内部依存として `tokio-rusqlite` や `sqlite-vec` を引き込むため、同期 `rusqlite` に統一したECSアーキテクチャに不要な非同期ラッパーが再混入する。したがって、公式が `rig-ecs` 向けに提供する標準同期ファイルストア `rig_memory::DirStore`（アダプタ自作ゼロ行）を採用するのが最も堅牢で最小構成となる。

- **FTS5 trigram ＋ ベクトル検索（インメモリ CompactVectorIndex によるゼロアロケーション）**:
  - 日本語の分かち書きエンジン不要の **FTS5 trigram（3-gram）** インデックスにより、助詞や固有名詞を含む高精度な語句検索を実現。空白・句読点でのみ分割し、助詞正規表現分割による誤切断を防止。
  - **`CompactVectorIndex`（ECS Resource キャッシュ）**: 毎ターンの類似度検索で SQLite から全 BLOB を `Vec<u8>` としてデシリアライズするオーバーヘッドを排除するため、SQLite を正本としつつ ECS Resource 上にフラットな `Vec<f32>` 連続配列キャッシュ（`CompactVectorIndex`）を保持。`memory_revision` 更新時のみ差分同期し、数十マイクロ秒・ゼロアロケーションでスライスコサイン類似度を高速計算。FTS5 の順位と Rust 側で RRF（Reciprocal Rank Fusion）統合する。
- **1ホップ認知関係性（エッジテーブル）**:
  - 認知的関係性（`companion->remembers->memory`, `memory->cited_in->turn`, `memory->supersedes->memory`）は高々1ホップの参照であるため、シンプルな `memory_edges(from_id, rel, to_id)` テーブルで高速・堅牢に表現可能。

#### 実装例・一次情報
- **依存定義 (`Cargo.toml`)**:
  ```toml
  rusqlite = { version = "0.42", features = ["bundled"] }
  rig-core = "0.46"
  rig-ecs = "0.46"
  rig-memory = { version = "0.46", features = ["fs"] }
  rig-rmcp = "0.46"
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  ```
- **スキーマ定義とハイブリッド検索（Rust）**:
  ```rust
  // 初期化スキーマ（FTS5 trigram + 記憶テーブル + エッジ + 整理カーソル + 承認状態 + スケジュール）
  // 全コネクションでビジータイムアウトと同期モードを必須設定
  conn.execute_batch(r#"
      PRAGMA journal_mode = WAL;
      PRAGMA busy_timeout = 5000;
      PRAGMA synchronous = NORMAL;
      
      -- 明示的 INTEGER PRIMARY KEY は SQLite の rowid のエイリアスとなり VACUUM 後も安定
      CREATE TABLE IF NOT EXISTS memories (
          rowid INTEGER PRIMARY KEY,
          id TEXT NOT NULL UNIQUE,
          owner TEXT NOT NULL,
          audiences TEXT NOT NULL, -- JSON array
          content TEXT NOT NULL,
          importance INTEGER NOT NULL CHECK (importance >= 0 AND importance <= 10),
          occurred_at TEXT,
          recorded_at TEXT NOT NULL,
          last_accessed_at TEXT,
          memory_revision INTEGER NOT NULL DEFAULT 1,
          content_revision INTEGER NOT NULL DEFAULT 1,
          emotion TEXT,
          source_context TEXT,     -- JSON object
          embedding BLOB,          -- f32 array bytes
          embedding_model TEXT,    -- 埋め込み空間複合識別子: "<model_name>:<dimensions>:<prep_version>"
          embedding_dimensions INTEGER,
          embedding_content_revision INTEGER -- 生成時の content_revision (不一致時は無効・再計算対象)
      );

      -- 外部コンテンツテーブル方式で memories と整数 rowid で結合
      CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
          content,
          content='memories',
          content_rowid='rowid',
          tokenize='trigram'
      );

      -- FTS5 同期トリガー
      CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
          INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
      END;
      CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
          INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
      END;
      CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF content ON memories BEGIN
          INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
          INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
      END;

      CREATE TABLE IF NOT EXISTS memory_edges (
          from_id TEXT NOT NULL,
          relation TEXT NOT NULL, -- 'remembers', 'cited_in', 'estimated_cited_in', 'supersedes'
          to_id TEXT NOT NULL,
          created_at TEXT NOT NULL,
          PRIMARY KEY (from_id, relation, to_id)
      );

      -- 自動記憶整理（Consolidation）の進行状態永続化テーブル
      CREATE TABLE IF NOT EXISTS consolidation_state (
          companion_id TEXT PRIMARY KEY,
          last_processed_turn_id TEXT NOT NULL,
          updated_at TEXT NOT NULL
      );

      -- 権限承認要求永続化テーブル
      CREATE TABLE IF NOT EXISTS approval_requests (
          request_id TEXT PRIMARY KEY,
          session_id TEXT NOT NULL,
          run_id TEXT NOT NULL,
          action_payload_hash BLOB NOT NULL,
          tool_name TEXT NOT NULL,
          normalized_arguments TEXT NOT NULL,
          status TEXT NOT NULL, -- 'pending', 'approved', 'denied', 'expired'
          created_at TEXT NOT NULL,
          resolved_at TEXT
      );

      -- スケジュール・自律リマインド永続化テーブル
      CREATE TABLE IF NOT EXISTS schedules (
          id TEXT PRIMARY KEY,
          companion_id TEXT NOT NULL,
          cron_expr TEXT,            -- 繰り返しスケジュール (5-field cron)
          run_at TEXT,               -- 一度限りの実行日時 (ISO 8601)
          title TEXT NOT NULL,       -- 予定・リマインドのタイトル
          instruction TEXT NOT NULL, -- 発火時に親エージェントへ投入するプロンプト
          enabled INTEGER NOT NULL DEFAULT 1,
          created_at TEXT NOT NULL,
          last_triggered_at TEXT
      );
  "#)?;
  ```
- **一次ドキュメント**:
  - [SQLite FTS5 Extension Documentation](https://www.sqlite.org/fts5.html)
  - [`rusqlite` Documentation (docs.rs)](https://docs.rs/rusqlite/latest/rusqlite/)

---

### B. エージェントループ：純粋な `rig-ecs` イベント駆動アーキテクチャ

エージェント状態や実行ループをTokio側で自作せず、公式の `rig-ecs` にエージェント実行・状態管理・会話ログ正本（`DirStore`）を一本化する。TokioはConnect-RPCサーバーと外部非同期I/O（ストリーミング通信・子プロセス監視）の非同期ランタイムに専念する。

CLIコーディングエージェント用の `rig-basics`（`ApprovalPlugin` 等）や `rig-mcp-client` は採用せず、純粋な `rig-ecs` プリミティブ（`Stage::Gate` ＋ `HoldOf` ＋ ホスト定義の `Approved` マーカー）と動的ツール登録で最小構成を実現する。MCPサーバーは `rig-rmcp` の `McpConnection` で開き、得たツールを同じ動的ツール登録に載せる。

> [!NOTE]
> **単発サイドコールの位置づけ**: 要約（compaction）、記憶統合（consolidation）、ハートビート（staged）のモデル呼び出しは、ターンを持たない単発の問い合わせであり、`AgentPass` のライフサイクルには載せない。ECSスレッドを塞がないよう `HostRuntime` 上で実行し、`rig-core` のエフェクトを直接ハンドラへ渡す（`model::unary`）ことで完結させる。これらをターンとして扱う必要が生じた時点で、`Effect` へ移行する。

```text
  ┌────────────────────────────────────────────────────────┐
  │ 【ホスト外部I/O・RPCサーバー】（Tokio 非同期ランタイム） │
  │  ・Connect-RPC 受付（ChatService / WorkService / etc.） │
  │  ・外部ACPエージェントプロセスの標準入出力監視          │
  │  ・マイク音声入力 / TTSエンジンへのHTTP・ストリーム送受信│
  │  ・バックグラウンド記憶整理・埋め込み生成タスク        │
  └───────────────────────────┬────────────────────────────┘
                              │ world.trigger(Deliver::new(...))
                              ▼
  ┌────────────────────────────────────────────────────────┐
  │ 【エージェント実行・状態管理】（rig-ecs 単一World基盤）  │
  │  ・入力投入: Deliver(agent, text, DeliveryMode)         │
  │  ・動的注入: PrepareSystems::Edit (Dynamic Tail)        │
  │  ・全体調整: PrepareSystems::Fit (recent::view 窓管理)  │
  │  ・権限ゲート: Stage::Gate (HoldOf 生成 ➔ ホスト定義 Approved 付与) │
  │  ・推論・ツール反復: AgentPlugin (ReActループ)          │
  │  ・文単位先行TTS: Stage::Review (MessageReader<StreamDelta>)│
  │    (親のユーザー向け発話のみフィルタし、句点 。！？ 送出)│
  │  ・会話ログ正本: SessionLogPlugin ＋ DirStore (公式標準) │
  │  ・MCPツール統合: world.spawn_tool(def, handler, opts)  │
  └────────────────────────────────────────────────────────┘
```

#### 実装のポイント
- **状態所有権の完全一本化（二重ループの排除）**:
  - ユーザー発話、知覚センサー（目）の要約、タイマースケジュール、作業エージェント（手）からの報告は、すべて `world.trigger(Deliver::new(agent, text, DeliveryMode))` を通じてECS Worldへ直接投入される。
- **純粋なECSプリミティブによる承認待機（`HoldOf` / ホスト定義の `Approved`）**:
  - CLIハーネス用の `ApprovalPlugin`（内部非公開マーカー `Approving` を前提とする機構）を排除し、`rig-ecs` のコアコンポーネントとホスト定義のマーカーだけで完結させる：
    1. **保留**: `Stage::Gate` システムにおいて、未承認のツール呼び出しエンティティ（`call_entity`）を検知した場合、`commands.spawn(HoldOf(call_entity))` を投入してReActループを安全に待機させる。
    2. **登録**: 保留した操作の引数ハッシュを計算し、`approval_requests` テーブルおよび未決リストへ登録。クライアントへ `ApprovalRequested` 通知を発行。
    3. **承認確定（RPC ➔ ECS）**: ユーザーが承認した場合、Connect-RPC（`SettingsService.SubmitApproval`）ハンドラからECS Worldへコマンドをディスパッチし、`commands.entity(call_entity).insert(Approved)`（ホスト定義のマーカー）を付与した上で `HoldOf` エンティティを `despawn` する。後続の `Stage::Gate` は `Approved` コンポーネントを検知して通過させる。
    4. **拒否確定**: ユーザーが拒否した場合、対象ツールの `Decision` を拒否（`decision.decide(Decider::Person, Outcome::refused("User denied"))`）として確定し、`HoldOf` エンティティを `despawn` する。
    5. **記録**: 承認結果は `host.sqlite3` の `approval_requests` テーブルへ永続化する。会話ログ（`SessionLog`）を汚染しない。
- **動的文脈の非永続注入（Dynamic Tail）と KV Prefix Cache 最大化**:
  - Anthropic/OpenAI/Gemini の KVキャッシュ（Prefix Caching）を最大限に活かすため、プロンプト構造を以下に固定する：
    $$\text{[System Prompt]} \longrightarrow \text{[Past Dialogue History]} \longrightarrow \text{[Dynamic Tail in User\_N]} \longrightarrow \text{[User Speech]}$$
  - Pinned Memory、想起エピソード記憶、現在の心（`MindState`）、知覚要約（Perception Summary）、現在日時は、永続会話ログ（`SessionLog`）には一切書き込まず、最新のユーザー発話ブロック（`User_N`）の末尾コンテキスト枠に一時注入する。
  - これにより、`[System Prompt, ..., Assistant_{N-1}]` までの過去ログ全体が完全に不変のプレフィックスとして保持され、>90% のキャッシュヒット率を達成する。
  - **1ターン内のReActツール反復におけるキャッシュ固定規約**:
    - 1ターン内でモデルがツール（例: `mind_recall`）を呼び出し、その `ToolResult` を持って2回目のモデル推論（Step 2）が走る際、ターン開始時に生成した Dynamic Tail のスナップショット（時刻・心境・想起記憶）を固定し、ターン内の複数ステップ間で全く同一のコンテキスト文字列を維持する。
    - これにより、1ターン内のReActツール反復において Prefix Cache が 100% 効くことを保証する。
  - `PrepareSystems::Edit` で動的文脈を注入し、`PrepareSystems::Fit` で動的文脈を含む要求全体に対してトークン数計測と `recent::view` 窓切り詰めを実行する。
- **先行TTSストリーム文字片の厳格なフィルタリング**:
  - `Stage::Review`（AgentPass 内）で `MessageReader<StreamDelta>` を購読するシステムは、**`Query<(&Effect, &CallOf), With<TurnCall>>` により `delta.effect` が通常の会話ターン呼び出し（`TurnCall` マーカー付き）であることを機械的にフィルタ**する。
  - これにより、親エージェントがトリガーしたサイドLLM呼び出し（Compaction要約や記憶整理）の出力がTTSに混入する事故を完全に防ぐ。
  - さらに **`matches!(delta.event, StreamEvent::Text { .. })` で思考トークン（Reasoning / Thinking）を確実に除外**し、親コンパニオンのユーザー向け発話テキストのみを句点（`。`、`！`、`？`、改行）ごとに切り出してTTSタスクへ送出する。
- **MCPツールの動的ECS登録（`rig-rmcp`）**:
  - 外部ファイル自動探索（`rig-mcp-client` の暗黙読み込み）を排し、公式の **`rig-rmcp`**（`McpConnection` / `ManagedToolSink`）または `rig_mcp_client::McpCorePlugin` を採用。
  - eneの設定DBで認可されたMCPサーバーのみを明示的に接続・再接続管理し、取得されたツール定義を `world.spawn_tool(tool_def, tool_handler, opts)`（Pure ECSツール登録）で動的エンティティとしてWorldへ追加・削除する。設定の正本はeneが単一所有する。
- **7つの `ModelAssignments` と `rig-core` カタログの最小マッピング**:
  - `rig_core::catalog::Catalog` はモデルメタデータ（`ModelSpec`：コンテキスト長・入出力上限等）を引くための純粋なデータ辞書として利用し、モデル接続はプロバイダークライアントから `rig_ecs::model::Connection` へ直接マッピングする：
    ```rust
    // rig-core 0.46 公式 ModelAdapter + Connection の最小構成
    // Catalog からモデル仕様 (ModelSpec) を取得
    let spec = catalog.get(&assignments.companion.model_id)?;
    // プロバイダークライアント経由で生成したモデルを ErasedHandler で包む
    let adapter = ModelAdapter::<Completion>::new(client.completion_model(&assignments.companion.model_id));
    let handler = ErasedHandler::new(adapter);
    // rig-ecs Connection コンポーネントとして World へ注入
    commands.entity(companion_entity).insert(rig_ecs::model::Connection {
        spec,
        handler: Handler(handler),
    });
    ```
- **自律tickターンの段階的昇格方式（Staged PromotionによるSessionLog保護）**:
  - `rig-ecs` の `SessionLogPlugin` は `Deliver` が呼ばれたフレーム末尾で即座にディスク上のファイル（`DirStore`）へ Incoming メッセージを追記する。`DirStore` は追記専用であるため、後から `Conversation` の末尾エントリを消してもディスク上のログを巻き戻すことはできない。
  - したがって、自律tickや見守り判定の推論は、最初からメインの `Conversation` に `Deliver` せず、現在の `Conversation` の一時ビュー（またはクローン）を用いて推論を実行する。
  - モデルが `noop()` 以外（ユーザーへの発話テキスト、または `start_work` 等の実アクション）を返した場合にのみ、そのターン（入力＋発話／アクション）をメインの `Conversation` 正本へ `Deliver` / 記録する。これにより、ディスクI/Oとセッションログの不要な肥大化を根本から防ぐ。

#### 実装例・一次情報
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)
- [`rig-ecs` Documentation](https://docs.rs/rig-ecs)（Bevy ECSベースのエージェント実行）
- [`rig-memory` Documentation](https://docs.rs/rig-memory)（DirStore・Compaction要約・Anchor）
- [`rig-rmcp` Documentation (docs.rs)](https://docs.rs/rig-rmcp)（公式RMCP SDKブリッジ）

---

### C. OSサンドボックス：Alpha最小構成

Alphaは、全OSで確実に効く境界だけを持ち、後続フェーズで強化する。

| クレート / 機構 | 担当責務 | 適用対象 |
| :--- | :--- | :--- |
| **`process-wrap`** | 子プロセスの生成、プロセスツリー/Job Object管理、kill-on-drop | 全OS共通（プロセスライフサイクルとタイムアウト強制終了） |
| **`bwrap`** | 読み取り専用ルート、専用 `/tmp`、PID名前空間、`network: isolated` 時のネットワーク名前空間 | Linux（`bwrap` がある場合のみ） |
| **`cap-std`** | ホスト（Rust）コード自身のファイル操作におけるパストラバーサル（`../`）防止 | 全OS共通（ホスト自身の安全なファイルI/O） |

`bwrap` が無い環境でもACPは起動し、`acp_sandbox_unavailable` の通知とともに非隔離で実行する。`landlock`、Windows `AppContainer`、macOS `Seatbelt`、およびホスト名単位の通信制限を担う代理プロキシは後続フェーズの分離強化であり、Alphaの構成には含めない（[権限・Sandbox](permissions.md) §2）。

#### 実装例・一次情報
- [`cap-std` Docs.rs](https://docs.rs/cap-std/latest/cap_std/)（オープンディレクトリをルートとする安全なファイル操作）
- [`process-wrap` Docs.rs](https://docs.rs/process-wrap/latest/process_wrap/)（Unix process-group、Windows Job Object、kill-on-dropのマルチプラットフォーム対応）
- [bubblewrap](https://github.com/containers/bubblewrap)（非特権ユーザー名前空間によるサンドボックス）
### D. 音声パイプライン（STT / TTS）：プラガブル設計

エンジンの通信仕様（文単位REST API / ストリーミング / 双方向Live API）に合わせて柔軟に動作する抽象インターフェース。

```rust
pub trait SttEngine: Send + Sync {
    /// 音声ストリームチャンクを受け取り、中間テキストまたは確定テキストを返す
    async fn process_chunk(&mut self, pcm_chunk: &[f32]) -> Result<Option<SttResult>, SttError>;
}

pub trait TtsEngine: Send + Sync {
    /// エンジンの能力を取得
    fn capabilities(&self) -> TtsCapabilities;
    
    /// 文単位テキストを音声に合成（Alpha標準）
    async fn synthesize(&self, text: &str, params: &VoiceParams) -> Result<AudioData, TtsError>;
}
```

#### サポート対象エンジン
- **TTSエンジン**:
  - `VOICEVOX` / `Style-Bert-VITS2`（Alpha標準: 日本語キャラクター対話の標準文単位HTTP API）
  - `sherpa-onnx` / `Piper`（ローカルニューラルストリーミング音声合成。後続フェーズでStream契約拡張）
  - `ElevenLabs` / `Gemini Live` / `OpenAI Realtime`（双方向会話バックエンドとして段階的拡張）
- **STTエンジン**:
  - `sherpa-onnx`（C++/Rustネイティブ・ONNXストリーミング、Python不要、Windows/Mac/Linux全対応）
  - `whisper-rs`（whisper.cppバインディング）
  - 高速クラウドAPI（Groq Whisper, Deepgram）

---

### E. アバター描画：`apps/ene-avatar`（Bevy + `ene-vrm`）の資産活用と段階的拡張

- **既存実装（`apps/ene-avatar`）で実装済みの機能**:
  - `apps/ene-avatar` は `bevy = "=0.20.0"` および `crates/ene-vrm` により、以下のコア基盤を完結して実装している：
    1. **クロスプラットフォーム透過デスクトップウィンドウ**:
       - Linux: Wayland layer-shell（`exwlshellev = "0.20.1"` による透過・背後クリック透過）。
       - Windows: Win32 API（`windows = "0.62"`）によるレイヤード透過ウィンドウ（Layered Window）。
    2. **VRM 1.0モデルロードと単一VRMAアニメーション再生**:
       - `ene-vrm` によるモデル読み込み、SpringBone（髪・服の揺れもの物理）、VRMAクリップ再生。
    3. **標準入出力 JSON Lines IPC基盤（`src/control.rs`）**:
       - `add`, `set`, `get`, `remove`, `center`, `fps`, `quit` の7種コマンドによるプロセス制御。
- **今後追加する最小拡張（Future Extensions）**:
  - **ワンウェイ通知型リップシンク（`lip_sync`）**:
    - リップシンク（口パク）は毎秒60回発生する高頻度ストリームであるため、通常のリクエスト-レスポンス（`{"id": N, "ok": null}`）ではなく、**応答不要のワンウェイ通知（Fire-and-forget Event）** として `{"op": "lip_sync", "character": "ene", "rms": 0.42}` を受け付けるよう拡張する。これにより、標準出力のバッファ溢れ・IPC詰まりを根本から防止する。
  - **表情ブレンドシェイプとクロスフェード遷移**:
    - `config.expression`（表情名指定）および Bevy標準 `AnimationTransitions` によるモーション切り替え時のスムーズなクロスフェード補間（約300ms）を段階的に導入する。

---

### F. デスクトップクライアント技術スタック（Desktop Client）

ホストデーモン（`apps/ene-host`）と協調して動作するデスクトップクライアント（UI/知覚/オーディオ）の技術スタック。実装固有の仕様は[デスクトップクライアント設計](desktop-client.md)を参照する。

| コンポーネント | 採用技術 | 選定理由と責務 |
| :--- | :--- | :--- |
| **ネイティブGUI** | **`Slint`** | ElectronやQtのような巨大なランタイム・Webブラウザエンジンを排し、Pure Rustで軽量・高速に描画されるネイティブGUIツールキットを採用。承認カード、設定画面、コンパニオン切替UIを数MBのフットプリントで瞬時に表示。<br>※ライセンスは `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0`。OSS公開時はGPLv3、プロプライエタリ配布時はSlint Royalty-free Desktop License（帰属表示義務を満たす）を採用 |
| **オーディオ入出力・リサンプリング** | **`cpal` ＋ `rubato`** | クロスプラットフォーム（ALSA/PulseAudio/WASAPI/CoreAudio）対応の低レイテンシオーディオI/O。オーディオハードウェアネイティブのサンプリングレート（48kHz/44.1kHz stereo等）と内部ワイヤー標準（24kHz mono PCM）間の相互変換を公式高品質リサンプラー `rubato` で行い、デバイス互換性と正確なRMS振幅計測を両立 |
| **知覚センサー（目）** | **OSネイティブウィンドウフック** | Linux（X11/Wayland）、Windows（`SetWinEventHook`）、macOS（Accessibility API）を通じてアクティブウィンドウのタイトルやフォーカス変化をリアルタイム検知 |
| **アバター子プロセス管理** | **標準入出力 JSON Lines IPC** | `apps/ene-avatar` を子プロセスとして起動・監視し、標準入出力経由でモーション指定、表情、リップシンク振幅をミリ秒単位で制御 |

---

### G. 外部ACPエージェント接続と Connect-RPC サーバー設定

- **ACP外部ハーネス接続の最小イディオム**:
  - `agent-client-protocol = "5.2.0"` を採用し、型定義は公式Docsの安定性規約に従い `agent_client_protocol::v1::schema` 経由で参照する（`agent-client-protocol-schema` への直接依存はマイナーバージョンの固定ピン破損を防ぐため禁止）。
    - 例外は Wasm ゲストSDK（`crates/ene-plugin`）のみ。ACP SDK は `wasm32-unknown-unknown` 向けにビルドできない（`uuid` の乱数源が未指定になる）ため、型定義だけを提供する `agent-client-protocol-schema` に直接依存する。その際もバージョンはキャレット要件（`"1.15.1"`）とし、SDK 側のピンが 1.x 内で動いても解決が壊れないようにする。ホスト側はこの例外を使わない。
  - `process-wrap` の `TokioCommandWrap` で起動した子プロセスの標準入出力を `Client::v1()` トランスポートへ接続する最小構成：
    ```rust
    use agent_client_protocol::{ByteStreams, Client, schema::ProtocolVersion, v1::schema::InitializeRequest};
    use process_wrap::tokio::*;
    use tokio::process::Command;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    let mut wrap = CommandWrap::with_new("opencode", |cmd| {
        cmd.stdin(std::process::Stdio::piped())
           .stdout(std::process::Stdio::piped());
    });
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrap.wrap(JobObject);
    wrap.wrap(KillOnDrop);

    let mut child = wrap.spawn()?;
    let stdin = child.stdin().take().expect("stdin").compat_write();
    let stdout = child.stdout().take().expect("stdout").compat();

    Client.v1()
        .name("ene-host")
        .connect_with(ByteStreams::new(stdin, stdout), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            // WorkSession (cx.build_session_cwd() 等) を維持して prompt / cancel を実行
            Ok(())
        })
        .await?;
    ```

- **Connect-RPC サーバーの HTTP/2（h2c）設定**:
  - `VoiceService.StreamAudioInput` 等の双方向ストリーミングを平文ローカル通信（h2c）で安定動作させるため、`connectrpc::Server` または axum / hyper において明示的に HTTP/2 prior knowledge（h2c）を有効化して起動する（デフォルトの HTTP/1.1 単一受付によるストリーミング遮断を防止）。

> [!NOTE]
> **Extism (Wasm) プラグインのフェーズ位置づけ**:
> Extism (Wasm) によるサードパーティプラグイン実行環境は、後続の拡張フェーズ（Phase 2）へ延期する。Alpha/Minimal構成では、内蔵Rigツール、外部MCPサーバー（JSON-RPC）、および外部ACPコーディングエージェント（stdio）の3系統に集中し、過剰な抽象化層を排して堅牢な動作を担保する。
> 型定義SDK（`crates/ene-plugin`）とExtism組み込みランタイム（`crates/ene-plugin-runtime`）はPhase 2に備えて先行実装済みで、ワークスペースのビルド・テスト対象に含まれる。Alphaの `ene-host` はこれらをリンクせず、プラグインツールも公開しない。
