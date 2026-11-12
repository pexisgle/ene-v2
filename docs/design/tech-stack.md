# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・音声](voice-avatar.md)、[権限・Sandbox](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`rusqlite` (bundled, WAL mode) ＋ FTS5 trigram ＋ Rustコサイン類似度** | C言語SQLiteを同梱（bundled）して静的コンパイル・リンクし、外部DBデーモン不要で単一SQLiteファイルで完結。FTS5 trigram全文検索とBLOBベクトルコサイン類似度検索を単一SQLiteで完結。`tokio-rusqlite`（または専用DBワーカー）を介して非同期ECSループをブロックせずACIDトランザクションを実行 |
| **エージェント実行・推論** | **`rig-ecs`（エージェントステートマシン）＋ Tokio（ホスト非同期I/Oランタイム）** | 親・内蔵の子の推論・ツール反復・会話ログ正本（`SessionLogPlugin` + `DirStore`）を `rig-ecs` のECSステージ・Observerに一本化。TokioはConnect-RPCサーバーや外部非同期I/Oに徹し、二重ループを完全排除 |
| **OSサンドボックス** | **OSネイティブ隔離の抽象化 (`cap-std`, `process-wrap`, `landlock`)** | Windows（Job Objects / AppContainer）、Linux（Landlock / bwrap）、macOS（Seatbelt）を統一インターフェースで制御。非対応環境では明示的警告・承認で縮退 |
| **アバター描画** | **`apps/ene-avatar` (`bevy` + `ene-vrm`)** | 既存資産をそのまま活用。透過デスクトップウィンドウ（Wayland layer-shell / Windows Win32 layered）、VRM 1.0モデルロード・アニメーション（`ene-vrm`）、RMS音量によるローカル即時口パク、標準入出力JSON Lines IPC制御 |
| **音声（STT/TTS）** | **プラガブル抽象インターフェース (`SttEngine` / `TtsEngine`)** | VOICEVOXやStyle-Bert-VITS2等の文単位HTTP APIから、sherpa-onnx等のストリーミングエンジン、双方向Live APIまで、各エンジンの通信仕様・機能特性に柔軟に適応 |
| **通信・RPC** | **`Connect-RPC` (`connectrpc` + `buffa` + `axum`)** | gRPC/Connect/HTTP/Web互換、高速Protobuf、Server Streaming対応 |

---

## 2. 各コンポーネントの詳細と実装例・リファレンス

### A. データベース：SQLite（Embedded `rusqlite` ＋ FTS5 ＋ Rustコサイン類似度）

#### 特徴と利点
- **超軽量・ゼロ外部デーモン依存**:
  - `rusqlite`（bundled）により、C言語SQLiteのソースコードをバイナリへ静的コンパイル・リンク。追加の巨大DBデーモンや外部C++依存を持たず、単一のSQLiteファイル（例: `~/.local/share/ene/host.sqlite3`）で完結。
  - WAL（Write-Ahead Logging）モードにより、高い並行読み取り性能と耐障害性を両立。
- **スレッド分離と非同期DBワーカー（ECSループの非ブロッキング化）**:
  - `rusqlite::Connection` は `Send` だが `Sync` ではないため、Bevy ECSのメインスレッドで長大なベクトル走査やDB書き込みロック待ちを実行すると、音声ストリーミングやゲームループがブロックするリスクがある。
  - したがって、DB操作は `tokio-rusqlite`（専用バックグラウンドスレッドで `Connection::call` を実行する薄い公式推奨パターン）または専用非同期タスク境界に集約し、ECSメインループは非同期チャネルを介して非ブロッキングで結果を受け取る。
- **FTS5 trigram ＋ ベクトル検索（BLOBコサイン類似度）**:
  - 日本語の分かち書きエンジン不要の **FTS5 trigram（3-gram）** インデックスにより、助詞や固有名詞を含む高精度な語句検索を実現。
  - 選択された埋め込みモデル（ローカル `fastembed` / クラウド `text-embedding-3-small` 等）のベクトルを BLOB として保存し、Rust内のスライスコサイン類似度計算と FTS5 の順位を Rust 側で RRF（Reciprocal Rank Fusion）統合（外部C拡張に依存せず安定・最小化）。
  - ベクトル走査は選択中モデル・次元数に一致するレコードのみを対象とし、必要に応じて小バッチ化して実行する。
- **1ホップ認知関係性（エッジテーブル）**:
  - 認知的関係性（`companion->remembers->memory`, `memory->cited_in->turn`, `memory->supersedes->memory`）は高々1ホップの参照であるため、外部キー制約およびシンプルな `memory_edges(from_id, rel, to_id)` テーブルで高速・堅牢に表現可能。

#### 実装例・一次情報
- **依存定義 (`Cargo.toml`)**:
  ```toml
  tokio-rusqlite = { version = "0.8", features = ["bundled"] }
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  ```
  ※ `tokio-rusqlite = { version = "0.8", features = ["bundled"] }` は内部で SQLite C ソースを同梱して静的リンクし、かつ `rusqlite` の型（`tokio_rusqlite::Connection` および re-export された `rusqlite` 型群）を直接公開するため、最上位で個別に `rusqlite` を重ねて宣言することによる版の不一致や C ライブラリの二重リンクを防止する。
- **スキーマ定義とハイブリッド検索（Rust）**:
  ```rust
  // 初期化スキーマ（FTS5 trigram + 記憶テーブル + エッジ + 整理カーソル）
  conn.execute_batch(r#"
      PRAGMA journal_mode = WAL;
      
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
  "#)?;
  ```
- **一次ドキュメント**:
  - [SQLite FTS5 Extension Documentation](https://www.sqlite.org/fts5.html)
  - [`rusqlite` Documentation (docs.rs)](https://docs.rs/rusqlite/latest/rusqlite/)
  - [`tokio-rusqlite` Documentation (docs.rs)](https://docs.rs/tokio-rusqlite)

---

### B. エージェントループ：`rig-ecs` 単一イベント駆動アーキテクチャ

エージェント状態や実行ループをTokio側で自作せず、Rig公式の `rig-ecs` にエージェント実行・状態管理・会話ログ正本を一本化する。TokioはRPCサーバー（axum + connectrpc）と外部非同期I/O（ストリーミング通信・子プロセス）の非同期ランタイムとして機能する。

```text
  ┌────────────────────────────────────────────────────────┐
  │ 【ホスト外部I/O・RPCサーバー】（Tokio 非同期ランタイム） │
  │  ・Connect-RPC 受付（ChatService / WorkService / etc.） │
  │  ・外部ACPエージェントプロセスの標準入出力監視          │
  │  ・マイク音声入力 / TTSエンジンへのHTTP・ストリーム送受信│
  │  ・非同期SQLiteワーカー（tokio-rusqlite）              │
  └───────────────────────────┬────────────────────────────┘
                              │ world.trigger(Deliver::new(...))
                              ▼
  ┌────────────────────────────────────────────────────────┐
  │ 【エージェント実行・状態管理】（rig-ecs 単一World基盤）  │
  │  ・入力投入: Deliver(agent, text, DeliveryMode)         │
  │  ・プロンプト準備: PrepareSystems::Fit                  │
  │    (動的文脈注入 ➔ 全体トークン計測 ➔ recent::view 調整) │
  │  ・権限判定ゲート: Stage::Gate                          │
  │    (承認保留は (Ask, ForAgent, HoldOf) エンティティ生成)│
  │  ・推論・ツール反復: AgentPlugin (ReActループ)          │
  │  ・文単位先行TTS: PostUpdate (MessageReader<StreamDelta>)│
  │    (親のユーザー向け発話のみフィルタし、句点 。！？ 送出)│
  │  ・会話ログ正本: SessionLogPlugin ＋ DirStore (追記ログ)│
  │  ・MCPツール統合: McpCorePlugin ＋ AddMcpServer (ene管理)│
  └────────────────────────────────────────────────────────┘
```

#### 実装のポイント
- **状態所有権の完全一本化（二重ループの排除）**:
  - ユーザー発話、知覚センサー（目）の要約、タイマースケジュール、作業エージェント（手）からの報告は、すべて `world.trigger(Deliver::new(agent, text, DeliveryMode))` を通じてECS Worldへ直接投入される。
  - **公式契約に準拠した承認待機（Ask / HoldOf / Decision）**:
    - `Stage::Gate` で承認保留を行う際、ツール呼び出し対象エンティティへ勝手な単一フラグを付与するのではなく、Rig公式契約通り保留エンティティ `(Ask, ForAgent(agent), HoldOf(call_entity))` をWorld内に生成する。
    - **承認質問の回答確定とツールの保留解除**:
      - ユーザーが承認した場合：対象ツールエンティティ（`call_entity`）へ **`Approved` コンポーネントを付与**（`commands.entity(call_entity).insert(Approved)`）した上で、承認質問エンティティ（およびそれが持つ `HoldOf`）を `despawn` する。これにより、後続のゲートシステム（`Without<Approved>` クエリ）が同一ツール呼び出しを再保留するのを防ぎつつ、ツール本体の `Decision` は未決定（undecided）のまま維持されるため、実行ステージで安全にツール実行が開始される（ツールを実行せずに勝手に成功結果を与えてしまう事故を防止）。
      - ユーザーが拒否した場合：対象ツールエンティティの `Decision` に対して **`decision.decide(Decider::Person, Outcome::refused("User denied"))`** を確定させた上で、承認質問エンティティの `HoldOf` を解除（`despawn`）する。カーネルはツールを実行せず拒否結果（Denied）として安全に確定・決済する。
      - これにより、他の無関係な保留理由を巻き込んで解除することなく、単一の契約に従って承認・拒否・タイムアウトが整合して終端する。
  - **ストリーム文字片の厳格なフィルタリング**:
    - `PostUpdate` で `MessageReader<StreamDelta>` を購読するシステムは、**親コンパニオンのユーザー向け応答ターン（`turn.is_user_facing && turn.agent_id == companion_id`）の文字片のみを対象**として句点（`。`、`！`、`？`、改行）ごとにテキストを切り出し、TTS非同期タスクへ送信する。
    - プラグイン自身の内部モデル呼び出し、Compaction要約、記憶整理、推論思考（thinking）断片、子エージェントの内部ログ等は一切TTSへ流さない。
- **会話ログと要約の責務分担**:
  - **会話セッションログと要約（短期正本）**: `rig-ecs` の `SessionLogPlugin` が要求する `SessionStore(Arc<dyn Store>)` に合わせ、追記型線形ログである `DirStore` を正本とする。Compaction要約も `apply_summary` により `Compacted` として `SessionLog` 内に直接記録される。
  - **動的文脈の非永続注入と Fit による最終全体予算管理**:
    - Pinned Memory、想起記憶、現在の心、知覚要約などの変動情報は `SessionLog` には書き込まず、インメモリの要求（Request）の末尾（最新ユーザー発話直前、Dynamic Tail）へ一時的に挿入する。
    - その後、`PrepareSystems::Fit` を**動的文脈を含む要求全体に対して実行**し、合計トークン数を評価した上で履歴ウィンドウ（`recent::view`）を切り詰める。これにより、動的文脈を追加したことでトークン上限を超過する事故を決定論的に防止する。
  - **長期エピソード記憶・心**: SQLite（`host.sqlite3`）に永続化し、長期的なエピソード記憶、心（感情・関係性）、および長期記憶の抽出進捗カーソル（`consolidation_state`）を管理する。
- **MCP設定のene単一所有権（McpCorePlugin ＋ AddMcpServer）**:
  - `rig_mcp_client::McpPlugin` は `RIG_HOME/mcp.json` 探索や独自の承認機構を持つため、ene側の設定管理と二重化する。
  - したがって、設定ファイルの自動探索を行わない **`McpCorePlugin`** を採用し、eneのDB・UIで認可されたMCPサーバーのみを **`AddMcpServer`** イベントによりプログラム的にECS Worldへ登録する。MCPサーバーの接続・切断・ツール公開は公式MCP統合に委譲しつつ、設定の正本はeneが単一所有する。

#### 実装例・一次情報
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)
- [`rig-ecs` Documentation](https://docs.rs/rig-ecs)（Bevy ECSベースのエージェント実行プラグイン・SessionLog）

---

### C. OSサンドボックス：ネイティブ抽象化

主要3OS（Linux / Windows / macOS）に対して、共通の抽象インターフェースを提供し、各OSの最適なセキュリティ機構を駆動する（ライセンスの制約がある外部ラッパーは排し、公式・パーミッシブなクレートに一本化）。

| クレート / 機構 | 担当責務 | 適用対象 |
| :--- | :--- | :--- |
| **`process-wrap`** | 子プロセスの生成、プロセスツリー/Job Object管理、kill-on-drop | 全OS共通（プロセスライフサイクルとタイムアウト強制終了） |
| **`landlock` ＋ `bwrap`** | ファイルシステムパス制限、UNIXソケットプロキシ経由の通信制御 | Linux（公式landlockクレート MIT/Apache-2.0 ＋ bwrap コマンドラッパー） |
| **`AppContainer`** | 専用SIDによるACLファイル隔離、ネットワークCapability制限 | Windows（Job Object / AppContainer による資源制限と隔離） |
| **`Seatbelt` (`sandbox-exec`)** | SBPLプロファイルによるファイル・プロセス制限 | macOS（システム標準のサンドボックス機構） |
| **`cap-std`** | ホスト（Rust）コード自身のファイル操作におけるパストラバーサル（`../`）防止 | 全OS共通（ホスト自身の安全なファイルI/O） |

#### 実装例・一次情報
- [`cap-std` Docs.rs](https://docs.rs/cap-std/latest/cap_std/)（オープンディレクトリをルートとする安全なファイル操作）
- [`process-wrap` Docs.rs](https://docs.rs/process-wrap/latest/process_wrap/)（Unix process-group、Windows Job Object、kill-on-dropのマルチプラットフォーム対応）
- [`landlock` Docs.rs](https://docs.rs/landlock)（Linux公式LandlockセキュリティモジュールのRustバインディング）
- [Microsoft Learn: AppContainer for Windows Developers](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)

---

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

### E. アバター描画：`apps/ene-avatar`（Bevy + `ene-vrm`）をそのまま採用

- **既存実装（`apps/ene-avatar`）の直接利用**:
  - アバター描画プロセスには、リポジトリ内に既に実装済みの `apps/ene-avatar` をそのまま採用する。
  - `apps/ene-avatar` は `bevy = "=0.20.0"` および専用の `ene-vrm`（`crates/ene-vrm`）により、VRM 1.0モデルロード、VRMA/モーフアニメーション再生、および表情ブレンドシェイプを完結して実装している。
- **実証済みの機能構成**:
  1. **クロスプラットフォーム透過デスクトップウィンドウ**:
     - Linux: Wayland layer-shell（`exwlshellev = "=0.20.1"` による透過・背後クリック透過）。
     - Windows: Win32 API（`windows = "0.62"`）によるレイヤード透過ウィンドウ（Layered Window）。
  2. **ローカルRMS音量解析による即時口パク（リップシンク）**:
     - 再生中PCM音声のRMS振幅をフレーム単位で解析し、VRMブレンドシェイプ（`aa` 等）を即時変位。
  3. **標準入出力 JSON Lines IPC制御**:
     - 親エージェントからの `body_motion` 指示（モーション名・表情）を標準入力で受信し、滑らかにクロスフェード再生。
