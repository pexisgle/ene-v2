# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・音声](voice-avatar.md)、[権限・Sandbox](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`SurrealDB` (Embedded SurrealKV)** | 100% Rustネイティブ、C++ビルド不要。心・記憶・関係性をグラフで表現し、HNSWベクトル検索、BM25全文検索、ACIDトランザクションを単一エンジンで完結 |
| **エージェント実行・推論** | **`rig-ecs` ＋ Tokio非同期ステートマシン** | 親・内蔵の子の推論・ツール反復・会話ログ正本（`SessionLogPlugin` + `DirStore`）を `rig-ecs` に委ね、外側のTokio非同期ステートマシンで音声入出力、アイドル管理、複数コンパニオン調停、ACP WorkSession接続維持を制御 |
| **OSサンドボックス** | **OSネイティブ隔離の抽象化 (`cap-std`, `process-wrap`, `sandbox_run`)** | Windows（AppContainer / Job Objects）、Linux（Landlock / bwrap）、macOS（Seatbelt）を統一インターフェースで制御。非対応環境では明示的警告・承認で縮退 |
| **アバター描画** | **`apps/ene-avatar` (`bevy` + `ene-vrm`)** | 既存資産を活用。透過デスクトップウィンドウ（Wayland / Windows）、VRM 1.0モデルロード、RMS音量によるローカル即時口パク |
| **音声（STT/TTS）** | **プラガブル抽象インターフェース (`SttEngine` / `TtsEngine`)** | VOICEVOXやStyle-Bert-VITS2等の文単位HTTP APIから、sherpa-onnx等のストリーミングエンジン、双方向Live APIまで、各エンジンの通信仕様・機能特性に柔軟に適応 |
| **通信・RPC** | **`Connect-RPC` (`connectrpc` + `buffa` + `axum`)** | gRPC/Connect/HTTP/Web互換、高速Protobuf、Server Streaming対応 |

---

## 2. 各コンポーネントの詳細と実装例・リファレンス

### A. データベース：SurrealDB（Embedded SurrealKV）

#### 特徴と利点
- **Pure Rust（SurrealKV）**:
  - `surrealdb` は完全Rust製の **SurrealKV**（`kv-surrealkv`）をサポート。
  - C++コンパイラや追加ランタイム不要で、Windows・macOS・Linux共通の単一フォルダ（例: `~/.local/share/ene/data`）に埋め込み永続化。
- **グラフ ＋ HNSWベクトル ＋ ドキュメントの統合**:
  - **エピソード記憶のベクトル検索**: HNSW（Hierarchical Navigable Small World）インデックスによる高速コサイン類似度検索。
  - **関係性のグラフ表現**: `companion->remembers->memory`、`memory->cited_in->turn` など、認知モデルの関係性をグラフエッジで自然に表現。
  - **ACID保証**: 記憶の追加・更新、引用実績、処理済みカーソルを同一トランザクションで確定。

#### 実装例・一次情報
- **依存定義 (`Cargo.toml`)**:
  ```toml
  surrealdb = { version = "3.0", features = ["kv-surrealkv"] }
  tokio = { version = "1", features = ["full"] }
  serde = { version = "1", features = ["derive"] }
  ```
- **初期化とクエリ例**:
  ```rust
  use surrealdb::Surreal;
  use surrealdb::engine::local::SurrealKv;

  // ローカルフォルダに組み込み初期化
  let db = Surreal::new::<SurrealKv>("./data/ene.db").await?;
  db.use_ns("ene").use_db("main").await?;

  // HNSW ベクトルインデックス・日本語2-gram全文検索インデックスの定義
  // ※インデックス次元数は選択されたモデル（ローカル/クラウド）に合わせて動的に設定
  db.query(r#"
      DEFINE TABLE memory SCHEMAFULL;
      DEFINE FIELD content ON TABLE memory TYPE string;
      DEFINE FIELD embedding ON TABLE memory TYPE option<array<float>>;
      DEFINE INDEX idx_memory_embedding ON TABLE memory FIELDS embedding 
          HNSW DIMENSION 1536 DIST COSINE TYPE F32;
      
      DEFINE ANALYZER japanese_analyzer TOKENIZERS blank,punct FILTERS lowercase,ngram(2,2);
      DEFINE INDEX idx_memory_fulltext ON TABLE memory FIELDS content 
          FULLTEXT ANALYZER japanese_analyzer;
  "#).await?.check()?;

  // 代表的なSurrealQLハイブリッド検索クエリ（ベクトル検索と全文検索の独立取得とRRF順位統合）
  let _results = db.query(r#"
      LET $semantic = (
          SELECT id, content, importance, occurred_at, last_accessed_at,
                 vector::similarity::cosine(embedding, $query_vec) AS score
          FROM memory
          WHERE embedding <|20, 40|> $query_vec
          ORDER BY score DESC
          LIMIT 20
      );
      LET $lexical = (
          SELECT id, content, importance, occurred_at, last_accessed_at,
                 search::score(1) AS score
          FROM memory
          WHERE content @1@ $query_text
          ORDER BY score DESC
          LIMIT 20
      );
      RETURN search::rrf([$semantic, $lexical], 20, 60);
  "#).bind(("query_vec", vec![0.0f32; 1536])).bind(("query_text", "好きなこと")).await?.check()?;
  ```
- **一次ドキュメント**:
  - [SurrealDB Embedded Rust SDK Guide](https://surrealdb.com/docs/surrealdb/integration/sdks/rust)
  - [SurrealDB Vector Search Documentation](https://surrealdb.com/docs/surrealql/functions/database/vector)
  - [SurrealDB Index Statements](https://surrealdb.com/docs/reference/query-language/statements/define/indexes)

---

### B. エージェントループ：ハイブリッド・ステートマシン

エージェントループをゼロから自作せず、Rig公式の実行基盤（`rig-ecs`、`rig-memory`、`rig-rmcp`）を活用し、ene固有のライフサイクル制御（音声・知覚・アイドル）と自然に連携させるハイブリッド設計。

```text
  ┌────────────────────────────────────────────────────────┐
  │ 【外側：ene-host 統括制御】（Tokio非同期ステートマシン） │
  │  ・マイク音声入力・ストリーミングSTT監視               │
  │  ・厳密なアイドル検知（ユーザー無操作時間計測・独り言除外）│
  │  ・複数コンパニオン調停（発話フロア・作業自律協調）    │
  │  ・LLM出力テキストストリームの文単位切り出しと先行TTS  │
  │  ・外部ACP WorkSession のプロセス接続維持              │
  └───────────────────────────┬────────────────────────────┘
                              │ ユーザー入力・拡張フック
                              ▼
  ┌────────────────────────────────────────────────────────┐
  │ 【内側：エージェント実行・会話管理】（rig-ecs 基盤）     │
  │  ・エージェントループ（rig-ecs AgentPlugin）           │
  │    モデル通信、ツール反復（ReAct）、構造化出力         │
  │  ・会話ログ正本（rig-ecs SessionLogPlugin + DirStore） │
  │    高速な追記型線形ログ（同期 Store 契約に完全準拠）    │
  │  ・MCPツール統合（rig-rmcp）                           │
  │    McpConnection ➔ McpTool ➔ DynamicTool 直接登録     │
  └────────────────────────────────────────────────────────┘
```

#### 実装のポイント
- **実装量の劇的削減と型契約の整合**:
  - 親エージェントおよび内蔵の子エージェントの推論・ツール反復（ReActループ）、引数パース、ツール結果のモデル返却は `rig-ecs` の `AgentPlugin` が担うため、独自エージェントループを自作する必要はない。
  - **保存の責務分担**:
    - **会話セッションログと要約（短期正本）**: `rig-ecs` の `SessionLogPlugin` が要求する `SessionStore(Arc<dyn Store>)`（同期 `Store`）に合わせ、高速・堅牢な追記型線形ログである `DirStore` をそのまま正本として採用する。Compaction要約も `apply_summary` により `Compacted` として `SessionLog` 内に直接記録されるため、SurrealDBへ別途二重保存する必要はない。
    - **長期記憶・心**: SurrealDB（SurrealKV）に非同期永続化し、長期的なエピソード記憶、心（感情・関係性）、HNSWベクトル検索、および長期記憶の抽出進捗（`processed_turns_cursor`）を管理する。
  - MCPツール連携は `rig-rmcp` を活用し、`McpConnection` から取得したツールを Rig の `DynamicTool` として直接登録する（独自のアダプター層を作らない）。
- **完全な外側制御権**:
  - アイドル時間計測、複数コンパニオン間の譲り合い、音声再生と割り込みバッファ破棄（Flush）、ACP WorkSession接続維持は、外側のTokioタスクが非同期に制御する。

#### 実装例・一次情報
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)
- [`rig-ecs` Documentation](https://docs.rs/rig-ecs)（Bevy ECSベースのエージェント実行プラグイン・SessionLog）
- [`rig-rmcp` Documentation](https://docs.rs/rig-rmcp)（MCPサーバー接続とツール変換）

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
#[async_trait::async_trait]
pub trait SttEngine: Send + Sync {
    /// 音声ストリームチャンクを受け取り、中間テキストまたは確定テキストを返す
    async fn process_chunk(&mut self, pcm_chunk: &[f32]) -> Result<Option<SttResult>, SttError>;
}

#[async_trait::async_trait]
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
