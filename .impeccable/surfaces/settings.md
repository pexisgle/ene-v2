# 設定ウィンドウ 表面ブリーフ

状態: 2026-11-18 改訂。前版（canon + kawaii、5章、説明文あり）はユーザーの指摘で破棄。
決めたこと: 設定項目の実体、行の型、章立て、見た目の制約。
決めていないこと: kawaii のどの系列で見た目を作るか。

## 1. 設定の実体（ホストの型から列挙）

| 単位 | 型 | 中身 |
|---|---|---|
| 接続 | `Connection` | `id, name, enabled, credential_state, revision, kind, last_check, config` |
| 種別 | `ConnectionKind` | LLM / TTS / STT / Embedding / MCP / ACP の6種 |
| 種別固有 | `ConnectionConfig.oneof` | LLM: `model_ids` / TTS: `voice_ids` / STT: `language_hints` / Embedding: `model_id, dimensions` / MCP: `allowed_tools, command, args` / ACP: `command` |
| 共通ヘッダ | `ConnectionEndpoint` | `adapter, endpoint, options_schema(JSON Schema), options(JSON)` |
| カタログ | `LlmProvider` | `id, name, adapter, endpoint, settings[], requires_credentials, environment_variable` |
| 役割 | `ModelAssignments` | `companion`(頭) / `task`(手) / `perception`(目) / `compaction` / `consolidation` / `decision` / `embedding` の7つ |
| 状態 | `ConnectionCheck` | `healthy, checked_at, failure, capabilities, resources` |
| 秘密 | `credential_state` + `RegisterCredentials` / `RevokeCredentials` | 値はホストに留まり、クライアントへ渡らない |

組み込み LLM プロバイダは4つで、**設定項目は既に非対称**:
- OpenAI: `base_url`, `organization`
- OpenRouter: `base_url`, `site_url`, `app_name`
- Anthropic: `base_url`
- Gemini: `base_url`

さらに `options_schema` はプロバイダごとに違う JSON Schema を返す。能力と資源（モデル・音声・ツール）は設定ではなく `ConnectionCheck` の結果から来る。

結論: **固定行では作れない。** 行は一様ではなく、展開の中身が接続ごとに変わる。

## 2. 行の型（決定）

- 行 = 名前 / 種別 / 状態語 / 主操作（再接続）。説明文を書かない。
- 展開 = 種別固有フィールド + プロバイダ固有（`options_schema` から生成）+ 資格情報の登録・削除 + 能力と資源の一覧 + 役割割当。
- 「＋ 接続を追加」= カタログから adapter を選んで1接続作る。
- 折り畳みは1段だけ。入れ子にしない。
- 状態語は `ConnectionCheck` から: 未確認 / 接続しています / 接続中 / 切断 / 資格情報なし。

## 3. 章立て

提案（3章。前版の5章は「承認」を外し、「音声」を接続と割当へ分解した）:

- `01 コンパニオン` — 名前、見た目、声の割当。新しいコンパニオンの入口もここ。
- `02 接続` — 6種の接続すべて。追加・展開・有効化・再接続・資格情報。
- `03 この端末` — クライアント能力、アバター、音声デバイス、いま会話を受け持っているか。

承認待ちと許可の判断はチャット側で完結する。設定には置かない。

## 4. 見た目の制約（決定済み）

- gpui でそのまま描ける範囲: 角丸・1px罫・単色塗り・影1つまで。グラデーション、模様、回転、ぼかし、ブレンドは使わない。
- 説明文を書かない。行は名前・状態・操作だけで成立させる。
- 左上にアプリ名のバッジを置かない（AI製品の雛形に見えるため）。
- 基調は kawaii（水準の参照は Desktop Mate、Project AIRI）。骨格はカテゴリ標準（macOS システム設定、Linear、Raycast）を守る。
- 色は操作できる要素にだけ差す。状態は言葉で持ち、色は補強。

## 5. 状態と幅

- 接続 0（初回）/ 4（典型）/ 12（最大）。展開は同時に1つだけ開く。
- 1行 56–72px。展開は行の高さの2〜8倍。
- 資源一覧は 0（未確認）/ 12（典型）/ 100超（MCPツール）。
- 接続中の再接続中は、行の中で状態語だけが変わる。
- 競合（他端末で変更）は行の中に `読み直す` を出す。

## 6. 未決定

- 見た目の系列（kawaii のどこを取るか）。
- `options_schema` をどの入力に落とすか（文字列・選択・真偽・数値の4種に限定するか）。
- 資源一覧が100件を超える接続（MCP）の扱い。
