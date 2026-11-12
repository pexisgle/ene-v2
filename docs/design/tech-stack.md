# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・音声](voice-avatar.md)、[権限・Sandbox](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`rusqlite` (bundled, WAL mode) ＋ FTS5 trigram ＋ Rustコサイン類似度** | C言語SQLiteを同梱（bundled）して静的コンパイル・リンクし、外部DBデーモン不要で単一SQLiteファイル（`host.sqlite3`）で完結。ECSループスレッドで同期接続を直接保持し、1ms未満のPK検索・FTS5・BLOBコサイン類似度・ルール照合を遅延なく実行。記憶整理や埋め込み生成等の重い非同期処理のみバックグラウンドタスクへ分離 |
| **会話ログ・要約正本** | **`rig_memory::DirStore` (`rig-memory` feature `fs`)** | 公式の `rig-ecs` が推奨する同期ファイルストア。プロセス間ファイルロック、破損末尾行の自動退避、画像Blob管理を公式実装に委譲し、アダプタ実装ゼロ行で完結。受理レシート（`request_key`）も `Conversation::add_aside` でセッションログ内に記録するため、SQLiteとの2相不整合は原理的に発生しない |
| **エージェント実行・推論** | **`rig-ecs`（エージェントステートマシン）＋ Tokio（ホスト非同期I/Oランタイム）** | 親・内蔵の子の推論・ツール反復を `rig-ecs` のECSステージ・Observerに一本化。CLIハーネス用プラグイン（`rig-basics` / `rig-mcp-client`）を排し、純粋なECSプリミティブ（`Stage::Gate` ＋ `HoldOf` ＋ `Approved`）と `world.spawn_tool` による動的MCP登録を採用。TokioはConnect-RPCサーバーや外部非同期I/Oに徹する |
| **OSサンドボックス** | **OSネイティブ隔離 (`cap-std`, `process-wrap`, `landlock`, `bwrap`)** | Linux（Landlock ＋ bwrap netns分離 ＋ UNIXソケットプロキシ）、Windows（AppContainer / Job Objects）、macOS（Seatbelt）を統一制御。非対応環境では明示的警告・承認で縮退 |
| **デスクトップクライアント** | **`Slint` (GUI) ＋ `cpal` (音声I/O) ＋ OSウィンドウ監視フック** | 軽量ネイティブGUIツールキットSlintによる軽快な設定・承認UI、cpalによる低遅延音声キャプチャ・再生、OSフックによる知覚センシング（目） |
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
  - 会話履歴（`SessionLog`）および要約状態（`Compacted`）は、公式 `rig-ecs` がネイティブに要求する同期ストア `rig_memory::DirStore`（`<state_dir>/rig/`）をそのまま採用。
  - プロセス間ファイルロック、破損末尾行の自動退避、画像等のバイナリBlob管理は公式実装に完全に委譲され、アダプタの自作はゼロ行（0行）。
  - クライアントからの会話受理レシート（`request_key` 重複排除）は SQLite ではなく `Conversation::add_aside`（SessionLog 内）に直接記録するため、ファイルシステムと SQLite 間の 2相コミットやクラッシュ時の不整合復旧問題が原理的に発生しない。
  - SQLite（`host.sqlite3`）は、心・長期エピソード記憶・認知的関係性・設定・作業・承認要求のACID管理に専念する。
- **FTS5 trigram ＋ ベクトル検索（BLOBコサイン類似度）**:
  - 日本語の分かち書きエンジン不要の **FTS5 trigram（3-gram）** インデックスにより、助詞や固有名詞を含む高精度な語句検索を実現。
  - 選択された埋め込みモデル（ローカル `fastembed` / クラウド `text-embedding-3-small` 等）のベクトルを BLOB として保存し、Rust内のスライスコサイン類似度計算と FTS5 の順位を Rust 側で RRF（Reciprocal Rank Fusion）統合（外部C拡張に依存せず安定・最小化）。
- **1ホップ認知関係性（エッジテーブル）**:
  - 認知的関係性（`companion->remembers->memory`, `memory->cited_in->turn`, `memory->supersedes->memory`）は高々1ホップの参照であるため、シンプルな `memory_edges(from_id, rel, to_id)` テーブルで高速・堅牢に表現可能。

#### 実装例・一次情報
- **依存定義 (`Cargo.toml`)**:
  ```toml
  rusqlite = { version = "0.40", features = ["bundled"] }
  rig-core = "0.46"
  rig-ecs = "0.46"
  rig-memory = { version = "0.46", features = ["fs"] }
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  ```
- **スキーマ定義とハイブリッド検索（Rust）**:
  ```rust
  // 初期化スキーマ（FTS5 trigram + 記憶テーブル + エッジ + 整理カーソル + 承認状態）
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
  "#)?;
  ```
- **一次ドキュメント**:
  - [SQLite FTS5 Extension Documentation](https://www.sqlite.org/fts5.html)
  - [`rusqlite` Documentation (docs.rs)](https://docs.rs/rusqlite/latest/rusqlite/)

---

### B. エージェントループ：純粋な `rig-ecs` イベント駆動アーキテクチャ

エージェント状態や実行ループをTokio側で自作せず、公式の `rig-ecs` にエージェント実行・状態管理・会話ログ正本（`DirStore`）を一本化する。TokioはConnect-RPCサーバーと外部非同期I/O（ストリーミング通信・子プロセス監視）の非同期ランタイムに専念する。

CLIコーディングエージェント用の `rig-basics`（`ApprovalPlugin` 等）や `rig-mcp-client` は採用せず、純粋な `rig-ecs` プリミティブ（`Stage::Gate` ＋ `HoldOf` ＋ `Approved`）と動的ツール登録で最小構成を実現する。

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
  │  ・権限ゲート: Stage::Gate (HoldOf 生成 ➔ Approved 付与) │
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
- **純粋なECSプリミティブによる承認待機（`HoldOf` / `Approved`）**:
  - CLIハーネス用の `ApprovalPlugin`（内部非公開マーカー `Approving` を前提とする機構）を排除し、`rig-ecs` のコアコンポーネントのみで完結させる：
    1. **保留**: `Stage::Gate` システムにおいて、未承認のツール呼び出しエンティティ（`call_entity`）を検知した場合、`commands.spawn(HoldOf(call_entity))` を投入してReActループを安全に待機させる。
    2. **登録**: 保留した操作の引数ハッシュを計算し、`approval_requests` テーブルおよび未決リストへ登録。クライアントへ `ApprovalRequested` 通知を発行。
    3. **承認確定（RPC ➔ ECS）**: ユーザーが承認した場合、Connect-RPC（`SettingsService.SubmitApproval`）ハンドラからECS Worldへコマンドをディスパッチし、`commands.entity(call_entity).insert(Approved)` を付与した上で `HoldOf` エンティティを `despawn` する。後続の `Stage::Gate` は `Approved` コンポーネントを検知して通過させる。
    4. **拒否確定**: ユーザーが拒否した場合、対象ツールの `Decision` を拒否（`decision.decide(Decider::Person, Outcome::refused("User denied"))`）として確定し、`HoldOf` エンティティを `despawn` する。
    5. **記録**: 承認結果は `host.sqlite3` の `approval_requests` テーブルへ永続化する。会話ログ（`SessionLog`）を汚染しない。
- **動的文脈の非永続注入（Dynamic Tail）と KV Prefix Cache 最大化**:
  - Anthropic/OpenAI/Gemini の KVキャッシュ（Prefix Caching）を最大限に活かすため、プロンプト構造を以下に固定する：
    $$\text{[System Prompt]} \longrightarrow \text{[Past Dialogue History]} \longrightarrow \text{[Dynamic Tail in User\_N]} \longrightarrow \text{[User Speech]}$$
  - Pinned Memory、想起エピソード記憶、現在の心（`MindState`）、知覚要約（Perception Summary）、現在日時は、永続会話ログ（`SessionLog`）には一切書き込まず、最新のユーザー発話ブロック（`User_N`）の末尾コンテキスト枠に一時注入する。
  - これにより、`[System Prompt, ..., Assistant_{N-1}]` までの過去ログ全体が完全に不変のプレフィックスとして保持され、>90% のキャッシュヒット率を達成する。
  - `PrepareSystems::Edit` で動的文脈を注入し、`PrepareSystems::Fit` で動的文脈を含む要求全体に対してトークン数計測と `recent::view` 窓切り詰めを実行する。
- **ストリーム文字片の厳格なフィルタリング**:
  - `Stage::Review`（AgentPass 内）で `MessageReader<StreamDelta>` を購読するシステムは、**`delta.agent` が親コンパニオンの Entity と一致するテキスト文字片のみを対象**として句点（`。`、`！`、`？`、改行）ごとにテキストを切り出し、TTS非同期タスクへ送信する。
  - Compaction要約、記憶整理、推論思考（thinking）断片、子エージェントの内部ログ等は一切TTSへ流さない。
- **MCPツールの動的ECS登録**:
  - 外部ファイル自動探索（`rig-mcp-client`）を排し、eneの設定DBで認可されたMCPサーバーのツール定義を `world.spawn_tool(tool_def, tool_handler, opts)`（Pure ECSツール登録）で動的エンティティとしてWorldへ追加・削除する。設定の正本はeneが単一所有する。

#### 実装例・一次情報
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)
- [`rig-ecs` Documentation](https://docs.rs/rig-ecs)（Bevy ECSベースのエージェント実行）
- [`rig-memory` Documentation](https://docs.rs/rig-memory)（DirStore・Compaction要約・Anchor）

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

---

### F. デスクトップクライアント技術スタック（Desktop Client）

ホストデーモン（`apps/ene-host`）と協調して動作するデスクトップクライアント（UI/知覚/オーディオ）の技術スタック。

| コンポーネント | 採用技術 | 選定理由と責務 |
| :--- | :--- | :--- |
| **ネイティブGUI** | **`Slint`** | ElectronやQtのような巨大なランタイム・Webブラウザエンジンを排し、Pure Rustで軽量・高速に描画されるネイティブGUIツールキットを採用。承認カード、設定画面、コンパニオン切替UIを数MBのフットプリントで瞬時に表示 |
| **オーディオ入出力** | **`cpal`** | クロスプラットフォーム（ALSA/PulseAudio/WASAPI/CoreAudio）対応の低レイテンシオーディオI/O。24kHz 16-bit mono PCMでのマイク録音・スピーカー再生を担い、再生中PCMからRMS振幅を算出して口パクIPCを駆動 |
| **知覚センサー（目）** | **OSネイティブウィンドウフック** | Linux（X11/Wayland）、Windows（`SetWinEventHook`）、macOS（Accessibility API）を通じてアクティブウィンドウのタイトルやフォーカス変化をリアルタイム検知 |
| **アバター子プロセス管理** | **標準入出力 JSON Lines IPC** | `apps/ene-avatar` を子プロセスとして起動・監視し、標準入出力経由でモーション指定、表情、リップシンク振幅をミリ秒単位で制御 |

> [!NOTE]
> **Extism (Wasm) プラグインのフェーズ位置づけ**:
> Extism (Wasm) によるサードパーティプラグイン実行環境は、後続の拡張フェーズへ延期する。Alpha/Minimal構成では、内蔵Rigツール、外部MCPサーバー（JSON-RPC）、および外部ACPコーディングエージェント（stdio）の3系統に集中し、過剰な抽象化層を排して堅牢な動作を担保する。
