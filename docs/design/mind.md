# 心：人格・経験・自動記憶・感情の統合設計

状態：Alphaの採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[技術スタック](tech-stack.md)、[作業相手API](work.md)、[Alpha実装計画](../alpha.md)、[記憶機能仕様](../features/memory/README.md)を参照する。

---

## 1. 目的と設計原則

「心」は、単にプロンプトへ過去ログや感情値を注入する機能ではない。
デスクトップ常駐コンパニオンが、作者が定めた「核となる人格」を守りつつ、日々のユーザーとの対話・作業経験を受け止め、持続的な思い出・感情・関係性として保持し、次の判断に活かすための統合認知アーキテクチャである。

### 設計原則
1. **作者設定（Profile）と実体験（Memory）の完全分離**:
   - キャラクターの根幹（口調、世界観、価値観）は作者設定であり、日々の会話でLLMが直接書き換えることはできない。
   - 実体験（エピソード記憶）はユーザーとの交流で蓄積・成長するが、架空の会話例やロールプレイ設定を実体験として混入させない。
2. **SurrealDB（SurrealKV組み込み）を唯一の正本とする**:
   - 記憶の正本、ベクトルインデックス、全文検索、および認知的関係性グラフを、C++依存のないPure Rust組み込みストレージ（SurrealKV）で単一トランザクション管理する。
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

## 3. SurrealDB 記憶データモデル・スキーマ

SurrealDB（SurrealKV）上に構築するスキーマ定義（SurrealQL DDL）。

### 3.1 テーブル定義

```surrealql
-- 1. コンパニオン（人格の核と現在の心）
DEFINE TABLE companion SCHEMAFULL;
DEFINE FIELD name ON TABLE companion TYPE string;
DEFINE FIELD persona_profile ON TABLE companion FLEXIBLE TYPE object; -- 作者設定（核・口調・価値観）
DEFINE FIELD current_mind ON TABLE companion FLEXIBLE TYPE object;    -- 現在の心（感情・関係理解・関心事）
DEFINE FIELD mind_revision ON TABLE companion TYPE int DEFAULT 1;
DEFINE FIELD updated_at ON TABLE companion TYPE datetime VALUE time::now();

-- 2. ユーザー基本プロファイル
DEFINE TABLE user_profile SCHEMAFULL;
DEFINE FIELD name ON TABLE user_profile TYPE string;
DEFINE FIELD preferences ON TABLE user_profile TYPE array<string> DEFAULT [];
DEFINE FIELD static_constraints ON TABLE user_profile TYPE array<string> DEFAULT [];
DEFINE FIELD updated_at ON TABLE user_profile TYPE datetime VALUE time::now();

-- 3. エピソード記憶（MemoryRecord）
DEFINE TABLE memory SCHEMAFULL;
DEFINE FIELD owner ON TABLE memory TYPE record<companion>;   -- 不変の原所有者（"shared"で上書きしない）
DEFINE FIELD audiences ON TABLE memory TYPE array<string> DEFAULT []; -- 共有先コンパニオンID一覧
DEFINE FIELD content ON TABLE memory TYPE string;            -- 記憶の要約・事実・手順本文
DEFINE FIELD importance ON TABLE memory TYPE int ASSERT $value >= 1 AND $value <= 10; -- 1〜10: 重要度・想起優先度
DEFINE FIELD occurred_at ON TABLE memory TYPE option<datetime>; -- 出来事の発生日時（不明時はnull）
DEFINE FIELD recorded_at ON TABLE memory TYPE datetime DEFAULT time::now() READONLY; -- 初回記録日時（更新不可）
DEFINE FIELD last_accessed_at ON TABLE memory TYPE option<datetime>; -- 明示的引用による最終利用日時（未引用時はnull）
DEFINE FIELD memory_revision ON TABLE memory TYPE int DEFAULT 1; -- 競合検知用の版
DEFINE FIELD emotion ON TABLE memory TYPE option<string>;     -- 当時の感情タグ
DEFINE FIELD source_context ON TABLE memory FLEXIBLE TYPE object; -- 由来（turn_id, run_id, 話者等）
DEFINE FIELD embedding ON TABLE memory TYPE option<array<float>>; -- 埋め込みベクトル（Alpha標準: 1536次元）
DEFINE FIELD embedding_model ON TABLE memory TYPE option<string>; -- 例: "text-embedding-3-small"
DEFINE FIELD embedding_dimensions ON TABLE memory TYPE option<int>; -- 例: 1536
DEFINE FIELD embedding_version ON TABLE memory TYPE option<string>; -- 例: "v1"

-- 会話ターン（会話ログへの参照点・引用先テーブル）
DEFINE TABLE turn SCHEMAFULL;
DEFINE FIELD session_id ON TABLE turn TYPE string;
DEFINE FIELD turn_number ON TABLE turn TYPE int;
DEFINE FIELD speaker ON TABLE turn TYPE string;
DEFINE FIELD timestamp ON TABLE turn TYPE datetime DEFAULT time::now() READONLY;

-- 4. グラフエッジ（認知的関係性）
DEFINE TABLE remembers SCHEMAFULL TYPE RELATION FROM companion TO memory;
DEFINE TABLE cited_in SCHEMAFULL TYPE RELATION FROM memory TO turn;
DEFINE TABLE supersedes SCHEMAFULL TYPE RELATION FROM memory TO memory;

-- 5. インデックス定義
-- HNSW ベクトルインデックス（セットアップ時に選択されたモデルの次元数で動的定義）
DEFINE INDEX idx_memory_embedding ON TABLE memory FIELDS embedding 
    HNSW DIMENSION <configured_dimensions> DIST COSINE TYPE F32;

-- 日本語2-gram全文検索インデックス
DEFINE ANALYZER japanese_analyzer TOKENIZERS blank,punct FILTERS lowercase,ngram(2,2);
DEFINE INDEX idx_memory_content_fulltext ON TABLE memory FIELDS content 
    FULLTEXT ANALYZER japanese_analyzer;

-- 複合検索・フィルタインデックス
DEFINE INDEX idx_memory_owner_importance ON TABLE memory FIELDS owner, importance;
```

### 3.2 所有権と共有（Privacy & Sharing）
- **原所有者（`owner`）の不変性**: 記憶の `owner` は作成したコンパニオンのIDを常に保持する。他コンパニオンへ共有する場合も `owner = "shared"` で上書きせず、`audiences` 配列に対象コンパニオンIDを追加する。
- **共有解除**: `audiences` から対象IDを削除することで、共有先コンパニオンの検索クエリから即座に不可視化される（原所有者の記憶としては残る）。
- **私的根拠の非漏洩**: 共有先が参照できるのは `content`, `importance`, `occurred_at` のみであり、プライベートな会話ログ全文や個人的感情メモは共有先へ露出させない。

---

## 4. 想起スコアリングと引用ガード

### 4.1 想起スコアリング計算式

Generative Agents と Mem0 の知見を統合し、想起候補の最終スコアは関連度・重要度・直近性の加重和として算出する。

$$\text{Score} = 0.5 \cdot \text{Relevance} + 0.3 \cdot \text{Importance} + 0.2 \cdot \text{Recency}$$

- **Relevance（関連度: $0.0 \sim 1.0$）**:
  - ベクトル意味検索（HNSW）とBM25全文検索を独立して取得し、SurrealDBの `search::rrf`（Reciprocal Rank Fusion）または正規化付き線形結合によって候補IDの和集合から統合したスコア。
  - `AND` 結合で絞り込むと「語句が一致しないが意味的に合致する候補（例: 犬の記憶をペットの問いで探す）」を全文検索が除外してしまうため、**ベクトル候補と全文候補を別々に取得してRRF統合**する。

```sql
-- SurrealQL ハイブリッド検索クエリ例（ベクトル検索と全文検索の独立取得とRRF順位統合）
-- 1. ベクトル意味検索候補（語句不一致でも意味が近い候補を漏らさず取得し、スコア降順で整列）
LET $semantic = (
    SELECT id, content, importance, occurred_at, last_accessed_at,
           vector::similarity::cosine(embedding, $query_vec) AS score
    FROM memory
    WHERE embedding <|20, 40|> $query_vec
    ORDER BY score DESC
    LIMIT 20
);

-- 2. 全文検索候補（語句・キーワード完全一致候補を取得し、スコア降順で整列）
LET $lexical = (
    SELECT id, content, importance, occurred_at, last_accessed_at,
           search::score(1) AS score
    FROM memory
    WHERE content @1@ $query_text
    ORDER BY score DESC
    LIMIT 20
);

-- 3. search::rrf による順位融合（必須引数: 候補リスト配列、結果件数 limit=20、定数 k=60）
RETURN search::rrf([$semantic, $lexical], 20, 60);
```
- **`search::rrf` の引数とスコア尺度合わせ（正規化）**:
  - `search::rrf($lists, $limit, $k)` は必須引数として結果件数の `$limit` を要求し、各入力リストが `score` の降順に整列（`ORDER BY score DESC`）されていることを前提とする。
  - RRFが返す `rrf_score` は各リストにおける順位の逆数和（2リスト・$k=60$ の理論上限は $2 / 61 \approx 0.0328$）であり、そのままでは $0.0 \sim 1.0$ の尺度にはならない。
  - したがって、**「RRFによる順位融合」と「スコアの尺度合わせ（正規化）」は別の処理**として扱い、RRFスコアを理論上限値（$\frac{2}{1+k}$）で除算するかMin-Max正規化を行って $0.0 \sim 1.0$ の `Relevance` へ尺度合わせを行ってから、重要度（`Importance`）・直近性（`Recency`）との加重和に組み込む。
- **マルチプロバイダー埋め込み（ローカル / クラウド選択性）**:
  - セットアップ時や設定画面で、ユーザーの環境やプライバシー要件に応じて埋め込みモデルを柔軟に選択可能とする（ローカル: `fastembed`, `bge-m3`, `all-MiniLM-L6-v2` 等、クラウド: OpenAI `text-embedding-3-small`, Gemini 等）。
  - SurrealDBのHNSWインデックスは、選択されたモデルの出力次元数（`configured_dimensions`）に合わせて初期化される。
  - 設定でモデルを変更した場合は再インデックスマイグレーションが走り、切り替え完了まで検索は安全にBM25全文検索へ縮退する。
- **Importance（重要度: $0.0 \sim 1.0$）**:
  - $1 \sim 10$ の順序尺度。上位の重要事象に強い格差を与える非線形スケールとして正規化（忘却時はこの優先度を引き下げる）。
    - $1 \sim 3$: 日常の雑談、一時的な状況
    - $4 \sim 6$: 趣味嗜好、仕事上の習慣
    - $7 \sim 9$: 約束、大きな決定、重要な出来事
    - $10$: コンパニオンとして常時忘れてはならない中核事実（Pinned Memory）
- **Recency（直近性 / 時間減衰: $0.0 \sim 1.0$）**:
  $$\text{Recency} = e^{-\lambda \Delta t}$$
  - $\lambda \approx 0.05$（半減期約14日）。
  - $\Delta t$: 出来事の発生日時（`occurred_at`、不明な場合は `recorded_at`）からの経過日数。些細な日常会話は自然に減衰し、高重要度の事象は長期間にわたり高い基礎スコアを維持する。

### 4.2 引用ガード（想起自己汚染ループの防止）

- **更新条件の厳格性**:
  - ベクトル検索により想起候補としてプロンプトへ注入されただけでは、`last_accessed_at` は**更新されない**。
  - プロンプトに注入された想起記憶のうち、親（頭）エージェントが応答内で明示的に参照した記憶（ストリーミング応答内の参照メタデータ・引用タグ `<cite id="..."/>`、または明示的な `mind_read` / `mind_update` 操作、またはターン完了時の増分整理判定）が確認された場合のみ、ホストが `last_accessed_at = time::now()` へ更新し、グラフエッジ `memory->cited_in->turn` を作成する。
  - これにより、無関係な記憶が一度検索に引っかかっただけで直近性スコアを引き上げ続け、次回以降も過剰に想起され続ける「想起の自己汚染ループ」を遮断する。

### 4.3 常時注入記憶（Pinned / Core Memory）の制約と保護

`importance == 10` のレコード、および `user_profile` は、ベクトル類似度検索をバイパスしてシステムプロンプトの中核枠へ常時注入される。ただし、以下の不変条件を厳格に適用する：

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
  - **想起スコア計算との連動**:
    - 想起スコア計算式（$\text{Score} = 0.5 \cdot \text{Relevance} + 0.3 \cdot \text{Importance} + 0.2 \cdot \text{Recency}$）において、優先度（`Importance`）が引き下げられることで総スコアが低下する。
    - 他の条件（Relevance, Recency）が同じであれば想起スコアは低減し、重要度10のPinned枠からも外れる。ただし、質問との関連度（`Relevance`）が極めて高い場合などは、人間が過去の記憶を詳細に問われて思い出すのと同様に再び想起されることがある。
  - **保証の明確な境界**:
    - **保証すること**: 忘却は想起優先度を引き下げる操作であり、他の条件が同じなら想起スコアは低下し、重要度10のPinned枠から外れる。遅延した整理ジョブが古い優先度で上書きすることは防止される。
    - **保証しないこと（意図的仕様）**: 関連度が極めて高いクエリにおける完全な不可視化や、直前の会話生ログ（Recent Dialogue History）・既存の要約文（Compaction Summary）からの物理的・遡及的な削除は保証しない。対話の客観的な事実記録としては残り、対話の進展に伴って直前ログが窓から自然に押し出されるか、次回のCompaction要約で抽象化されるのを待つ。
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
