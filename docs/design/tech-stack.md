# 技術スタック選定・ライブラリ・実装リファレンス設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[作業相手API](work.md)、[知覚・自律ループ・音声](loop-perception-voice.md)、[権限](permissions.md)を支える具体的な技術スタック・採用ライブラリ・実装例の所在を定義する。

---

## 1. 決定した技術スタック一覧

| 領域 | 採用技術 / ライブラリ | 選定理由 |
| :--- | :--- | :--- |
| **データベース** | **`SurrealDB` (Embedded SurrealKV)** | 100% Rustネイティブ、C++ビルド不要。心・記憶・関係性をグラフ（Graph）で表現し、HNSWベクトル検索とACIDトランザクションを単一エンジンで完結 |
| **エージェントループ** | **LLM通信基盤 (`rig-core` / `genai`) ＋ Tokio非同期ステートマシン** | API通信・ツールパースの車輪の再発明を避けつつ、ECSフレーム同期や線形配列の制約から脱却し、分岐（DAG）・割り込み・全二重を自在に制御 |
| **OSサンドボックス** | **OSネイティブ隔離の抽象化 (`cap-std`, `process-wrap`, `sandbox_run`)** | Windows / Linux / macOS の主要3OSに対応。LinuxのLandlock/bwrap、WindowsのJob Objects/Tokens、macOSのSeatbeltを薄くラップ |
| **アバター描画** | **`apps/ene-avatar` (`bevy` + `ene-vrm`)** | 既存資産を活用。透過デスクトップウィンドウ、VRM 1.0モデルロード、RMS音量によるゼロ遅延口パク |
| **音声（STT/TTS）** | **プラガブル抽象インターフェース (`SttEngine` / `TtsEngine`)** | 最初は代表的最小セット（VOICEVOX / ローカルWhisper / 高速API）で動かし、後にsherpa-onnxやElevenLabs等へ自由に差し替え可能 |
| **通信・RPC** | **`Connect-RPC` (`connectrpc` + `buffa` + `axum`)** | gRPC/Connect/HTTP/Web互換、高速Protobuf、双方向ストリーミング対応 |

---

## 2. 各コンポーネントの詳細と実装例・リファレンス

### A. データベース：SurrealDB（Embedded SurrealKV）

#### 特徴と利点
- **Pure Rust（SurrealKV）**:
  - `surrealdb` はバージョン4.0以降、従来のRocksDB（C++依存）に代わり完全Rust製の **SurrealKV**（`kv-surrealkv`）を標準採用。
  - C++コンパイラやビルド依存がなく、Windows・macOS・Linuxで同一の単一フォルダ（例: `~/.local/share/ene/data`）に埋め込み動作可能。
- **グラフ ＋ HNSWベクトル ＋ ドキュメントの統合**:
  - **エピソード記憶のベクトル検索**: HNSW（Hierarchical Navigable Small World）インデックスによる高速コサイン類似度検索。
  - **関係性のグラフ表現**: `companion->remembers->memory`、`memory->cited_in->turn`、`memory->relates_to->user` など、認知モデルの関係性をグラフエッジで自然に表現。
  - **ACID保証**: 記憶の追加・更新、引用回数のインクリメントを同一トランザクションで確定。

#### 実装例・一次情報
- **依存定義 (`Cargo.toml`)**:
  ```toml
  surrealdb = { version = "2.2", features = ["kv-surrealkv"] }
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

  // HNSW ベクトルインデックスの定義
  db.query(r#"
      DEFINE TABLE memory SCHEMALESS;
      DEFINE INDEX memory_vector ON TABLE memory FIELDS embedding HNSW DIMENSION 1536 DIST COSINE;
  "#).await?;
  ```
- **一次ドキュメント**:
  - [SurrealDB Embedded Rust Guide](https://surrealdb.com/docs/surrealdb/integration/sdks/rust)
  - [SurrealDB Vector Search Documentation](https://surrealdb.com/docs/surrealql/functions/database/vector)

---

### B. エージェントループ：ハイブリッド・ステートマシン

「完全自作による実装量の爆発」を避けつつ、「フレームワークの制約で分岐や全二重が書けない問題」を解消するハイブリッド設計。

```text
  ┌────────────────────────────────────────────────────────┐
  │ 【LLM通信・ツール定義レイヤー】（ライブラリ活用）         │
  │  ・rig-core / genai                                    │
  │  ・OpenAI, Anthropic, Gemini, Groq, OpenRouter, Ollama │
  │  ・ツール定義マクロ、JSON Schema生成、SSEストリームパース │
  └───────────────────────────┬────────────────────────────┘
                              │ 抽象プロバイダー / ツール呼び出し
                              ▼
  ┌────────────────────────────────────────────────────────┐
  │ 【エージェントループ・状態機械】（Tokio非同期ステートマシン） │
  │  ・会話木構造（DAG）とPre-compaction分岐管理           │
  │  ・厳密なアイドル検知（ユーザー操作不在計測・独り言除外） │
  │  ・Shizuku-Duplex全二重（ひとりごと判定、文節TTS送出）    │
  │  ・マルチモーダル先着優先承認（音声/画面ロック）         │
  └────────────────────────────────────────────────────────┘
```

#### 実装のポイント
- **実装量の抑制**: 各社LLMのHTTPヘッダー、ストリーミングパース、ツール呼び出し引数のJSONパース、リトライは `rig-core` または `genai` が担うため、ゼロからAPIクライアントを書く必要はない。
- **完全な制御権**: 会話ターンやCompactionの管理をBevy ECSのtickからTokio非同期ループへ移行。これにより、バックグラウンド要約中の割り込みや、0.5秒でのAdaptive Barge-inバッファ破棄を直感的な非同期タスクとして書くことができる。

#### 実装例・一次情報
- [`genai` GitHub (jeremychone/rust-genai)](https://github.com/jeremychone/rust-genai)（マルチプロバイダーの最小・統一LLMクライアント）
- [`rig-core` GitHub (0xPlaygrounds/rig)](https://github.com/0xPlaygrounds/rig)（型安全なTool deriveマクロとプロバイダー連携）

---

### C. OSサンドボックス：ネイティブ抽象化（方式1）

主要3OS（Linux / Windows / macOS）に対して、共通の抽象インターフェースを提供し、各OSの最適なセキュリティ機構を駆動する。

| OS | 適用する隔離機構 | 制御内容 |
| :--- | :--- | :--- |
| **Linux** | **`Landlock`**（カーネル5.13+）/ **`bubblewrap`** | ファイルシステムの指定パス外アクセス遮断、ネットワーク名前空間制限 |
| **Windows** | **`Job Objects`** ＋ **`Restricted Tokens`** | CPU・メモリ・プロセス生成制限、セキュリティトークンによるアクセス権剥奪 |
| **macOS** | **`Apple Sandbox (Seatbelt / SBPL)`** | macOSネイティブのサンドボックスプロファイルによるFS・ネットワーク制限 |
| **全OS共通** | **`cap-std`** (`=4.0.3`) | Capability-basedファイルシステム。親ディレクトリへのパストラバーサル（`../`）をOSレベルで遮断 |

#### 実装例・一次情報
- [`cap-std` Docs.rs](https://docs.rs/cap-std/latest/cap_std/)（オープンディレクトリをルートとする安全なファイル操作）
- [`process-wrap` Docs.rs](https://docs.rs/process-wrap/latest/process_wrap/)（Unix process-group、Windows Job Object、kill-on-dropのマルチプラットフォーム対応）
- [`sandbox_run` Docs.rs](https://docs.rs/sandbox_run)（Linux LandlockとmacOS SandboxのCommandラッパー）

---

### D. 音声パイプライン（STT / TTS）：プラガブル設計

インターフェース（Trait）を定義し、初期は代表的な最小構成（例: ローカルVOICEVOX + 高速API/ローカルWhisper）から開始して、後から自由に拡張できる設計とする。

```rust
#[async_trait::async_trait]
pub trait SttEngine: Send + Sync {
    /// 音声ストリームチャンクを受け取り、中間テキストまたは確定テキストを返す
    async fn process_chunk(&mut self, pcm_chunk: &[f32]) -> Result<Option<SttResult>, SttError>;
}

#[async_trait::async_trait]
pub trait TtsEngine: Send + Sync {
    /// 文節テキストを受け取り、オーディオストリームを順次送出する
    async fn synthesize_stream(&self, text: &str, voice_params: &VoiceParams) 
        -> Result<Pin<Box<dyn Stream<Item = AudioChunk> + Send>>, TtsError>;
}
```

#### サポート候補（段階的に追加）
- **STT候補**:
  - `sherpa-onnx`（C++/Rustネイティブ・ONNXストリーミング、Python不要、Windows/Mac/Linux全対応）
  - `whisper-rs`（whisper.cppバインディング）
  - クラウド高速API（Groq Whisper, Deepgram, OpenAI Whisper）
- **TTS候補**:
  - `VOICEVOX` / `Style-Bert-VITS2`（日本語キャラクター対話の標準HTTP API）
  - `Piper` / `sherpa-onnx` TTS（超軽量ローカルニューラル音声合成）
  - `ElevenLabs`（高品質WebSocketストリーミング音声）

---

### E. アバター描画：`apps/ene-avatar`（Bevy + `ene-vrm`）の活用

- **既存資産の最大活用**:
  - `apps/ene-avatar` は既に Bevy 0.20 + `ene-vrm` によるVRMロード、モーフターゲット（表情・口パク）、および透過ウィンドウ（Wayland/Windows）の基盤を持つ。
- **クロスプラットフォーム対応**:
  - Windows: Win32 APIによるレイヤード透過ウィンドウ（実装済み）。
  - Linux: Wayland layer-shell（実装済み）。
  - macOS: `bevy_winit` の `Window` 設定（`transparent: true`, `decorations: false`）による透過対応。
- **RMS音量解析によるゼロ遅延口パク**:
  - 音声再生のPCM振幅（RMS）をBevyのFrame Updateで直接計測し、VRMブレンドシェイプ（`aa` 等）を変位。
