# 仕様シナリオ集

[機能一覧](../features/README.md) に定義された各機能が、実際のユーザー利用場面でどのように振る舞うかを具体的な対話・行動ベースでまとめたシナリオ集です。

本シナリオ集は、クライアントUIと対話体験に関する共通原則（[機能一覧](../features/README.md#クライアント画面の範囲)）を前提としています。

- **窓口の一貫性**：作業の進捗・相談・最終報告、スケジュール確認、記憶の確認・訂正・忘却はすべてコンパニオンとの対話で完結します。
- **画面の最小構成**：クライアント画面は「文字入力と文字表示」「設定」「承認カード」に限り、専用の進捗パネル、作業一覧、質問カード、知覚の常設表示、記憶一覧UI、保留一覧は設けません。
- **「頭」「手」「目」の役割分担**：ユーザーと向き合う「頭」（コンパニオンAgent）がテンポを維持し、非同期の実務はプラグイン化された「手」、知覚情報の選別・要約は軽量モデルの「目」が担います。

## シナリオ一覧

| No. | シナリオ | 主な登場要素・場面 | 主に対象とする機能 |
| --- | --- | --- | --- |
| 01 | [日常の音声雑談・即時Web検索・割り込みとテキスト切替](01-daily-voice-chat-and-search.md) | デスクトップ常駐での雑談、天気を聞いて即答、割り込み質問、会議室でテキスト切替 | [Chat](../features/chat/README.md), [Voice](../features/voice/README.md), [Web検索](../features/tools/search.md), [デスクトップ表示](../features/character/desktop.md) |
| 02 | [作業・横断リサーチの依頼、並行雑談、方針変更と相談](02-task-delegation-and-research.md) | 見積書と評判の比較依頼、雑談しながらの方針変更、定期的な情報交換を通じた相談、スキル学習 | [作業の実行](../features/task/request.md), [調査の依頼](../features/task/research.md), [記憶機能](../features/memory/README.md) |
| 03 | [記憶の形成・時間減衰・対話での忘却とCompaction連動整理](03-memory-lifecycle-and-compaction.md) | 好みとアレルギーの記憶、FTS5＋ベクトル想起と引用ガード、「忘れて」、トークン閾値でのCompaction一括整理 | [記憶機能](../features/memory/README.md), [Chat](../features/chat/README.md) |
| 04 | [「目」による見守り・価値ある変化の選別と自発声かけ](04-watch-over-and-autonomy.md) | 軽量モデル「目」によるノイズ除外、エラー往復を捉えた声かけ、集中時の静かな見守り | [見守り](../features/autonomy/watch-over.md), [デスクトップ表示](../features/character/desktop.md) |
| 05 | [対話でのスケジュール登録・実行と不在時の継続](05-schedule-and-away-execution.md) | 朝のニュースやリマインドの登録・確認・解除、切断中の継続、再接続時の自然な報告 | [スケジュール実行](../features/autonomy/schedule.md), [ホストとクライアント](../features/hosting/README.md) |
| 06 | [機械的許可・Decision API・承認カードと会話代理承認](06-permissions-and-approval.md) | 3段階判定パイプライン、自由文ポリシー、承認カードと `resolve_approval` 代理承認、離席中の保留 | [権限とデータの管理](../features/settings/permissions.md), [Tools](../features/tools/README.md) |
| 07 | [複数コンパニオン同席時の会話・タスク・共有記憶](07-multi-companion-conversation.md) | 「凪」と「レイ」の同時常駐、名指しと文脈に応じた参加、個人/共有スコープの記憶 | [Voice](../features/voice/README.md), [ホストとクライアント](../features/hosting/README.md), [記憶機能](../features/memory/README.md) |
| 08 | [PC⇄スマホ引き継ぎ・クライアント能力登録と未接続時保留](08-multi-device-handover.md) | Tailscale/Cloudflare接続、別端末からの呼び出し切替、`RegisterClient` 能力、未接続時の安全保留 | [ホストとクライアント](../features/hosting/README.md), [Tools](../features/tools/README.md) |
| 09 | [「体・声・心」の作成・プレビュー・分割配布と成長](09-character-creation-and-growth.md) | キャラ定義とプレビュー、秘密・記憶を含まないパッケージ書き出し、感情反映と成長 | [キャラクター作成](../features/character/create.md), [感情](../features/character/mind.md), [成長](../features/character/grow.md) |
| 10 | [用途別モデル割り当てとAPI障害時の切り替え](10-model-settings-and-failover.md) | 頭/手/目/Compaction別のモデル配分、送信データ確認、障害時の代替モデル切替 | [AIの選択・接続](../features/settings/models.md), [感情](../features/character/mind.md) |
| 11 | [プラグインによる「手」「目」「体」の拡張と差し替え](11-extensions-and-plugins.md) | MCP・ACPラッパー「手」・Wasm・知覚センサー・モーション追加と設定画面での差し替え | [機能の拡張](../features/extensions/README.md) |

## 機能領域別カバレッジ

- **Chat**（[chat/README.md](../features/chat/README.md)）：01, 02, 03, 07, 09
- **Character**（[create.md](../features/character/create.md), [desktop.md](../features/character/desktop.md), [mind.md](../features/character/mind.md), [grow.md](../features/character/grow.md)）：01, 04, 07, 09, 10
- **Memory**（[memory/README.md](../features/memory/README.md)）：02, 03, 07, 09
- **Tools**（[tools/README.md](../features/tools/README.md), [search.md](../features/tools/search.md)）：01, 02, 03, 05, 06, 08
- **Voice**（[voice/README.md](../features/voice/README.md)）：01, 06, 07
- **Task**（[request.md](../features/task/request.md), [research.md](../features/task/research.md)）：02, 06, 07, 11
- **Autonomy**（[schedule.md](../features/autonomy/schedule.md), [watch-over.md](../features/autonomy/watch-over.md)）：04, 05, 07
- **Hosting**（[hosting/README.md](../features/hosting/README.md)）：05, 07, 08
- **Settings**（[models.md](../features/settings/models.md), [permissions.md](../features/settings/permissions.md)）：06, 08, 10
- **Extensions**（[extensions/README.md](../features/extensions/README.md)）：08, 11
