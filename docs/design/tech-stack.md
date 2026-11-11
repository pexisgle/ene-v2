# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・音声](voice-avatar.md)、[権限・Sandbox](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`rusqlite` (bundled, WAL mode) ＋ FTS5 trigram ＋ `sqlite-vec`（またはコサイン類似度）** | 100% Rust/Cネイティブ、省メモリ・超軽量。FTS5 trigram全文検索とベクトル検索を単一SQLiteで完結。`rig-ecs` の同期システムから `.await` なしで直接ACIDトランザクションを実行可能 |
| **エージェント実行・推論** | **`rig-ecs`（エージェントステートマシン）＋ Tokio（ホスト非同期I/Oランタイム）** | 親・内蔵の子の推論・ツール反復・会話ログ正本（`SessionLogPlugin` + `DirStore`）を `rig-ecs` のECSステージ・Observerに一本化。TokioはConnect-RPCサーバーや外部非同期I/Oに徹し、二重ループを完全排除 |
| **OSサンドボックス** | **OSネイティブ隔離の抽象化 (`cap-std`, `process-wrap`, `sandbox_run`)** | Windows（Job Objects / AppContainer）、Linux（Landlock / bwrap）、macOS（Seatbelt）を統一インターフェースで制御。非対応環境では明示的警告・承認で縮退 |
| **アバター描画** | **`apps/ene-avatar` (`bevy` + `ene-vrm`)** | 既存資産を活用。透過デスクトップウィンドウ（Wayland / Windows）、VRM 1.0モデルロード、RMS音量によるローカル即時口パク。クライアントからの標準入出力JSON Lines制御 |
| **音声（STT/TTS）** | **プラガブル抽象インターフェース (`SttEngine` / `TtsEngine`)** | VOICEVOXやStyle-Bert-VITS2等の文単位HTTP APIから、sherpa-onnx等のストリーミングエンジン、双方向Live APIまで、各エンジンの通信仕様・機能特性に柔軟に適応 |
| **通信・RPC** | **`Connect-RPC` (`connectrpc` + `buffa` + `axum`)** | gRPC/Connect/HTTP/Web互換、高速Protobuf、Server Streaming対応 |

---

## 2. 各コンポーネントの詳細と実装例・リファレンス

### A. データベース：SQLite（Embedded `rusqlite` ＋ FTS5 ＋ `sqlite-vec`）

#### 特徴と利点
- **超軽量・ゼロ依存肥大化**:
  - `rusqlite`（bundled）により、追加の巨大DBエンジンやC++外部依存を持たず、単一のSQLiteファイル（例: `~/.local/share/ene/host.sqlite3`）で完結。
  - WAL（Write-Ahead Logging）モードにより、高い並行読み取り性能と耐障害性を両立。
- **同期ACIDトランザクションと `rig-ecs` の直結**:
  - `rig-ecs` のECSシステム内から、非同期の複雑な `.await` や型変換（SurrealValue等）を挟むことなく、同期トランザクションで記憶の作成・更新・カーソル進行を原子的（Atomic）にコミット可能。
- **FTS5 trigram ＋ ベクトル検索（`sqlite-vec` / BLOBコサイン類似度）**:
  - 日本語の分かち書きエンジン不要の **FTS5 trigram（3-gram）** インデックスにより、助詞や固有名詞を含む高精度な語句検索を実現。
  - 選択された埋め込みモデル（ローカル `fastembed` / クラウド `text-embedding-3-small` 等）のベクトルを保存し、Rust側でのRRF（Reciprocal Rank Fusion）により語句検索結果と順位融合。
- **1ホップ認知関係性（エッジテーブル）**:
  - 認知的関係性（`companion->remembers->memory`, `memory->cited_in->turn`, `memory->supersedes->memory`）は高々1ホップの参照であるため、外部キー制約およびシンプルな `edges(from_id, rel, to_id)` テーブルで高速・堅牢に表現可能。

#### 実装例・一次情報
- **依存定義 (`Cargo.toml`)**:
  ```toml
  rusqlite = { version = "0.40", features = ["bundled"] }
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  ```
- **スキーマ定義とハイブリッド検索（Rust）**:
  ```rust
  // 初期化スキーマ（FTS5 trigram + 記憶テーブル + エッジ）
  conn.execute_batch(r#"
      PRAGMA journal_mode = WAL;
      
      CREATE TABLE IF NOT EXISTS memories (
          id TEXT PRIMARY KEY,
          owner TEXT NOT NULL,
          audiences TEXT NOT NULL, -- JSON array
          content TEXT NOT NULL,
          importance INTEGER NOT NULL CHECK (importance >= 0 AND importance <= 10),
          occurred_at TEXT,
          recorded_at TEXT NOT NULL,
          last_accessed_at TEXT,
          memory_revision INTEGER NOT NULL DEFAULT 1,
          emotion TEXT,
          source_context TEXT,     -- JSON object
          embedding BLOB,          -- f32 array bytes
          embedding_model TEXT,
          embedding_dimensions INTEGER
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
          relation TEXT NOT NULL, -- 'remembers', 'cited_in', 'supersedes'
          to_id TEXT NOT NULL,
          created_at TEXT NOT NULL,
          PRIMARY KEY (from_id, relation, to_id)
      );
  "#)?;
  ```
- **一次ドキュメント**:
  - [SQLite FTS5 Extension Documentation](https://www.sqlite.org/fts5.html)
  - [`rusqlite` Documentation (docs.rs)](https://docs.rs/rusqlite/latest/rusqlite/)

---

### B. エージェントループ：`rig-ecs` 単一イベント駆動アーキテクチャ

エージェント状態や実行ループをTokio側で自作せず、Rig公式の `rig-ecs` にエージェント実行・状態管理・会話ログ正本を一本化する。TokioはRPCサーバー（axum + connectrpc）と外部非同期I/O（ストリーミング通信・子プロセス）の非同期ランタイムとして機能する。

```text
  ┌────────────────────────────────────────────────────────┐
  │ 【ホスト外部I/O・RPCサーバー】（Tokio 非同期ランタイム） │
  │  ・Connect-RPC 受付（ChatService / WorkService / etc.） │
  │  ・外部ACPエージェントプロセスの標準入出力監視          │
  │  ・マイク音声入力 / TTSエンジンへのHTTP・ストリーム送受信│
  └───────────────────────────┬────────────────────────────┘
                              │ world.trigger(Deliver::new(...))
                              ▼
  ┌────────────────────────────────────────────────────────┐
  │ 【エージェント実行・状態管理】（rig-ecs 単一World基盤）  │
  │  ・入力投入: Deliver(agent, text, DeliveryMode)         │
  │  ・プロンプト準備: PrepareSystems::Fit                  │
  │    (窓切り出し recent::view + 先頭に既存Compacted要約)   │
  │  ・権限判定ゲート: Stage::Gate                          │
  │    (承認保留は HoldOf(call_entity) を付与して待機)      │
  │  ・推論・ツール反復: AgentPlugin (ReActループ)          │
  │  ・文単位先行TTS: Stage::Review                         │
  │    (StreamDelta を購読し、句点 。！？ でTTSキューへ投入)│
  │  ・会話ログ正本: SessionLogPlugin ＋ DirStore (追記ログ)│
  │  ・MCPツール統合: WorldToolsExt::spawn_tool             │
  └────────────────────────────────────────────────────────┘
```

#### 実装のポイント
- **状態所有権の完全一本化（二重ループの排除）**:
  - ユーザー発話、知覚センサー（目）の要約、タイマースケジュール、作業エージェント（手）からの報告は、すべて `world.trigger(Deliver::new(agent, text, DeliveryMode))` を通じてECS Worldへ直接投入される。
  - 承認待ち（Approval Hold）は `Stage::Gate` でコール対象エンティティに `HoldOf` コンポーネントを付与するだけで非同期保留され、クライアントから承認RPCが届いた際に `HoldOf` を削除することで推論が再開する。
  - 文単位先行TTSは、`Stage::Review` で `StreamDelta` の文字片を購読するシステムが句点（`。`、`！`、`？`、改行）ごとにテキストを切り出し、TTS非同期タスクへ送信する。
- **会話ログと要約の責務分担**:
  - **会話セッションログと要約（短期正本）**: `rig-ecs` の `SessionLogPlugin` が要求する `SessionStore(Arc<dyn Store>)` に合わせ、追記型線形ログである `DirStore` を正本とする。Compaction要約も `apply_summary` により `Compacted` として `SessionLog` 内に直接記録される。
  - **長期エピソード記憶・心**: SQLite（`host.sqlite3`）に同期永続化し、長期的なエピソード記憶、心（感情・関係性）、および長期記憶の抽出進捗カーソル（`processed_turns_cursor`）を管理する。
- **MCPツールの直接登録**:
  - `rig-mcp-client` / `WorldToolsExt::spawn_tool` を活用し、MCPツールを独自変換層なしに直接ECS Worldに登録する。

#### 実装例・一次情報
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)
- [`rig-ecs` Documentation](https://docs.rs/rig-ecs)（Bevy ECSベースのエージェント実行プラグイン・SessionLog）

---

### C. OSサンドボックス：ネイティブ抽象化

主要3OS（Linux / Windows / macOS）に対して、共通の抽象インターフェースを提供し、各OSの最適なセキュリティ機構を駆動する。

| クレート / 機構 | 担当責務 | 適用対象 |
| :--- | :--- | :--- |
| **`process-wrap`** | 子プロセスの生成、プロセスツリー/Job Object管理、kill-on-drop | 全OS共通（プロセスライフサイクルとタイムアウト強制終了） |
| **`sandbox-run`** | 子プロセスに対するOSネイティブ隔離ポリシーの適用 | Linux（Landlock/bwrap）、macOS（Seatbelt） |
| **`AppContainer`** | 専用SIDによるACLファイル隔離、ネットワークCapability制限 | Windows（Alpha初期はJob Objectによる資源制限から段階的拡張） |
| **`cap-std`** | ホスト（Rust）コード自身のファイル操作におけるパストラバーサル（`../`）防止 | 全OS共通（ホスト自身の安全なファイルI/O） |

#### 実装例・一次情報
- [`cap-std` Docs.rs](https://docs.rs/cap-std/latest/cap_std/)（オープンディレクトリをルートとする安全なファイル操作）
- [`process-wrap` Docs.rs](https://docs.rs/process-wrap/latest/process_wrap/)（Unix process-group、Windows Job Object、kill-on-dropのマルチプラットフォーム対応）
- [`sandbox_run` Docs.rs](https://docs.rs/sandbox_run)（Linux LandlockとmacOS SandboxのCommandラッパー）
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

### E. アバター描画：`apps/ene-avatar`（Bevy + `ene-vrm`）の活用

- **既存資産の活用**:
  - `apps/ene-avatar` は既に Bevy + `ene-vrm` によるVRM 1.0モデルロード、モーフターゲット（表情・口パク）、および透過ウィンドウ（Wayland/Windows）の基盤を持つ。
- **クロスプラットフォーム対応**:
  - Windows: Win32 APIによるレイヤード透過ウィンドウ。
  - Linux: Wayland layer-shell。
  - macOS: `bevy_winit` の `Window` 設定（`transparent: true`, `decorations: false`）。
- **ローカルRMS音量解析による即時口パク**:
  - 音声再生のPCM振幅（RMS）をBevyのFrame Updateで直接計測し、VRMブレンドシェイプ（`aa` 等）を変位。サーバー往復遅延ゼロで口パクが連動。
