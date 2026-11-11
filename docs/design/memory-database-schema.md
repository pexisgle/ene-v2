# 心・記憶データモデルとデータベース設計

状態：Alpha採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[心と自動記憶](mind.md)、[技術スタック](tech-stack.md)に基づく、記憶・感情・関係性・データベース（SurrealDB）の正本仕様である。

---

## 1. 概念モデル

キャラクターAIの「心」と「記憶」は、単なるテキストの断片や検索用ベクトルではなく、**「人格の核」「エピソード記憶」「関係性グラフ」「現在の感情状態」**が相互に結びついた認知構造として定義される。

```text
  ┌────────────────────────────────────────────────────────────┐
  │                        Companion                           │
  │  ・コア人格（自己像、価値観、対人姿勢、話し方）            │
  │  ・現在の心（MindState: 感情、関係認識、関心事）           │
  └───────┬────────────────────────────┬───────────────────────┘
          │ (remembers)                │ (relates_to)
          ▼                            ▼
  ┌───────────────────────────┐    ┌───────────────────────────┐
  │       MemoryRecord        │    │           User            │
  │  ・出来事・事実の要約      │    │  ・呼称、基本属性、前提    │
  │  ・重要度（1〜10）         │    └───────────────────────────┘
  │  ・発生日時 / 記録日時     │                   ▲
  │  ・最終引用日時（ガード付）│                   │
  │  ・当時の感情タグ          │                   │ (experienced_with)
  │  ・HNSWベクトル埋め込み    │───────────────────┘
  └───────────────────────────┘
```

---

## 2. データベース論理スキーマ（SurrealQL定義）

永続化エンジンには、ドキュメント・グラフ・ベクトルを統合処理できる組み込みSurrealDB（SurrealKVエンジン）を採用する。

### A. コンパニオン定義（`companion`）
```surrealql
DEFINE TABLE companion SCHEMAFULL;

DEFINE FIELD name ON TABLE companion TYPE string;
DEFINE FIELD core_profile ON TABLE companion TYPE object;
-- コアプロファイル（自己像、価値観、話し方等）
DEFINE FIELD core_profile.identity ON TABLE companion TYPE string;
DEFINE FIELD core_profile.values ON TABLE companion TYPE string;
DEFINE FIELD core_profile.tone ON TABLE companion TYPE string;

-- 現在の心（MindState: 感情、関係性の理解、関心事）
DEFINE FIELD current_mind ON TABLE companion TYPE object;
DEFINE FIELD current_mind.emotion ON TABLE companion TYPE string;
DEFINE FIELD current_mind.emotion_target ON TABLE companion TYPE string;
DEFINE FIELD current_mind.concerns ON TABLE companion TYPE array<string>;
DEFINE FIELD current_mind.updated_at ON TABLE companion TYPE datetime DEFAULT time::now();
```

### B. ユーザー基本プロファイル（`user_profile`）
```surrealql
DEFINE TABLE user_profile SCHEMAFULL;

DEFINE FIELD preferred_name ON TABLE user_profile TYPE string;
-- ユーザーに関する中核前提・ルール（常時注入）
DEFINE FIELD core_facts ON TABLE user_profile TYPE array<string>;
DEFINE FIELD updated_at ON TABLE user_profile TYPE datetime DEFAULT time::now();
```

### C. エピソード記憶（`memory`）
```surrealql
DEFINE TABLE memory SCHEMAFULL;

-- 所有者（特定コンパニオン、または全コンパニオン共有の "shared"）
DEFINE FIELD owner ON TABLE memory TYPE string;
-- 記憶本文（出来事、好み、合意事項、知見）
DEFINE FIELD content ON TABLE memory TYPE string;
-- 重要度: 1〜10（順序尺度・非線形優先度）
DEFINE FIELD importance ON TABLE memory TYPE int ASSERT $value >= 1 AND $value <= 10;
-- 出来事の発生日時（推測または明示された日時。不明ならnull）
DEFINE FIELD occurred_at ON TABLE memory TYPE option<datetime>;
-- レコード生成日時
DEFINE FIELD recorded_at ON TABLE memory TYPE datetime DEFAULT time::now();
-- 実際に発話や行動で引用された最終日時（引用ガードにより更新）
DEFINE FIELD last_accessed_at ON TABLE memory TYPE datetime DEFAULT time::now();
-- 出来事当時の感情タグ・受け止め
DEFINE FIELD emotion ON TABLE memory TYPE option<string>;
-- 根拠情報（発言出所、元ログ範囲、作業セッション参照）
DEFINE FIELD source_context ON TABLE memory TYPE string;
-- 想起状態（通常想起 / 抑制 / 忘却）
DEFINE FIELD recall_preference ON TABLE memory TYPE string 
  ASSERT $value IN ['normal', 'suppressed', 'forgotten'] DEFAULT 'normal';
-- 意味検索用ベクトル埋め込み（1536次元）
DEFINE FIELD embedding ON TABLE memory TYPE option<array<float, 1536>>;

-- インデックス定義
-- HNSWベクトルインデックス（コサイン類似度）
DEFINE INDEX memory_vector ON TABLE memory FIELDS embedding HNSW DIMENSION 1536 DIST COSINE;
-- 全文検索インデックス（BM25）
DEFINE INDEX memory_fulltext ON TABLE memory FIELDS content SEARCH ANALYZER standard BM25;
-- 所有者・想起設定の複合インデックス
DEFINE INDEX memory_lookup ON TABLE memory FIELDS owner, recall_preference, importance;
```

### D. 認知グラフ関係性（Graph Edges）
SurrealQLのグラフエッジを用い、意味論的な関係性を多次元に結線する。
```surrealql
-- コンパニオンと記憶の紐付け
DEFINE TABLE remembers SCHEMAFULL;
DEFINE FIELD in ON TABLE remembers TYPE record<companion>;
DEFINE FIELD out ON TABLE remembers TYPE record<memory>;

-- 記憶の引用履歴（どの会話ターンで引用されたか）
DEFINE TABLE cited_in SCHEMAFULL;
DEFINE FIELD in ON TABLE cited_in TYPE record<memory>;
DEFINE FIELD out ON TABLE cited_in TYPE record<dialogue_turn>;
DEFINE FIELD cited_at ON TABLE cited_in TYPE datetime DEFAULT time::now();

-- 記憶の訂正・上書き関係
DEFINE TABLE supersedes SCHEMAFULL;
DEFINE FIELD in ON TABLE supersedes TYPE record<memory>;
DEFINE FIELD out ON TABLE supersedes TYPE record<memory>;
DEFINE FIELD reason ON TABLE supersedes TYPE string;
```

---

## 3. 想起スコアリングと検索パイプライン

### A. スコア算出式
記憶の想起は、Generative AgentsとMem0の枠組みを統合した以下の加重和モデルに基づいて計算される。

$$\text{Score} = w_{\text{rel}} \cdot \text{Relevance} + w_{\text{imp}} \cdot \text{Importance} + w_{\text{rec}} \cdot e^{-\lambda \Delta t}$$

| 要素 | 重み | 定義・算出方法 |
| :--- | :---: | :--- |
| **Relevance（関連度）** | $w_{\text{rel}} = 0.5$ | クエリ埋め込みとの**HNSWコサイン類似度**（$[0, 1]$）およびBM25全文検索スコアの正規化値 |
| **Importance（重要度）** | $w_{\text{imp}} = 0.3$ | 1〜10の非線形スケール。日常雑談（1〜3）、継続的習慣（4〜6）、重要事象（7〜9）。$[0, 1]$ へ正規化 |
| **Recency（直近性）** | $w_{\text{rec}} = 0.2$ | $e^{-\lambda \Delta t}$。$\lambda \approx 0.05$（半減期約14日）。$\Delta t$ は出来事の発生日（または記録日）からの経過日数 |

### B. 常時注入記憶（Pinned / Core Memory）
- **バイパス処理**:
  `importance == 10` のレコード、および `user_profile`（ユーザーの呼称、譲れない大前提）は、類似度計算やベクトル検索を介さず、**プロンプトの中核枠へ常時注入**される。
- **目的**:
  ユーザーの名前や決定的な約束事が、話題の揺らぎによってベクトル検索から落ちてしまう「想起漏れ」を物理的に防止する。

---

## 4. 最終アクセス日時の更新ガード（Access Update Guard）

検索された記憶が無関係に再浮上し続ける「自己汚染ループ」を防ぐため、厳格なアクセス更新ガードを定義する。

```text
[クエリ検索実行] ───> [想起候補としてプロンプトへ注入]
                                │
               ┌────────────────┴────────────────┐
               ▼ (モデルが発話で引用しなかった)     ▼ (明示的に引用した)
       【更新しない（何もしない）】        talk(cited_memories=["memory:123"])
                                                 │
                                                 ▼
                                     [last_accessed_at = time::now()]
                                     [cited_in エッジを追加]
```

1. **未引用の保護**:
   ベクトル検索や語句検索にヒットしてプロンプトの文脈へ配置されただけでは、`last_accessed_at` は更新されない。
2. **明示引用時の更新**:
   親（頭）の推論が `talk` ツールを呼び出し、引数の `cited_memories` に記憶ID（例: `memory:xyz`）を明示して発話した場合のみ、ホストが `last_accessed_at` を現在日時に更新し、`cited_in` エッジを記録する。
3. **記憶整理へのフィードバック**:
   引用頻度と履歴は、後述のアイドル時自動整理において、記憶の重要度再評価や記憶テキストの洗練を行う際の客観的証拠として利用される。

---

## 5. 記憶自動整理（Consolidation）のライフサイクル

記憶の抽出と定着は、ユーザーとの対話が途切れたアイドル時に、Compactionジョブと同期してバックグラウンドで実行される。

```text
未処理の対話ログ ＋ 直近の記憶引用履歴
  │
  ▼
[記憶整理モデル（purpose=memory）]
  │
  ├─ 1. 新規エピソードの抽出 (importance 1〜10, occurred_at, emotion, embedding)
  ├─ 2. 既存記憶の更新・統合 (重複情報のマージ)
  ├─ 3. 引用実績に基づく重要度の再調整 (よく引用される記憶の強化、誤想起の抑制)
  └─ 4. 訂正関係の結線 (supersedes エッジの生成)
  │
  ▼
[SurrealDB トランザクションコミット]
```

### 訂正と忘却のセマンティクス
- **訂正（Superseding）**:
  新情報で過去の認識を訂正する際、過去の記憶レコードを物理削除せず、`supersedes` グラフエッジで結ぶ。これにより「現在はリンゴが好きだが、以前はミカンが好きだった」という経緯を失わずに両立できる。
- **忘却（Forgetting）**:
  ユーザーから「忘れて」と指示された場合、レコードの物理削除は行わず、`recall_preference = 'forgotten'` に設定して通常想起の対象から恒久的に除外する。
