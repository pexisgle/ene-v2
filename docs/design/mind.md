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
-- rowid を明示的 INTEGER PRIMARY KEY AUTOINCREMENT とすることで、VACUUM時もFTS5外部コンテンツとの対応関係が不変であることを保証
CREATE TABLE IF NOT EXISTS memories (
    rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    owner TEXT NOT NULL,              -- 不変の原所有者コンパニオンID
    audiences TEXT NOT NULL,          -- JSON array: 共有先コンパニオンID一覧
    content TEXT NOT NULL,            -- 記憶の要約・事実・手順本文
    importance INTEGER NOT NULL CHECK (importance >= 0 AND importance <= 10), -- 想起優先度（0〜10: 忘却時は0または1へ引き下げ）
    occurred_at TEXT,                 -- 出来事の発生日時（ISO 8601、不明時はnull）
    recorded_at TEXT NOT NULL,        -- 初回記録日時
    last_accessed_at TEXT,            -- 明示的引用による最終利用日時
    memory_revision INTEGER NOT NULL DEFAULT 1,
    emotion TEXT,                     -- 当時の感情タグ
    source_context TEXT NOT NULL,     -- JSON: 由来（turn_id, run_id, 話者等）
    embedding BLOB,                   -- ベクトルバイト列 (f32 LE: Rust内スライスコサイン類似度計算用)
    embedding_model TEXT,             -- 例: "text-embedding-3-small"
    embedding_dimensions INTEGER      -- 例: 1536
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
    relation TEXT NOT NULL, -- 'remembers', 'cited_in', 'supersedes'
    to_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (from_id, relation, to_id)
);

-- 検索インデックス
CREATE INDEX IF NOT EXISTS idx_memories_owner_importance ON memories(owner, importance);
```

### 3.2 所有権と共有（Privacy & Sharing）
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
- **マルチプロバイダー埋め込み（ローカル / クラウド選択性）**:
  - セットアップ時や設定画面で、ユーザーの環境やプライバシー要件に応じて埋め込みモデルを柔軟に選択可能とする（ローカル: `fastembed`, `bge-m3`, `all-MiniLM-L6-v2` 等、クラウド: OpenAI `text-embedding-3-small`, Gemini 等）。
  - 各記憶レコードは使用したモデル名と次元数を保持し、検索時は現在のモデルと一致するベクトルのみを照合する。
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

### 4.2 引用ガード（想起自己汚染ループの防止）

- **更新条件の厳格性**:
  - ベクトル検索により想起候補としてプロンプトへ注入されただけでは、`last_accessed_at` は**更新されない**。
  - **発話本文を汚染しない増分整理判定**:
    - 音声ストリーミング（TTS）を阻害しないよう、発話本文に `<cite id="..."/>` 等のタグを出力させることは行わない。
    - ターン完了後のバックグラウンド増分整理（Consolidation）において、整理モデルがターンの会話内容を分析し、実際に参照・活用された記憶ID（`cited_memory_ids`）を構造化出力として確定する。
    - または親が `mind_read` / `mind_update` ツールを明示的に呼び出した場合のみ、ホストが `last_accessed_at = now()` へ更新し、エッジテーブルに `(memory_id, 'cited_in', turn_id)` を追加する。
  - これにより、無関係な記憶が一度検索に引っかかっただけで直近性スコアを引き上げ続け、次回以降も過剰に想起され続ける「想起の自己汚染ループ」を遮断する。

### 4.3 常時注入記憶（Pinned / Core Memory）と動的コンテキストの非永続注入

`importance == 10` のレコード、および `user_profile` は、ベクトル類似度検索をバイパスして注入される。

> [!IMPORTANT]
> **動的文脈の非永続注入原則（SessionLog の汚染防止）**:
> Pinned Memory、想起された記憶、現在の心、知覚要約、現在日時は、**会話ログ正本（`SessionLog`）へ `DeliveryMode::Inform` 等で書き込んではならない**。
> `SessionLog` に一度書き込まれた内容は永続化され、履歴取得API（`GetHistory`）への内部JSON漏洩や、増分自動整理（Consolidation）が想起記憶を自ら再学習する自己汚染ループを引き起こす。
> 動的コンテキストはすべて、モデル呼び出し直前の `PrepareSystems::Fit` において、`rig_memory::recent::view` で窓を切り詰めた直後の **インメモリ `ModelRequest.history` の末尾（最新ユーザー入力の直前、Dynamic Tail）へ一時的に挿入** する。

ただし、以下の不変条件を厳格に適用する：

1. **優先度低下・訂正の適用**: 訂正されて `superseded_by` を持つ古い記憶や、忘却（想起優先度引き下げ）された記憶は、重要度10を満たさないためPinned枠へ注入されない。
2. **トークン予算上限（Token Budget）**:
   - Pinned Memory枠には最大トークン予算（例: 最大 1,000 トークン）を設定する。
   - 自動整理モデルが誤って重要度10を大量生成した場合でも、Pinned枠だけでプロンプト全体が圧迫される事故を防止する。予算超過時は最新の `last_accessed_at`（未引用時は `recorded_at`）順で収容する。
   - 自動整理モデルが誤って重要度10を大量生成した場合でも、Pinned枠だけでプロンプト全体が圧迫される事故を防止する。予算超過時は最新の `last_accessed_at`（未引用時は `recorded_at`）順で収容する。

---

## 5. 即時更新と自動整理の整合性

### 5.1 親（頭）の即時更新ツール
親エージェントには以下のツールが提供される（主体・権限・操作IDはホストが自動束縛）：
- `mind_recall(query: String, limit: Option<u32>) -> Vec<MemoryItem>`: 記憶の検索
- `mind_read(memory_id: String) -> MemoryDetail`: 記憶の詳細および由来の閲覧
- `mind_update(action: MindUpdateAction) -> MindUpdateResult`: 明示的な記憶の追加・訂正・想起優先度引き下げ

### 5.2 自動整理（Consolidation）における内容の分割保存
対話ログからの長期記憶形成は、**Compaction（要約）の発生を必須条件とせず、独立してバックグラウンド実行**される。
- **起動契機**: ユーザー対話が途切れたアイドル時（60秒以上）、または未整理の確定ターン数が閾値（例: 5ターン）に達した時点。
- **内容ごとの独立レコード分割保存**:
  - 1つの会話発言から複数の異なる事実（例: 「好きな飲み物はコーヒー」と「勤務先はTech社」）が読み取れる場合、**単一のレコードにまとめず、独立した個別の `MemoryRecord` として保存する**。
  - 各レコードは固有のIDを持ち、出所（`source_context.turn_id`）を共有しながらも、重要度（想起優先度）を完全に独立して管理できる。
- **未処理カーソル（`processed_turns_cursor`）による増分抽出**:
  - 自動整理ジョブは、最後に抽出を完了した `processed_turns_cursor` より後の新規ターンのみを増分走査する。
  - 過去に処理済みのターンを何度も再走査しないため、以前に抽出・優先度変更された事実が、過去ログから新しいレコードとして重複再生成されることは構造的に防止される。

```text
未処理の対話ログ・子の作業報告
  → processed_turns_cursor より後の未読ターンを取得（処理済みターンは再走査をスキップ）
  → 既存記憶と現在の版（memory_revision）・想起優先度（importance）を参照
  → 経験の抽出（異なる内容は個別の独立したMemoryRecordとして分割）
  → 既存記憶の統合・訂正、引用実績に基づく重要度再評価
  → トランザクション内で競合検証（版不一致・古い優先度への不当な巻き戻しを拒否）
  → 記憶・processed_turns_cursor を原子的コミット
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
| **遅延整理の競合排除** | 記憶変更後に、変更前の古い文脈から生成した整理結果をコミット試行 | 版不一致（Conflict）として安全に拒否され、最新状態が上書きされないこと |
| **引用ガードの厳密性** | 検索候補としてプロンプトに注入された記憶と、応答内で明示引用された記憶を比較 | 検索されただけでは `last_accessed_at` が更新されず、応答メタデータや明示操作で引用された場合のみ更新されること |
| **Pinned予算制限** | 重要度10の記憶を多数作成し、プロンプト生成を実行 | 予算上限（例: 1,000トークン）を超過せず、文脈窓を圧迫しないこと |
| **埋め込み空間の隔離** | 異なる埋め込みモデル名・次元数のベクトルを登録して検索 | 次元不一致・モデル違いのベクトルが混入せず、一致する空間のみで類似度計算が行われること |
