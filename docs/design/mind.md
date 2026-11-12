# 心：人格・経験・自動記憶・感情の統合設計

状態：採用設計。[全体アーキテクチャ](../architecture.md)、[技術スタック](tech-stack.md)、[作業相手API](work.md)、[記憶機能仕様](../features/memory/README.md)を参照する。

---

## 1. 目的と設計原則

「心」は、単にプロンプトへ過去ログや感情値を注入する機能ではない。
デスクトップ常駐コンパニオンが、作者が定めた「核となる人格」を守りつつ、日々のユーザーとの対話・作業経験を受け止め、持続的な思い出・感情・関係性として保持し、次の判断に活かすための統合認知アーキテクチャである。

### 設計原則
1. **作者設定（Profile）と実体験（Memory）の完全分離**:
   - キャラクターの根幹（口調、世界観、価値観）は作者設定であり、日々の会話でLLMが直接書き換えることはできない。
   - 実体験（エピソード記憶）はユーザーとの交流で蓄積・成長するが、架空の会話例やロールプレイ設定を実体験として混入させない。
2. **SQLite（Embedded `rusqlite` WAL ＋ FTS5 ＋ ベクトル検索）を唯一の正本とする**:
   - 記憶の正本、ベクトル検索、全文検索、および認知的関係性エッジを、追加ランタイム不要のPure Rust/C組み込みストレージ（`rusqlite` WALモード）で単一同期トランザクション管理する。
3. **境界不変条件の厳密な保証**:
   - 忘却（想起優先度の引き下げ）された記憶はPinned枠から外れて想起スコアが低下し、遅れて完了した自動整理ジョブが古い優先度で上書きする競合を防止する。
   - ベクトル検索空間はモデル名・次元数・前処理版で厳密に識別し、異種ベクトルの混線・汚染を防止する。

---

## 2. 人格と情報の分類と「心」のライフサイクル

| 情報の種類 | 性質 | 心での扱い |
| :--- | :--- | :--- |
| **作者設定（CharacterProfile）** | 静的設定 | ロールプレイの核。会話で直接書き換えない不変の境界。配布・エクスポートの対象 |
| **現在の心（current_mind）** | 動的状態 | 気分・感情タグ・最近の関心事。ターンごとに更新され、長時間のアイドルで基本気質へ自然減衰 |
| **説明用の会話例** | 参考材料 | 話し方・判断傾向の材料。実体験の記憶テーブルへは混入させない |
| **ユーザーの発言・出来事** | 観測事実 | 誰が何を言ったかの記録。内容自体の客観的真偽とは区別して保存 |
| **作業・操作結果** | 実績事実 | ホストや対象ツールが返した実行結果。成否不明・失敗も正確に保持 |
| **解釈・感情の受け止め** | 主観 | 根拠と結び付いた主観。客観的観測事実やシステム権限へ昇格させない |
| **要約・想起・再整理** | 派生物 | 既存経験の派生物。新しい出来事が再発生したことにはしない |

---

## 3. SQLite 記憶データモデル・スキーマ

`apps/ene-host` で運用される完全な同期SQLiteスキーマ定義（`host.sqlite3`）。

### 3.1 テーブル定義

```sql
PRAGMA journal_mode = WAL;

-- 1. コンパニオン（人格の核と現在の心）
CREATE TABLE IF NOT EXISTS companions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    persona_profile TEXT NOT NULL, -- JSON: 作者設定（核・口調・価値観・会話例）
    current_mind TEXT NOT NULL,    -- JSON: 現在の心（感情・関係理解・関心事）
    mind_revision INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);

-- 2. ユーザー基本プロファイル
CREATE TABLE IF NOT EXISTS user_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    preferences TEXT NOT NULL,         -- JSON array
    static_constraints TEXT NOT NULL,  -- JSON array
    updated_at TEXT NOT NULL
);

-- 3. エピソード記憶（MemoryRecord）
-- rowid を明示的 INTEGER PRIMARY KEY とすることで、SQLite の 64-bit rowid の直接エイリアスとなり VACUUM 時も安定して FTS5 外部コンテンツテーブルとの整数参照関係を保持
CREATE TABLE IF NOT EXISTS memories (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    owner TEXT NOT NULL,              -- 不変の原所有者コンパニオンID
    audiences TEXT NOT NULL,          -- JSON array: 共有先コンパニオンID一覧
    content TEXT NOT NULL,            -- 記憶の要約・事実・手順本文
    importance INTEGER NOT NULL CHECK (importance >= 0 AND importance <= 10), -- 想起優先度（0〜10: 忘却時は0または1へ引き下げ）
    occurred_at TEXT,                 -- 出来事の発生日時（ISO 8601、不明時はnull）
    recorded_at TEXT NOT NULL,        -- 初回記録日時
    last_accessed_at TEXT,            -- 明示的引用による最終利用日時
    memory_revision INTEGER NOT NULL DEFAULT 1,
    content_revision INTEGER NOT NULL DEFAULT 1, -- 本文更新時にインクリメントされ、埋め込みの鮮度検証に使用
    emotion TEXT,                     -- 当時の感情タグ
    source_context TEXT NOT NULL,     -- JSON: 由来（turn_id, run_id, 話者等）
    embedding BLOB,                   -- ベクトルバイト列 (f32 LE: Rust内スライスコサイン類似度計算用)
    embedding_model TEXT,             -- 埋め込み空間複合識別子: "<model_name>:<dimensions>:<prep_version>"（例: "text-embedding-3-small:1536:v1"）
    embedding_dimensions INTEGER,     -- 例: 1536
    embedding_content_revision INTEGER -- 生成時の content_revision (不一致時は無効・再計算対象)
);

-- 4. 日本語 trigram 全文検索外部コンテンツ仮想テーブル
-- 明示的 INTEGER PRIMARY KEY である rowid を使って memories と同期する外部コンテンツ方式
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

-- 5. 認知的関係性グラフ（1ホップ エッジテーブル）
CREATE TABLE IF NOT EXISTS memory_edges (
    from_id TEXT NOT NULL,
    relation TEXT NOT NULL, -- 'remembers', 'cited_in', 'estimated_cited_in', 'supersedes'
    to_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (from_id, relation, to_id)
);

-- 6. 増分自動整理の進行状態永続化テーブル
CREATE TABLE IF NOT EXISTS consolidation_state (
    companion_id TEXT PRIMARY KEY,
    last_processed_turn_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- 検索インデックス
CREATE INDEX IF NOT EXISTS idx_memories_owner_importance ON memories(owner, importance);
```

### 3.2 データベーススレッドモデルと実行境界
- **ECSループスレッドでの直接同期アクセス（決定論的・ゼロオーバーヘッド）**:
  - Bevy ECS のシステム（`PrepareSystems::Edit`, `Stage::Gate` 等）は同期関数として動作する。
  - デスクトップ利用におけるプライマリキー参照、ルール照合、FTS5 trigram検索、およびインメモリBLOBコサイン類似度計算（通常1,000件未満）は **1ms未満（数百マイクロ秒）** で完了する。
  - したがって、ECSメインスレッドに同期の `rusqlite::Connection` を直接保持して同期実行し、非同期タスクスイッチやチャネル往復オーバーヘッドを完全に排除する。
  - 会話セッションログ正本は `rig::sqlite::SqliteSessionStore`（`rig-sqlite`）により単一の `host.sqlite3` に一元化される。
  - 記憶整理（Consolidation）のLLM呼び出しや埋め込みベクトル生成（Embedding API）などの長時間非同期処理のみ、専用のTokioバックグラウンドタスクおよび別コネクションへ分離する。

### 3.3 所有権と共有（Privacy & Sharing）
- **原所有者（`owner`）の不変性**: 記憶の `owner` は作成したコンパニオンのIDを常に保持する。他コンパニオンへ共有する場合も `owner = "shared"` で上書きせず、`audiences` JSON配列に対象コンパニオンIDを追加する。
- **共有解除**: `audiences` から対象IDを削除することで、共有先コンパニオンの検索クエリから即座に不可視化される（原所有者の記憶としては残る）。
- **私的根拠の非漏洩**: 共有先が参照できるのは `content`, `importance`, `occurred_at` のみであり、プライベートな会話ログ全文や個人的感情メモは共有先へ露出させない。

---

## 4. 想起スコアリングと引用ガード

### 4.1 想起スコアリング計算式

Generative Agents と Mem0 の知見を統合し、想起候補の最終スコアは関連度・重要度・直近性の加重和に、重要度による減衰係数を乗じて算出する。

$$\text{Score} = \min\left(1.0, \frac{\text{Importance}}{0.3}\right) \times (0.5 \cdot \text{Relevance} + 0.3 \cdot \text{Importance} + 0.2 \cdot \text{Recency})$$

- **減衰係数（直後の再想起防止）**:
  - $\text{Importance} = \text{importance} / 10.0$（$0.0 \sim 1.0$）。
  - ユーザーが「今の話は忘れて！」と指示した直後は、出来事が起きた直後であるため $\text{Recency} \approx 1.0$、直後の話題が類似していれば $\text{Relevance} \approx 1.0$ となり、単純加算ではスコアが不当に高くなってしまう。
  - $\min(1.0, \text{Importance} / 0.3)$ を乗じることで、$\text{importance} = 0$ のときはスコアが $0.0$ となり、$\text{importance} = 1$ のときもスコア全体が $1/3$ に減衰し、忘却直後のスコアインフレを数学的に防止する（$\text{importance} \ge 3$ では係数は $1.0$ で減衰なし）。
- **自動想起と明示想起の境界**:
  - **自動想起（毎ターンのプロンプト注入枠）**: `WHERE m.importance >= 2` を対象とする。忘却指示（`importance = 0` または `1`）を受けた記憶は、自動で取り出す記憶の候補から即座に外れる。
  - **明示想起（`mind_recall` ツール枠）**: ユーザーが「さっき忘れてって言ったあの話だけど…」等と明示的に過去を尋ねた場合は、`importance >= 0` の全件が対象となり、過去の客観的事実として再確認できる。

- **Relevance（関連度: $0.0 \sim 1.0$）**:
  - FTS5 trigram 語句検索とベクトル意味検索を独立して取得し、Rust側で **RRF（Reciprocal Rank Fusion）** により順位融合したスコア。
  - 語句一致がなくても意味的に近い候補、および類似度は低くても固有名詞が完全一致する候補の双方を漏らさず取得する。

```rust
// Rust側でのハイブリッド検索とRRF統合のフロー
// 1. 全文検索候補の取得（FTS5 BM25スコア順・最大20件）
// ※FTS5 trigram は文字列全体をクォートすると完全一致部分文字列しかマッチしないため、
// 3文字以上の部分トークン（空白・記号・助詞等で分割）をクォートし OR で連結する。
// 3文字以上のトークンが存在しない場合（例:「犬」「AI」）は LIKE へフォールバック。
let tokens: Vec<String> = split_search_tokens(&query_text)
    .into_iter()
    .filter(|t| t.chars().count() >= 3)
    .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
    .collect();

let lexical_matches = if !tokens.is_empty() {
    let fts_expr = tokens.join(" OR ");
    let mut stmt = conn.prepare(r#"
        SELECT m.id, m.content, m.importance, m.occurred_at, m.last_accessed_at, bm25(memories_fts) AS rank
        FROM memories m
        JOIN memories_fts f ON m.rowid = f.rowid
        WHERE (m.owner = ?1 OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = ?2))
          AND (?4 = false OR m.importance >= 2) -- 自動想起時は >= 2、明示的想起は全件
          AND memories_fts MATCH ?3
        ORDER BY rank ASC
        LIMIT 20
    "#)?;
    stmt.query_map(params![companion_id, companion_id, fts_expr, is_auto_recall], |row| ...)?
} else {
    // 2文字以下のフォールバック検索（LIKE）
    let like_pattern = format!("%{}%", query_text.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(r#"
        SELECT m.id, m.content, m.importance, m.occurred_at, m.last_accessed_at, 0.0 AS rank
        FROM memories m
        WHERE (m.owner = ?1 OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = ?2))
          AND (?4 = false OR m.importance >= 2)
          AND m.content LIKE ?3 ESCAPE '\'
        ORDER BY m.importance DESC
        LIMIT 20
    "#)?;
    stmt.query_map(params![companion_id, companion_id, like_pattern, is_auto_recall], |row| ...)?
};

// 2. ベクトル意味検索候補の取得（Rust内スライスコサイン類似度順・最大20件）
// ※選択された埋め込みモデル・次元数が一致するBLOBレコードのみを走査し、Rust内でコサイン類似度を算出
let semantic_matches = query_vector_similarity(&conn, companion_id, &query_vec, is_auto_recall, 20)?;

// 3. RRF (Reciprocal Rank Fusion) による順位融合
// ※SQLite の bm25() は負の浮動小数点数を返すため、ソート後の1始まり整数順位（rank_i = index + 1）を使用する
// rrf_score = sum(1.0 / (60.0 + rank_i))
let fused_candidates = reciprocal_rank_fusion(&lexical_matches, &semantic_matches, 60.0);

// 4. 正規化 Relevance (0.0 〜 1.0) の算出
// 2リスト・k=60 の理論上限値 (2.0 / 61.0) で除算して 0.0〜1.0 にスケール合わせ
for candidate in &mut fused_candidates {
    candidate.relevance = (candidate.rrf_score / (2.0 / 61.0)).clamp(0.0, 1.0);
}
```
- **マルチプロバイダー埋め込み（ローカル / クラウド選択性と空間分離）**:
  - セットアップ時や設定画面で、ユーザーの環境やプライバシー要件に応じて埋め込みモデルを柔軟に選択可能とする（ローカル: `fastembed`, `bge-m3`, `all-MiniLM-L6-v2` 等、クラウド: OpenAI `text-embedding-3-small`, Gemini 等）。
  - 各記憶レコードは、モデル名・次元数・前処理バージョンを複合化した `embedding_model` 識別子（例: `"text-embedding-3-small:1536:v1"`）を保持し、検索時は現在の空間識別子と完全一致するベクトルのみを走査・類似度計算の対象とする（次元不一致や前処理変更による破綻を決定論的に防止）。
- **Importance（重要度: $0.0 \sim 1.0$）**:
  - $0 \sim 10$ の順序尺度。$\text{Importance} = \text{importance} / 10.0$ として正規化（忘却時は $0$ または $1$ へ引き下げる）。
    - $0$: 忘却指示を受けた記憶（明示的検索でのみ参照可能）
    - $1 \sim 3$: 日常の雑談、一時的な状況
    - $4 \sim 6$: 趣味嗜好、仕事上の習慣
    - $7 \sim 9$: 約束、大きな決定、重要な出来事
    - $10$: コンパニオンとして常時忘れてはならない中核事実（Pinned Memory）
- **Recency（直近性 / 時間減衰: $0.0 \sim 1.0$）**:
  $$\text{Recency} = e^{-\lambda \Delta t}$$
  - $\lambda \approx 0.05$（半減期約14日）。
  - $\Delta t$: 最終アクセス日時または発生日時（`COALESCE(last_accessed_at, occurred_at, recorded_at)`）からの経過日数。引用ガードにより実際に会話で参照された記憶は `last_accessed_at` が更新されて直近性がリセットされ、未参照の記憶は時間とともに自然減衰する。

### 4.2 引用ガード（想起自己汚染の低減）

- **更新条件の厳格性とホスト側引用検証**:
  - ベクトル検索により想起候補としてプロンプトへ注入されただけでは、`last_accessed_at` は**更新されない**。
  - **発話本文を汚染しない増分整理判定**:
    - 音声ストリーミング（TTS）を阻害しないよう、発話本文に `<cite id="..."/>` 等のタグを出力させることは行わない。
    - ターン完了後のバックグラウンド増分整理（Consolidation）において、整理モデルがターンの会話内容を分析し、実際に参照・活用された記憶ID（`cited_memory_ids`）を構造化出力として抽出する。
    - **ホストによる引用IDの照合検証と利用実績の区分**:
      - ホストは抽出された `cited_memory_ids` が**「そのターンで実際にプロンプトへ注入された記憶IDリスト」に含まれているかを機械的に検証**する。注入されていない記憶IDや幻覚されたIDの引用は棄却し、不当なアクセス実績の更新を遮断する。
      - 親エージェントによる明示的な `mind_read` / `mind_update` ツール呼び出し（「直接・確実な利用実績」）は関係エッジ `(memory_id, 'cited_in', turn_id)` として記録する。
      - 一方、整理モデルによる抽出判定は「推定利用実績」として位置づけ、ホストの注入照合検証を通過したもののみ関係エッジ `(memory_id, 'estimated_cited_in', turn_id)` として区別して記録する。
      - いずれの確実な利用実績または検証済み推定利用実績が成立した場合に限り、ホストが `last_accessed_at = now()` へ更新する。
  - これにより、無関係な記憶が一度検索に引っかかっただけで直近性スコアを引き上げ続け、次回以降も過剰に想起され続ける誤った再強化を低減する。

### 4.3 常時注入記憶（Pinned / Core Memory）と動的コンテキストの非永続注入

`importance == 10` のレコード、および `user_profile` は、ベクトル類似度検索をバイパスして注入される。

> [!IMPORTANT]
> **動的文脈の非永続注入原則（SessionLog の汚染防止）**:
> Pinned Memory、想起された記憶、現在の心、知覚要約、現在日時は、**会話ログ正本（`SessionLog`）へ `DeliveryMode::Inform` 等で書き込んではならない**。
> `SessionLog` に一度書き込まれた内容は永続化され、履歴取得API（`GetHistory`）への内部JSON漏洩や、増分自動整理（Consolidation）が想起記憶を自ら再学習する自己汚染ループを引き起こす。
> 動的コンテキストはすべて `PrepareSystems::Edit` で `ModelRequest.history` 末尾の最新ユーザー発話メッセージ（`User_N`）内にブロックとしてインメモリ結合し、その上で **`PrepareSystems::Fit` を動的文脈を含む要求全体に対して実行**し、合計トークン数に基づいて履歴ウィンドウ（`recent::view`）を切り詰める。

ただし、以下の不変条件を厳格に適用する：

1. **優先度低下・訂正の適用**: 訂正されて `superseded_by` を持つ古い記憶や、忘却（想起優先度引き下げ）された記憶は、重要度10を満たさないためPinned枠へ注入されない。
2. **トークン予算上限（Token Budget）**:
   - Pinned Memory枠には最大トークン予算（例: 最大 1,000 トークン）を設定する。
   - 自動整理モデルが誤って重要度10を大量生成した場合でも、Pinned枠だけでプロンプト全体が圧迫される事故を防止する。予算超過時は最新の `last_accessed_at`（未引用時は `recorded_at`）順で収容する。

---

## 5. 即時更新と自動整理の整合性

### 5.1 親（頭）の即時更新ツール
親エージェントには以下のツールが提供される（主体・権限・操作IDはホストが自動束縛）：
- `mind_recall(query: String, limit: Option<u32>) -> Vec<MemoryItem>`: 記憶の検索
- `mind_read(memory_id: String) -> MemoryDetail`: 記憶の詳細および由来の閲覧
- `mind_update(action: MindUpdateAction) -> MindUpdateResult`: 明示的な記憶の追加・訂正・想起優先度引き下げ

### 5.2 自動整理（Consolidation）の単一方向パイプラインと重複防止
対話ログからの長期記憶形成は、**Compaction（要約）の発生を必須条件とせず、独立してバックグラウンド実行**される。
- **起動契機**: ユーザー対話が途切れたアイドル時（60秒以上）、または未整理の確定ターン数が閾値（例: 5ターン）に達した時点。
- **単一フライト実行ガード（Single-Flight Consolidation Guard）**:
  - コンパニオンごとに同時に稼働する自動整理ジョブは**常に高々1件**に制限する（ホスト側インメモリの `is_consolidating` フラグまたは排他ガード）。
  - 先行ジョブの処理中に新たなアイドルやターン蓄積が発生しても、多重に並行ジョブを走らせず、スキップまたは次回アイドル時へ安全に遅延させる。
- **物理的再実行の確実な重複排除と意味的統合の境界**:
  - **同一会話ターン範囲の二重コミットの排除（No Duplicate Commit of the Same Turn Range）**: 確定ログのターンIDおよび `consolidation_state.last_processed_turn_id` のカーソル管理により、同一会話ターン範囲を二重にコミットする事故を決定論的に防止する。
  - **意味的な統合提案（Heuristic Consolidation）**: 会話から抽出された知識候補が既存記憶と同一・類似であるかの判定は、FTS5およびベクトル類似検索を参考にした整理モデルの意味的推論（統合提案）として扱う。
    - 既存記憶の更新と判断された場合：`memory_revision` と `content_revision` をインクリメントして内容・重要度を更新し、古い埋め込みを無効化（再計算対象にマーキング）。
    - 忘却済み記憶（`importance <= 1`）の復活防止：出所ターンや既存エッジ（`supersedes` 等）を照合し、同一主題の過去記憶が忘却されている場合は、安易に高優先度の新規レコードとして再生成しないよう整理プロンプトの制約を課す。
- **単一方向パイプライン（Single-Direction Consolidation Pipeline）**:
  1. **確定済みログの読み込みと開始カーソルの束縛**:
     - ジョブ開始時に `consolidation_state` から現在の `job_start_turn_id` を読み込み束縛する。
     - `SessionLog`（追記型ディスク正本）から、`job_start_turn_id` より後の確定ターン（最新の未完了ターンを除く安定範囲）を読み込む。Compaction後のプロンプト用Viewではなく、客観的な会話生ログを正本入力とする。
  2. **事実抽出と既存記憶の照合**: 整理モデルが会話から抽出した知識候補について、ホストがFTS5およびベクトル検索を用いて既存記憶との類似候補を取得・照合する。
  3. **更新（Merge）と新規作成（Insert）の適用**:
     - 既存の記憶と同一事実または更新である場合：`memory_revision` と `content_revision` をインクリメントして内容・重要度を更新。
     - 新規の独立した事実である場合：新しい `MemoryRecord` を採番して作成。1つの発話から複数事実が抽出された場合は、内容ごとに独立したレコードとして分割保存する。
  4. **原子的コミットとカーソル整合性の検証（Atomic Commit with Cursor Match）**:
     - 記憶の変更・エッジ（`estimated_cited_in` 等）の追加・およびカーソル更新を、同一のSQLiteトランザクション内で実行する。
     - トランザクション内で以下の更新条件を検証する：
       ```sql
       UPDATE consolidation_state
       SET last_processed_turn_id = :new_cursor, updated_at = :now
       WHERE companion_id = :companion_id AND last_processed_turn_id = :job_start_turn_id;
       ```
     - もし更新行数が 0 行（コミット直前に他処理によるカーソル前進や不整合が生じた場合）であった場合は、**トランザクション全体を直ちにロールバック（アボート）**し、重複挿入・誤コミットを完全に防止する。

```text
SessionLog（追記型確定生ログ）
  │
  ├─ job_start_turn_id より後の安定ターンを読み出し (Single-Flight)
  ▼
整理モデルによる経験・事実の抽出
  │
  ├─ 既存記憶のFTS5/ベクトル照合（重複検出）
  ▼
分岐：既存更新（UPDATE + revision進捗） or 新規分割作成（INSERT）
  │
  ├─ 引用実績のホスト側検証（プロンプト注入IDとの照合）
  ▼
SQLite トランザクション (Atomic Commit)
  ├─ memories / memory_edges のコミット
  └─ consolidation_state 更新 (WHERE last_processed_turn_id = job_start_turn_id)
     └─ 一致時: コミット完了 / 不一致時: ロールバック破棄
```

### 5.3 「忘却」のセマンティクス（想起優先度の引き下げ）と境界
- **人間の自然な記憶に即した忘却設計（明確な削除フラグの排除）**:
  - **記憶に明確な削除（物理削除や除外フラグ）は存在しない**。
  - 忘却とは、記憶レコードの**想起優先度（`importance`）を引き下げ、日常会話で自然に想起されにくくすること**である。人間が過去の経験そのものを脳内から完全に抹消できないのと同様に、DBからの物理削除や除外フラグは設けない。
  - **想起スコア計算との連動と自動想起除外**:
    - 想起スコア計算式（$\text{Score} = \min(1.0, \text{Importance}/0.3) \times (0.5 \cdot \text{Relevance} + 0.3 \cdot \text{Importance} + 0.2 \cdot \text{Recency})$）において、優先度（`Importance`）が引き下げられることで総スコアが低下し、減衰係数により直後（高Recency）の再想起スコアインフレも防止される。
    - **自動想起（毎ターン注入枠）からの即時除外**: 自動想起クエリは `importance >= 2` を条件とするため、忘却指示により重要度が 0 または 1 に引き下げられた記憶は、**「自動で取り出す記憶の候補」から即座に外れる**。
    - 他の条件（Relevance, Recency）が同じであれば想起スコアは低減し、重要度10のPinned枠からも外れる。ただし、ユーザーが過去について明示的に尋ねた場合（`mind_recall` ツール実行時）は `importance >= 0` 全件が検索対象となり、人間が過去の記憶を詳細に問われて思い出すのと同様に再び確認できる。
  - **保証の明確な境界**:
    - **保証すること**: 忘却は想起優先度を引き下げる操作であり、自動想起候補から外れ、他の条件が同じなら想起スコアは低下し、重要度10のPinned枠から外れる。遅延した整理ジョブが古い優先度で上書きすることは防止される。
    - **保証しないこと（意図的仕様）**: ユーザーからの明示的な過去検索（`mind_recall`）における完全な不可視化や、直前の会話生ログ（Recent Dialogue History）・既存の要約文（Compaction Summary）からの物理的・遡及的な削除は保証しない。対話の客観的な事実記録としては残り、対話の進展に伴って直前ログが窓から自然に押し出されるか、次回のCompaction要約で抽象化されるのを待つ。
  - **分割保存との連携**:
    - 内容ごとに独立した `MemoryRecord` として分割保存されているため、一方の記憶（例: 勤務先）の優先度を引き下げても、同一ターンに由来する別の記憶（例: 好きな飲み物）の想起には一切影響を与えない。
  - **遅延整理ジョブの競合解決**:
    - ユーザーが「今の話は忘れて」と指示して優先度が引き下げられた後、バックグラウンドで走っていた古い整理ジョブが元の優先度で上書きしようとした場合：
      - トランザクション内で `memory_revision` を照合し、**古い整理結果を拒否（Conflict）**して破棄する。
      - 同一の出所ターンにおいて既に優先度が引き下げられているレコードが存在する場合、そのレコードの優先度を不当に巻き戻さない。

### 5.4 記憶の検査と対話的更新（`ListMemories` と `mind_update`）
- **開発者・デバッグ用インスペクション（`HostService.ListMemories`）**:
  - ホストは開発者や設定UIでの確認用に、保存された記憶一覧をページネーション付きで取得するRPC（`ListMemories`）を提供する。
  - これは状態の監査・デバッグ用であり、一般ユーザーが手動でテーブルを直接編集して記憶を育てることを前提としない。
- **日常の対話的更新（`mind_update` ツール）**:
  - 日常会話の中でのユーザーの好みや事実の訂正、忘却指示は、親エージェント（頭）がReActループ内で専用の `mind_update` ツールを自律的に呼び出すことで行われる。
  - 自然言語対話を通じて「あ、私コーヒーじゃなくて紅茶派だった」「了解、紅茶が好きなんだね！」というやり取りとともに、該当レコードの `importance` や内容がSQLite内で安全に更新される。

---

## 6. スキル記憶の管理と「手」への分離（HermesAgent方式）

実務タスクの手順やノウハウは、エピソード記憶とは分離してファイルベースのスキル形式で管理する。

- **ファイル構造**: `skills/<name>/SKILL.md` の形式でMarkdown管理。
- **段階的開示（Progressive Disclosure）**:
  - 親（頭）のプロンプトにはスキル一覧を**一切注入しない**（認知負荷・トークンの削減）。
  - 作業エージェント「手」（Hands / WorkSession）の開始時にコンパクトなインデックス（名前と1行概要）のみを渡し、タスク実行時に必要なスキルの詳細手順を専用ツールでオンデマンド取得させる。
- **自律学習（Closed Learning Loop）**:
  - 「手」が複雑な作業を完遂した際、またはユーザーから対話で特定の手順指導を受けた際に、LLMが手順・利用ツール・注意点を構造化したスキルとして書き出し・更新する。

---

## 7. 検証条件と受け入れ基準 (Alpha Mind Invariants)

| 不変条件 | 検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **忘却のセマンティクス** | 記憶の想起優先度（`importance`）を引き下げた後、想起スコアおよびPinned枠を検証 | 他の条件が同じなら想起スコアが低下し、重要度10のPinned枠から外れること。再整理ジョブによって古い優先度へ巻き戻らないこと（過去ログの客観記録は保持されること） |
| **遅延整理・並行コミットの競合排除** | 記憶変更後またはカーソル前進後に、変更前の古い文脈から生成した整理結果をコミット試行 | `last_processed_turn_id` 不一致または版不一致（Conflict）としてトランザクションがロールバックされ、同一ターン範囲の二重コミットや古い状態の上書きが完全に排除されること |
| **引用ガードの厳密性** | 検索候補としてプロンプトに注入された記憶と、応答内で明示操作または推定引用された記憶を比較 | 検索されただけでは `last_accessed_at` が更新されず、明示操作（`cited_in`）またはホスト照合を通過した推定引用（`estimated_cited_in`）の場合のみ更新されること |
| **Pinned予算制限** | 重要度10の記憶を多数作成し、プロンプト生成を実行 | 予算上限（例: 1,000トークン）を超過せず、文脈窓を圧迫しないこと |
| **埋め込み空間の隔離** | 異なる埋め込みモデル名・次元数・前処理バージョンの複合キー（`embedding_model`）を持つベクトルを登録して検索 | 次元不一致や前処理違いのベクトルが混入せず、一致する空間複合キーのみで類似度計算が行われること |
