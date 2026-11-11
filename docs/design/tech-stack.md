# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・音声](voice-avatar.md)、[権限・Sandbox](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`SurrealDB` (Embedded SurrealKV)** | 100% Rustネイティブ、C++ビルド不要。心・記憶・関係性をグラフで表現し、HNSWベクトル検索、BM25全文検索、ACIDトランザクションを単一エンジンで完結 |
| **エージェント通信・推論** | **`rig-core` ＋ Tokio非同期ステートマシン** | OpenAI / Anthropic / Gemini / OpenRouter 等の通信・ツール定義（`#[derive(Tool)]`）・ストリーミングパースを `rig-core` に委ね、会話ターンやアイドル管理・割り込みをTokio非同期ステートマシンで直感的に制御 |
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
  ```
- **一次ドキュメント**:
  - [SurrealDB Embedded Rust SDK Guide](https://surrealdb.com/docs/surrealdb/integration/sdks/rust)
  - [SurrealDB Vector Search Documentation](https://surrealdb.com/docs/surrealql/functions/database/vector)
  - [SurrealDB Index Statements](https://surrealdb.com/docs/reference/query-language/statements/define/indexes)

---

### B. エージェントループ：ハイブリッド・ステートマシン

「完全自作による実装量の爆発」を避けつつ、「フレームワークの制約で柔軟な割り込みや非同期制御が書けない問題」を解消するハイブリッド設計。

```text
  ┌────────────────────────────────────────────────────────┐
  │ 【LLM通信・ツール定義レイヤー】（rig-core 活用）         │
  │  ・OpenAI, Anthropic, Gemini, Groq, OpenRouter 等の通信│
  │  ・型安全なツール定義（#[tool] / Tool トレイト）       │
  │  ・JSON Schema生成、SSEストリームパース                │
  └───────────────────────────┬────────────────────────────┘
                              │ 抽象プロバイダー / ツール呼び出し
                              ▼
  ┌────────────────────────────────────────────────────────┐
  │ 【エージェントループ・状態機械】（Tokio非同期ステートマシン） │
  │  ・追記型線形ログ ＋ スナップショットによる分岐管理    │
  │  ・厳密なアイドル検知（ユーザー操作不在計測・独り言除外） │
  │  ・音声パイプライン（文単位先行合成 / ストリーミング） │
  │  ・マルチモーダル先着優先承認（音声/画面ロック）         │
  └────────────────────────────────────────────────────────┘
```

#### 実装のポイント
- **実装量の抑制**: 各社LLMのHTTPヘッダー、ストリーミングパース、ツール呼び出し引数のJSONパース、リトライは `rig-core` が担うため、ゼロから低レベルなAPIクライアントを書く必要はない。
- **完全な制御権**: 会話ターンやCompaction、先行TTS合成の管理をTokio非同期タスクとして実装。バックグラウンド要約中の割り込みや、ユーザー発話検知時の即時バッファ破棄（Flush）を直感的に書くことができる。

#### 実装例・一次情報
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)（Rust製LLMアプリケーションフレームワーク・ツールマクロ）

---

### C. OSサンドボックス：ネイティブ抽象化

主要3OS（Linux / Windows / macOS）に対して、共通の抽象インターフェースを提供し、各OSの最適なセキュリティ機構を駆動する。

| OS | 適用する隔離機構 | 制御内容 |
| :--- | :--- | :--- |
| **Linux** | **`Landlock`** ＋ **`bubblewrap`** | ファイルシステムパス制限、TCPポート制限、ネットワーク名前空間による完全遮断 |
| **Windows** | **`AppContainer`** ＋ **`Job Objects`** | 専用SIDによるACLファイル隔離、ネットワークCapability制限、CPU/メモリ/プロセス生成制限 |
| **macOS** | **`Apple Sandbox (Seatbelt)`** | `sandbox-exec` / SBPLプロファイルによるファイルパス・ネットワーク・バイナリ実行制限 |
| **全OS共通** | **`cap-std`** | ホスト（Rust）コード自身のファイル操作におけるパストラバーサル（`../`）防止 |

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
