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
   - 忘却された記憶は通常想起・Pinned枠から確実に排除され、遅れて完了した自動整理ジョブが古い記憶を復活させる競合を防止する。
   - ベクトル検索空間はモデル名・次元数・前処理版で厳密に識別し、異種ベクトルの混線・汚染を防止する。

---

## 2. 人格と情報の分類

| 情報の種類 | 性質 | 心での扱い |
| :--- | :--- | :--- |
| **作者設定（CharacterProfile）** | 静的設定 | ロールプレイの核。会話で直接書き換えない。配布・エクスポートの対象 |
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
DEFINE FIELD importance ON TABLE memory TYPE int ASSERT $value >= 1 AND $value <= 10;
DEFINE FIELD occurred_at ON TABLE memory TYPE option<datetime>; -- 出来事の発生日時（不明時はnull）
DEFINE FIELD recorded_at ON TABLE memory TYPE datetime DEFAULT time::now() READONLY; -- 初回記録日時（更新不可）
DEFINE FIELD last_accessed_at ON TABLE memory TYPE datetime; -- 明示的引用による最終利用日時
DEFINE FIELD memory_revision ON TABLE memory TYPE int DEFAULT 1; -- 競合検知用の版
DEFINE FIELD is_forgotten ON TABLE memory TYPE bool DEFAULT false; -- 忘却フラグ（通常想起から除外）
DEFINE FIELD emotion ON TABLE memory TYPE option<string>;     -- 当時の感情タグ
DEFINE FIELD source_context ON TABLE memory FLEXIBLE TYPE object; -- 由来（turn_id, run_id, 話者等）
DEFINE FIELD embedding ON TABLE memory TYPE option<array<float>>; -- 埋め込みベクトル（Alpha標準: 1536次元）
DEFINE FIELD embedding_model ON TABLE memory TYPE option<string>; -- 例: "text-embedding-3-small"
DEFINE FIELD embedding_dimensions ON TABLE memory TYPE option<int>; -- 例: 1536
DEFINE FIELD embedding_version ON TABLE memory TYPE option<string>; -- 例: "v1"

-- 4. グラフエッジ（認知的関係性）
DEFINE TABLE remembers SCHEMAFULL TYPE RELATION FROM companion TO memory;
DEFINE TABLE cited_in SCHEMAFULL TYPE RELATION FROM memory TO record;
DEFINE TABLE supersedes SCHEMAFULL TYPE RELATION FROM memory TO memory;

-- 5. インデックス定義
-- HNSW ベクトルインデックス（Alpha標準 1536次元コサイン距離。モデル変更時は再構築）
DEFINE INDEX idx_memory_embedding ON TABLE memory FIELDS embedding 
    HNSW DIMENSION 1536 DIST COSINE TYPE F32;

-- 日本語2-gram全文検索インデックス
DEFINE ANALYZER japanese_analyzer TOKENIZERS ngram 2 3 FILTERS lowercase;
DEFINE INDEX idx_memory_content_fulltext ON TABLE memory FIELDS content 
    FULLTEXT ANALYZER japanese_analyzer;

-- 複合検索・フィルタインデックス
DEFINE INDEX idx_memory_owner_active ON TABLE memory FIELDS owner, is_forgotten, importance;
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
  - クエリと記憶のベクトルコサイン類似度と、BM25全文検索ランクを正規化合成したスコア。
  - 埋め込みモデル・次元数・前処理版が一致するレコードのみを対象とする。
- **Importance（重要度: $0.0 \sim 1.0$）**:
  - $1 \sim 10$ の順序尺度。上位の重要事象に強い格差を与える非線形スケールとして正規化。
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
  - 親（頭）エージェントが発話ツール `talk` の引数 `cited_memories` に明示的に記憶IDを含めた場合のみ、ホストが `last_accessed_at = time::now()` へ更新し、グラフエッジ `memory->cited_in->turn` を作成する。
  - これにより、無関係な記憶が一度検索に引っかかっただけで直近性スコアを引き上げ続け、次回以降も過剰に想起され続ける「想起の自己汚染ループ」を遮断する。

### 4.3 常時注入記憶（Pinned / Core Memory）の制約と保護

`importance == 10` のレコード、および `user_profile` は、ベクトル類似度検索をバイパスしてシステムプロンプトの中核枠へ常時注入される。ただし、以下の不変条件を厳格に適用する：

1. **忘却・訂正の適用**: `is_forgotten == true` の記憶、または訂正されて `superseded_by` を持つ古い記憶は、重要度が10であっても**Pinned枠へ注入してはならない**。
2. **トークン予算上限（Token Budget）**:
   - Pinned Memory枠には最大トークン予算（例: 最大 1,000 トークン）を設定する。
   - 自動整理モデルが誤って重要度10を大量生成した場合でも、Pinned枠だけでプロンプト全体が圧迫される事故を防止する。予算超過時は最新の `last_accessed_at` 順で収容する。

---

## 5. 即時更新と自動整理の整合性

### 5.1 親（頭）の即時更新ツール
親エージェントには以下のツールが提供される（主体・権限・操作IDはホストが自動束縛）：
- `mind_recall(query: String, limit: Option<u32>) -> Vec<MemoryItem>`: 記憶の検索
- `mind_read(memory_id: String) -> MemoryDetail`: 記憶の詳細および由来の閲覧
- `mind_update(action: MindUpdateAction) -> MindUpdateResult`: 明示的な記憶の追加・訂正・忘却

### 5.2 自動整理（Consolidation）の起動条件と整合性
対話ログからの長期記憶形成は、**Compaction（要約）の発生を必須条件とせず、独立してバックグラウンド実行**される。
- **起動契機**: ユーザー対話が途切れたアイドル時（60秒以上）、または未整理の確定ターン数が閾値（例: 5ターン）に達した時点。

```text
未処理の対話ログ・子の作業報告
  → 未処理範囲と出所を取得（忘却指定されたturn_idは再抽出対象から除外）
  → 既存記憶と現在の版（memory_revision）・忘却状態（is_forgotten）を参照
  → 経験の抽出（importance, occurred_at, emotion）
  → 既存記憶の統合・訂正、引用実績に基づく重要度再評価
  → トランザクション内で競合検証（版不一致・忘却済み記憶の再生成を拒否）
  → 記憶・処理済みカーソルを原子的コミット
```

### 5.3 遅延整理ジョブの競合解決
- ユーザーが「今の話は忘れて」と指示して `is_forgotten = true` または版が進んだ後、バックグラウンドで走っていた古い整理ジョブが同じ記憶を書き戻そうとした場合：
  - トランザクション内で `memory_revision` および `is_forgotten` を照合し、**古い整理結果を拒否（Conflict）**して破棄する。
  - 同一の出所（`turn_id`）に紐づく忘却記録が存在する場合、別IDでの新規再抽出も遮断し、忘却したはずの記憶が形を変えて遅れて復活する不整合を防止する。

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
| **忘却の厳密性** | 記憶を忘却（`is_forgotten = true`）させた後、通常想起およびPinned枠を検証 | 通常想起・Pinned枠のいずれからも完全に除外され、再整理ジョブによって復活しないこと |
| **遅延整理の競合排除** | 記憶変更後に、変更前の古い文脈から生成した整理結果をコミット試行 | 版不一致（Conflict）として安全に拒否され、最新状態が上書きされないこと |
| **引用ガードの厳密性** | 検索候補としてプロンプトに注入された記憶と、`talk` で引用された記憶を比較 | 検索されただけでは `last_accessed_at` が更新されず、`cited_memories` に含まれた場合のみ更新されること |
| **Pinned予算制限** | 重要度10の記憶を多数作成し、プロンプト生成を実行 | 予算上限（例: 1,000トークン）を超過せず、文脈窓を圧迫しないこと |
| **埋め込み空間の隔離** | 異なる埋め込みモデル名・次元数のベクトルを登録して検索 | 次元不一致・モデル違いのベクトルが混入せず、一致する空間のみで類似度計算が行われること |
