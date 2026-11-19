# ene-v2 統合監査レポート

不足機能・不具合・設計差分・リファクタリング

作成日：2026-11-19（日本時間）

**2回の既存監査を統合したレポート。現在のHEADを新たに検証した結果ではありません。**

## 目次

1. 要点・対象・読み方
2. 仕組み単位の実装状態と未接続点
3. 優先度付き統合一覧
4. 不具合・具体的不整合の詳細
5. 不足機能・統合未達の詳細
6. 追加検証が必要な候補
7. リファクタリング候補と変更の境界
8. 設計判断・非採用・文書の不一致
9. 実装ロードマップと依存関係
10. 受入テストと完了判定
11. 以前の監査との対応・重複整理
12. 根拠資料・確認範囲・留保

## 01 要点・対象・読み方

**結論：不足の中心は、個別機能の数ではなく、機能をつなぐ契約・権限・対象・寿命の不整合である。**

LLM確認、Wasm実行、能力登録RPC、アバターIPCは存在する。しかし、それぞれの存在だけでは、拡張を導入して権限を与え、イベントを届け、対象に作用させ、停止・交換するまでの体験は完成しない。既存の音声停止・承認保存・作業寿命などの不具合を直すことと、この接続不足を埋めることを同じ計画で扱う。詳細と根拠は各IDおよび第2章に示す。

### 1.1 対象と資料の位置付け

| 資料 | 対象コミット | 今回の扱い |
| --- | --- | --- |
| 監査A：不具合・小機能・構造 | 8a9a6bd4be81ec3e84bf3c30698d06f3af107498 | 既存Markdown台帳の55項目を継承。B/Q/F/RのIDを維持。 |
| 監査B：仕組み・拡張の接続 | f28e138aa929e96d92f1a8477f836b10552c4f7f | 前回答の大枠監査を統合。B20、F16〜F24を追加し、関連既存項目を補足。 |
| 本統合版 | 2026-11-19 作成 | 既存2監査の編集・統合。新しいHEADの再監査ではない。 |

**確認版の違いを維持する。** Aで確認した細部がBや現在のHEADでもすべて残っていると判断したものではない。今回、追加のコード取得、ビルド、テスト実行、実機・GPU・音声・ネットワーク検証は行っていない。「確認」は各既存監査が記録した確認水準を示す。着手時には対象HEADとの差分と再現を取り直す。

### 1.2 件数と分類

| 区分 | 件数 | 意味 |
| --- | --- | --- |
| B：不具合・具体的不整合 | 20 | 静的経路または仕様差の指摘。20件の実機再現済みバグという意味ではない。 |
| F：不足機能・統合未達 | 24 | 完成像への不足。現行Alphaの必須要件と将来拡張を区別する。 |
| Q：追加検証・設計リスク | 5 | 発生条件や影響の確認が残る。確定バグへ自動昇格しない。 |
| R：構造改善の候補 | 16 | B/F/Qを小さい変更で直すための整理。同じ問題を別角度から扱う。 |
| 合計 | 65 | 管理レコード数。独立したバグや独立した実装作業の総数ではない。 |

別途、9件の設計判断Dを示す。12領域Mは全体を把握するための分類であり、件数には加算しない。

### 1.3 優先度と読順

P1は認可・処理喪失・既存機能の破壊を先に直す。P2は主要な機能経路・設定帰属・拡張の接続を完成させる。P3は完成像への追加・精度向上・整理で、リリース対象として採用した時点で順序を決める。重大度だけでなく、安全に変更するための依存関係を第9章に示す。

意思決定は第2章と第8章、実装着手は第3章の一覧と第4〜7章の個票、受入判定は第10章を参照する。第11章で以前の回答との対応を追跡できる。

## 02 仕組み単位の実装状態と未接続点

| 領域 | 実装状態 | 主な関連ID |
| --- | --- | --- |
| M01 権限のLLM確認と承認 | 実装あり／適用範囲と意味が不統一 | B02・B03・B04・B05・B06・B18 ほか |
| M02 Sandboxと実行境界 | Wasm隔離あり／OS隔離は現行非採用 | B19・F19・F23・D01 |
| M03 プロバイダーのプラグイン化 | 内蔵＋追加プラグイン／モダリティ差あり | B07・B11・Q03・F04・F09・F22 ほか |
| M04 Character・フック・ホストサービス・Wasmツール | 契約やサンプルが先行／製品への接続不足 | F17・F18・F19・F20・F12・R06 ほか |
| M05 ene-avatarとdesktopのイベント連携 | 描画指示あり／反応の往復が未完成 | B09・F01・F02・F14・F15・F21 ほか |
| M06 「目」「体」とブラウザ・IDE・ギミック拡張 | ホストの受付あり／desktopの拡張実行不足 | F11・F14・F17・F18・F19・F21 ほか |
| M07 「手」・外部ACP・手順学習 | 組み込み／Wasmハーネスあり／外部接続と学習不足 | B08・F06・F19・F23・R01 |
| M08 MCPとWeb検索 | 基本実行あり／認証・認可・交換性不足 | B03・B04・B17・F13・F24・R04 ほか |
| M09 拡張パッケージの導入・管理 | 起動時読込あり／利用者向けライフサイクル不足 | F12・F17・F19・F20・F23 |
| M10 音声対話の完成度 | VAD・STT/TTSあり／停止契約と対話モード不足 | B01・B11・Q01・Q02・F04・F07 ほか |
| M11 記憶・文脈・自律スケジュール | 基盤あり／起動原因・注入・配送の整合性不足 | B07・B10・B13・B14・Q03・Q05 ほか |
| M12 複数コンパニオン・端末・キャラクター設定 | CRUDとrenderer基盤あり／利用経路の統合不足 | B11・B15・B16・Q04・F01・F02 ほか |

### M01 権限のLLM確認と承認

**存在する機能：** HoldOfでの保留、Decisionモデルの呼出し、allow／require_approval、タイムアウト時の承認カード、親による代理承認は存在する。

**不足・境界：** MCP・検索を含む共通認可、自由文ポリシーのUI、HumanRequiredとNeedsDecisionの分離が不足する。

**完成させる方向：** 操作の出所ではなく作用と対象で判定する。すべてにLLM確認を追加するのではなく、必要な場合だけ補助判定へ回す。

**対応項目：** B02 / B03 / B04 / B05 / B06 / B18 / B19 / B20 / F16 / R04 / R05

**根拠：** [SB01](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/permission/mod.rs) [SB02](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/permissions.md) [SB04](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/ui/device.rs)

### M02 Sandboxと実行境界

**存在する機能：** Extismのネットワーク禁止、WASI無効、メモリ・時間・fuel制限と、ホストサービスの宣言・使用量の検査がある。

**不足・境界：** ネイティブコマンドのOS隔離は現在の採用設計に含まれない。実装漏れとして件数に足さず、必要なら要件の変更として扱う。

**完成させる方向：** Wasmコードの制限、許可付きホストサービス、OSプロセスの終了管理、OS隔離を別の保証として説明する。LLM判定やprocess-wrapをSandboxの代用品と表現しない。

**対応項目：** B19 / F19 / F23 / D01

**根拠：** [SB20](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/mod.rs) [SB12](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/invocation.rs) [SB02](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/permissions.md)

### M03 プロバイダーのプラグイン化

**存在する機能：** 後段の採用記録は標準をRig内蔵、追加をプラグインとするハイブリッド。LLMの互換形式を使う転送、TTS/STTの呼出しとbrokerはある。

**不足・境界：** EmbeddingのHTTP配線、接続単位の一貫した設定解決、逐次音声の契約が未完成。LLMの公開生成形式も限定的で、任意の生成方式まで対応済みではない。

**完成させる方向：** まず同じ呼出し文脈を全モダリティへ渡す。全provider外出しや任意API対応は別の方針変更として判断し、既存の内蔵経路を理由なく削除しない。

**対応項目：** B07 / B11 / Q03 / F04 / F09 / F22 / R06 / R10 / R13 / D02

**根拠：** [SB25](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/provider-plugins.md) [SB26](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/model/plugin.rs) [SB27](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/mind/plugin.rs) [SB17](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/voice/plugin.rs)

### M04 Character・フック・ホストサービス・Wasmツール

**存在する機能：** CharacterEventと行動戻り値、複数のServiceRequest、provider/harnessのWasm実行基盤はある。

**不足・境界：** Character専用パッケージの実行所有者、設計フックの登録・配送、設定／状態／ツール等の実サービス、汎用Wasmツールを提供する公開契約が不足する。

**完成させる方向：** ロード→イベント入力→行動の許可→実行→停止を一本の利用経路にする。SDKの型やテストだけで対応済みと表示しない。

**対応項目：** F17 / F18 / F19 / F20 / F12 / R06 / R16

**根拠：** [SB05](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/character.rs) [SB06](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/wire.rs) [SB09](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs) [SB07](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/mod.rs) [SB10](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/services.rs) [SB08](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md)

### M05 ene-avatarとdesktopのイベント連携

**存在する機能：** モデル、モーション、表情、サイズ、口パクの指示と、読込待ち・終了後の待機復帰がある。renderer自身はクリックや状態も通知する。

**不足・境界：** クリックを反応へ渡す処理、応答待ち中の通知保持、状態変更からUI／能力への追随、renderer IDとcompanion IDの対応付けが不足する。

**完成させる方向：** IPCの方式を置き換えるより、要求応答と通知の独立消費、型付きイベント、対象の帰属を先に完成させる。バイナリ統合はこの修正の前提ではない。

**対応項目：** B09 / F01 / F02 / F14 / F15 / F21 / R03 / R12

**根拠：** [SB14](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-avatar/src/character.rs) [SB13](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-avatar/src/desktop/pointer.rs) [SB15](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/avatar.rs) [SB16](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/session.rs)

### M06 「目」「体」とブラウザ・IDE・ギミック拡張

**存在する機能：** RegisterClient、SetCapabilities、DeviceStream、ReportPerceptionで能力・観測を受け取るホスト境界はある。

**不足・境界：** desktopにパッケージをロードし、追加センサーや身体機能を登録・実行・解除する機構が不足する。固定presetを増やすだけでは外部拡張にならない。

**完成させる方向：** クライアントに属するOS操作をホストへ戻さず、導入・許可・能力公開・停止を最小の拡張経路として接続する。

**対応項目：** F11 / F14 / F17 / F18 / F19 / F21 / R16

**根拠：** [SB03](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-rpc/proto/host.proto) [SB28](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/capabilities.rs) [SB08](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md)

### M07 「手」・外部ACP・手順学習

**存在する機能：** WorkSession／Run、作業報告、WasmハーネスのStart／Prompt／Respond／Cancelはある。

**不足・境界：** Target::Acpから外部実行ファイルへ至るnative bridgeと、SKILL.mdの作成・索引・段階的読込が不足する。作業の寿命が再購読へ結び付く問題もある。

**完成させる方向：** harness契約を利用し、実際の外部ACP通信をテスト用プロセスで完結させる。技能は知識であって許可ではなく、通常の認可を維持する。

**対応項目：** B08 / F06 / F19 / F23 / R01

**根拠：** [SB11](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/work/hand.rs) [SB20](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/mod.rs) [SB09](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs)

### M08 MCPとWeb検索

**存在する機能：** MCPのstdio／HTTP接続、許可ツールの登録、組み込みWeb検索はある。

**不足・境界：** MCP資格情報や起動設定の適用、共通認可、失敗時の再接続制御、検索provider選択が不足する。

**完成させる方向：** サーバーへの接続許可と各ツールの操作許可を分離する。検索も選択済み接続として扱い、MCPの認証成功だけで全操作を許可しない。

**対応項目：** B03 / B04 / B17 / F13 / F24 / R04 / R08

**根拠：** [SB22](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/mcp.rs) [SB01](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/permission/mod.rs) [SA08](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/web_search.rs)

### M09 拡張パッケージの導入・管理

**存在する機能：** ディレクトリのパッケージ検証・読込と、参照するprovider/harnessの開始はある。

**不足・境界：** 導入、互換性・権限表示、状態表示、停止、交換、削除を利用者が完了できる導線が不足する。connection.enabledだけで代替できない。

**完成させる方向：** 安全な取り込みと再起動反映から始める。実行中の依存関係を確認し、交換失敗で稼働済みの接続を壊さない。ストアやホットリロードは必須化しない。

**対応項目：** F12 / F17 / F19 / F20 / F23

**根拠：** [SB07](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/mod.rs) [SB25](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/provider-plugins.md)

### M10 音声対話の完成度

**存在する機能：** VAD、マイク・スピーカー、文単位合成、世代、口パク、明示停止の構成がある。

**不足・境界：** 停止後の再生回復、遅延世代とリサンプラの境界、PTT、自動割込み、逐次STT/TTS、Realtimeセッションを区別して整備する必要がある。

**完成させる方向：** 既存の再生・取消を直してから入力モードと自然な割込みを追加する。文単位ストリーミングとRealtime対話を混同しない。

**対応項目：** B01 / B11 / Q01 / Q02 / F04 / F07 / F08 / F09 / F22 / R07 / R13

**根拠：** [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs) [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA33](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/voice/README.md) [SB17](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/voice/plugin.rs)

### M11 記憶・文脈・自律スケジュール

**存在する機能：** 記憶保存・検索、pinned/shared、compaction、予定保存・発火の実装はある。

**不足・境界：** 観測・予定由来の想起、最終入力と注入済み記憶の対応、Embedding空間の識別、モデルが使えない時の予定配送に不足がある。

**完成させる方向：** 一回のターンの原因→検索→予算内の構築→注入実績を一貫させる。予定の到来と会話受付を分け、外部操作のexactly-onceまでは保証しない。

**対応項目：** B07 / B10 / B13 / B14 / Q03 / Q05 / F06 / R09 / R10 / R14

**根拠：** [SA18](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/autonomy.rs) [SA26](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/recall.rs) [SA13](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/embedding.rs)

### M12 複数コンパニオン・端末・キャラクター設定

**存在する機能：** コンパニオンの保存、body／voiceフィールド、端末登録、rendererの複数体処理はある。

**不足・境界：** 同時常駐、bodyの描画反映、接続と話者の結合、テンプレート配布、作成プレビュー、端末ID、担当復元、外部ホスト接続が未完成。

**完成させる方向：** 表示選択、常駐、音声所有者、作業対象を分けてから画面を追加する。保護された代替経路がない段階でOwner Bearerを削除しない。

**対応項目：** B11 / B15 / B16 / Q04 / F01 / F02 / F03 / F04 / F05 / F10 / F21 / R03

**根拠：** [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto) [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA42](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/host-access.md) [SA34](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md)

## 03 優先度付き統合一覧

一覧は管理用の索引である。確認版A/Bは第1章の固定コミットを指し、未実装と非採用を混同しない。詳細な確認区分・影響・受入条件は後続の個票にまとめる。

### 不具合・具体的不整合

| ID | 優先度 | 確認版 | 項目 |
| --- | --- | --- | --- |
| B01 | P1 | A | 再生停止が、その後の全発話まで永久に失効させる |
| B02 | P1 | A | 保存ルールが対象ではなく、任意の文字列引数に一致する |
| B03 | P1 | A | MCP ツールが共通権限パイプラインを迂回する |
| B04 | P1 | A | 組み込み Web 検索も外部送信の判定から外れている |
| B05 | P1 | A | ポリシー読取失敗を空ルールとして扱う |
| B06 | P1 | A | 承認の永続化前に待機解除と実行許可が進む |
| B07 | P1 | A | HTTP を使う埋め込みプラグインのサービス配線が欠落 |
| B08 | P1 | A | 知覚・設定変更による再接続で実行中の作業が破棄される |
| B09 | P1 | A | アバター IPC の同期・無期限待機がセッションを止め得る |
| B10 | P1 | A | モデル利用不能時に一回限りの予定を消費する |
| B11 | P2 | A | 選んだ話者と、使用する TTS 接続が結び付いていない |
| B12 | P2 | A | 読み上げ上の制限をテキスト回答にも適用している |
| B13 | P2 | A | 自動想起の検索語が現在の起動原因と一致しない |
| B14 | P2 | A | 実際に注入されない記憶まで注入済みとして記録する |
| B15 | P2 | A | 複数端末の既定クライアント ID が衝突する |
| B16 | P2 | A | 既存 activation の復元経路では再取得フラグが残る |
| B17 | P2 | A | MCP 接続失敗の直後に再起動を繰り返す |
| B18 | P2 | A | 「常に承認」の保存失敗が利用者に分からない |
| B19 | P2 | A | Hard Deny の実装対象が採用設計より狭い |
| B20 | P1 | B | 「毎回、人間に確認」と「LLMで補助判定」を区別できない |

### 不足機能・統合未達

| ID | 優先度 | 確認版 | 項目 |
| --- | --- | --- | --- |
| F01 | P2 | A | 同一端末で複数コンパニオンを同時に常駐させる |
| F02 | P2 | A | コンパニオンの body 設定から描画までの接続 |
| F03 | P2 | A | キャラクターテンプレートと生活インスタンス、配布用往復 |
| F04 | P2 | A | 声の複数プリセットとコンパニオンへの割当 |
| F05 | P2 | A | キャラクター作成時のプレビューと最初の出会い |
| F06 | P2 | A | SKILL.md による手順の学習・再利用 |
| F07 | P2 | A | Push-to-talk と VAD の明示的切替 |
| F08 | P2 | A | 自動バージインとエコー対策を伴う同時対話 |
| F09 | P3 | A | Realtime 音声モデル用のセッション契約 |
| F10 | P2 | A | 外部ホスト接続と採用済みアクセス設計への移行 |
| F11 | P2 | A | 目・体のクライアント側プラグイン実行基盤 |
| F12 | P3 | A | プラグイン導入・状態確認・交換の利用者向け導線 |
| F13 | P2 | A | 検索プロバイダを選ぶ設定 |
| F14 | P3 | A | 視線追従・ウィンドウとの身体的な関わり |
| F15 | P2 | A | 輪郭どおりのクリック透過 |
| F16 | P2 | B | 自由文ポリシーを設定・保存するデスクトップ導線 |
| F17 | P2 | B | Character拡張を製品アプリから実行する接続 |
| F18 | P2 | B | 設計されたライフサイクルフックの登録・配送 |
| F19 | P2 | B | プラグイン向け設定・状態・ツール等のホストサービス |
| F20 | P2 | B | 汎用Wasmツールを公開・登録する拡張契約 |
| F21 | P2 | B | アバター操作・状態をコンパニオンの反応へ結ぶイベント配送 |
| F22 | P2 | B | TTS/STTプラグインの継続ストリーミング契約 |
| F23 | P2 | B | 外部ACP実行ファイルへの実接続アダプター |
| F24 | P2 | B | 認証付きMCPとサーバー別起動設定の接続管理 |

### 追加検証・設計リスク

| ID | 優先度 | 確認版 | 項目 |
| --- | --- | --- | --- |
| Q01 | P2 | A | 古い音声世代が新しい再生を上書きし得る |
| Q02 | P2 | A | リサンプラの flush・発話末尾処理が不足する |
| Q03 | P2 | A | 埋め込み空間の ID が異なる実体を区別できない |
| Q04 | P2 | A | 非同期 UI 結果の接続世代・設定 revision の帰属 |
| Q05 | P2 | A | ストレージ読取失敗を不存在と見なす復元経路 |

### リファクタリング候補

| ID | 優先度 | 確認版 | 項目 |
| --- | --- | --- | --- |
| R01 | P1 | A | Session を「接続の寿命」と「実行の寿命」に分ける |
| R02 | P2 | A | Hub から初期設定・接続検証・RPC ワークフローを外す |
| R03 | P2 | A | 選択中・常駐中・操作対象・音声所有者を別概念にする |
| R04 | P1 | A | 共通 CandidateAction と型付き target を導入する |
| R05 | P1 | A | 承認状態遷移を一か所に閉じる |
| R06 | P1 | A | ProviderInvocationContext を全モダリティで共有する |
| R07 | P1 | A | 音声を識別子と状態遷移で整理する |
| R08 | P2 | A | MCP の接続管理とツール実行を分離する |
| R09 | P2 | A | 記憶検索の起動原因と ContextBuilder を分離する |
| R10 | P2 | A | 埋め込み transport・索引・backfill を分離する |
| R11 | P2 | A | 読み上げ整形を RPC クレートから外す |
| R12 | P2 | A | アバター IPC の DTO と JSON Schema の重複を減らす |
| R13 | P2 | A | ホスト voice.rs の接続・変換・発話制御を分離する |
| R14 | P2 | A | 欠落・無効・取得失敗を型で区別する |
| R15 | P3 | A | proto をサービス／領域で整理する |
| R16 | P2 | A | 能力宣言と予約ツール名の正本を統一する |

## 04 不具合・具体的不整合の詳細

Bは、既存機能が期待する契約と一致しない箇所である。静的な根拠を示しており、発生頻度・実測性能・全OSでの再現を保証するものではない。

### B01 [P1] 再生停止が、その後の全発話まで永久に失効させる

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `Session::stop_playback` は `player.flush(u64::MAX)` を呼ぶ。`Player::flush` は失効済み世代を `fetch_max` で保持し、`push` はその値以下の世代をすべて捨てる。`open_devices` は既存 Player を再利用する。

**利用者・運用への影響：** 一度停止すると、同じ Player に届く以後のすべての世代が再生対象から外れる。通常の再接続では回復しない。

**最小の修正方針：** 「いまの再生を止める」と「ホストが指定した世代まで失効させる」を別操作にする。無限大をキャンセルの代用品にしない。

**完了条件：** 世代 N を停止した後に N の遅延チャンクは拒否し、N+1 は再生する。ホスト再起動時の世代リセットも検証する。

**関連ID：** Q01 / Q02 / F08 / F22 / R01 / R07

**根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs)

### B02 [P1] 保存ルールが対象ではなく、任意の文字列引数に一致する

**確認版：** A　**確認区分：** コード経路を確認／設計にも修正が必要。

**確認内容：** `matches` は `json_contains_matching_str` により、引数中の任意の文字列を許可パターンと比較する。パスや宛先に限定していない。

**利用者・運用への影響：** 例えば `work.write` の `safe/*` 許可が、対象パスは `outside.txt` なのに本文が `safe/example` というだけで一致する。操作対象を限定した許可として安全に解釈できない。設計 §5 にも任意の文字列との一致が書かれており、§3 の `(action_type, target)` という要求と矛盾する。

**最小の修正方針：** 実行側で正規化した action と typed target のみを照合する。引数全体の完全一致ルールと、対象範囲のルールを別の型にする。本文・説明・workspace 表示文字列を対象認可に使わない。

**完了条件：** 許可対象文字列が本文だけに含まれる書き込みは許可されない。read の許可で write/delete/execute が許可されない。

**関連ID：** B03 / B04 / B19 / B20 / R04

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) [SA04](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md) [SA05](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/work_tools.rs)

### B03 [P1] MCP ツールが共通権限パイプラインを迂回する

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `host_owned` は MCP が提供するツールをホスト所有として扱い、gate が保存ルール・Decision API・承認処理の前に通過させる。MCP の実行側にも同等の操作単位の判定はない。

**利用者・運用への影響：** MCP の接続・ツール利用を許可することと、個別の送信・書き込み等を許可することが区別されない。拒否ルールを設定した利用者の期待と実際の実行がずれる。

**最小の修正方針：** 会話・記憶などの内部管理操作と、外部に効果を持つ操作を分ける。MCP、端末能力、組み込み外部ツールを共通 CandidateAction で判定する。

**完了条件：** MCP ツールについて deny/confirm/allow、Decision エラー、承認中の引数変更の各経路を確認する。

**関連ID：** B04 / B20 / F19 / F20 / F24 / R04 / R08

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) [SA06](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mcp.rs) [SA07](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/extensions/README.md) [SA04](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md)

### B04 [P1] 組み込み Web 検索も外部送信の判定から外れている

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `web_search` は BUILTIN_TOOLS に含まれ、ホスト所有として gate を通過する。実装は固定の Mwmbl URL にクエリを送る。検索サービスの接続設定や操作単位の許可をこの実行経路では解決しない。

**利用者・運用への影響：** 外部送信を制御するという製品上の約束と整合しない。実際に機密情報が送られたと確認したわけではないが、その送信先と許可を共通機構で制御できない。

**最小の修正方針：** 検索を明示的に選択された外部サービスとして扱い、他の外部操作と同じ判定・送信先制御を適用する。

**完了条件：** 検索未設定・無効化・deny の場合に通信が発生しない。許可された接続の送信先だけに送る。

**関連ID：** B03 / F13 / R04

**根拠：** [SA08](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/web_search.rs) [SA09](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/client/mod.rs) [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) [SA10](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/tools/search.md) [SA11](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/PRODUCT.md)

### B05 [P1] ポリシー読取失敗を空ルールとして扱う

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** gate が `store.policy_rules().unwrap_or_default()` を使用する。読取失敗と「ルールなし」が同じ値になる。

**利用者・運用への影響：** 保存された deny/confirm を読めなかったときに既定の Auto 判定などへ進めるため、認可の失敗が許可拡大に変わる。

**最小の修正方針：** 読取失敗は実行不可または明示的な保留とする。空の正規状態と、取得不能の状態を区別する。

**完了条件：** policy_rules の読取に障害を注入し、既知・未知どちらの能力でも自動実行されないことを確認する。

**関連ID：** B06 / Q05 / R05 / R14

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) [SA04](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md)

### B06 [P1] 承認の永続化前に待機解除と実行許可が進む

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `submit` は `release` で hold を取り除いた後、承認成功の保存より先に `Approved` を挿入する。後段の `store.resolve_approval` が失敗し得る。

**利用者・運用への影響：** RPC はエラー、DB は pending なのに、ECS 側では待機が外れ承認済みという不整合を作る。

**最小の修正方針：** 検証、DB の状態遷移、実行可能化の順を定める。失敗時に実行されない単一の遷移責任者を設ける。DB と ECS を無理に一つの一般化トランザクション基盤にする必要はない。

**完了条件：** 引数ハッシュ確認・承認保存・常時許可保存の各段階で失敗させ、API 応答・DB・実行可否が矛盾しないことを確認する。

**関連ID：** B05 / B18 / B20 / R05 / R14

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs)

### B07 [P1] HTTP を使う埋め込みプラグインのサービス配線が欠落

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `mind/plugin.rs` の埋め込み呼び出しは `NoServices` を渡し、その handler は全サービス要求を拒否する。`Running::call` は渡されたサービスをそのまま Instance に渡す。LLM・音声向けの `BrokerServices` は存在するが、この経路では使わない。さらに endpoint/options は接続設定でなく manifest の値を使う。

**利用者・運用への影響：** HTTP broker を必要とする埋め込みプラグインが動かず、ユーザーの接続先やオプションの変更も正しく反映できない。

**最小の修正方針：** 他モダリティと同じ connection-specific な呼び出し文脈から endpoint/options/credential grants/broker を渡す。

**完了条件：** ローカル mock HTTP を使う埋め込みプラグインを実行する。同じ adapter の二つの接続で送信先・資格情報・設定が混ざらないことを確認する。

**関連ID：** Q03 / F19 / R06 / R10

**根拠：** [SA12](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/plugin.rs) [SA13](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/embedding.rs) [SA14](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/mod.rs) [SA15](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/services.rs)

### B08 [P1] 知覚・設定変更による再接続で実行中の作業が破棄される

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `SetEyesEnabled`、`Refresh` 等は serve の再開始を要求する。作業タスクの JoinSet は serve の寿命に属する。シェルは future が破棄されると RunningShell の Drop でプロセスを終了させる。

**利用者・運用への影響：** 「目」を切り替えただけで実行中のシェルが止まる。一方、spawn_blocking で開始済みのファイル処理には同じキャンセル保証を適用できず、結果報告と実際の副作用が離れる危険もある。

**最小の修正方針：** 設定購読・接続と、受付済み操作の実行器を別の寿命にする。明示的な Stop と購読再作成を区別し、完了結果の配送も実行器が管理する。

**完了条件：** 長時間シェル・ファイル編集の途中で知覚切替、TTS 設定変更、通信断を起こす。意図しない中断・二重実行・完了の喪失がないことを確認する。

**関連ID：** B09 / F23 / R01

**根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA16](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/update.rs) [SA05](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/work_tools.rs)

### B09 [P1] アバター IPC の同期・無期限待機がセッションを止め得る

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `Avatar::call` は同期 stdin 書込と `Receiver::recv()` を使用し、応答待ちに期限がない。Session のコマンド処理・アバター操作から直接呼ぶ。

**利用者・運用への影響：** 子プロセスが生存したまま応答を返さないと、同じセッションの停止・音声・知覚処理まで進まなくなり得る。async 関数の中に置いても同期待機は非同期化されない。

**最小の修正方針：** IPC を独立した所有タスクまたは専用スレッドに置き、要求期限と取消を持つ。結果だけをセッションへ戻す。遅延応答 ID の扱いも定める。

**完了条件：** 応答しない偽 renderer を使っても停止・音声制御・終了が進む。タイムアウト後の遅延応答が次の要求へ混入しない。

**関連ID：** F21 / R01 / R12

**根拠：** [SA17](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/avatar.rs) [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs)

### B10 [P1] モデル利用不能時に一回限りの予定を消費する

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** `poll_schedules` は `mark_schedule_fired` の後で `agent::deliver` を呼び、その bool 結果を確認しない。deliver は ensure_model が失敗すると通知して false を返し、会話には配送しない。

**利用者・運用への影響：** 発火時にモデルが束縛できないだけで一回限りの予定が無効化される。保存から配送までの間のクラッシュにも喪失窓がある。

**最小の修正方針：** 予定の到来、配送待ち、会話への受付を区別する。occurrence の一意キーを持つ永続 outbox を小さく導入し、受付済みを重複配送しない。外部副作用の exactly-once まで保証したと表現しない。

**完了条件：** 予定時刻にモデル未設定、設定を修復後に一度だけ配送される。保存直後の再起動でも重複・喪失がない。

**関連ID：** B13 / Q05

**根拠：** [SA18](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/autonomy.rs) [SA19](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/agent.rs) [SA20](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/autonomy/schedule.md)

### B11 [P2] 選んだ話者と、使用する TTS 接続が結び付いていない

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** キャラクター編集画面は複数接続の話者候補を表示するが、選択値は voice_identifier の文字列である。`voice_connection` はコンパニオンを受け取らず、先頭の有効な TTS 接続を返す。

**利用者・運用への影響：** 別接続の話者を選んでも違うエンジンへ話者 ID が渡る。話者 ID が接続間で重複した場合も曖昧になる。

**最小の修正方針：** VoiceBinding を connection_id、model/preset、voice_id の組として扱う。表示名からエンジンを推測しない。

**完了条件：** 二つの TTS 接続に同じ話者 ID を用意し、どちらの選択も指定した接続へ届くことを確認する。

**関連ID：** F04 / R03 / R06 / R13

**根拠：** [SA21](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/companions.rs) [SA22](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/voice.rs) [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto)

### B12 [P2] 読み上げ上の制限をテキスト回答にも適用している

**確認版：** A　**確認区分：** 仕様との差を確認。

**確認内容：** 共通の SPOKEN_RESPONSE は URL・パス・コード等を発言全体で禁止する。会話履歴の assistant 表示にも plain_reply を適用する。plain_reply 自体はコードや URL を全面削除するわけではないが、表・Markdown・絵文字などの表示情報を変換する。

**利用者・運用への影響：** 文字では URL を本文中に示す検索仕様と矛盾し、コピー可能なコードや構造化結果の扱いも音声都合に制約される。

**最小の修正方針：** モデル出力の原文、画面表示、読み上げ投影を分離する。内部モーション JSON の漏れ防止は、回答全体の形式禁止ではなく出力境界で扱う。

**完了条件：** コード・表・リンクを含む回答について、画面と履歴は原文を保ち、音声には別の自然な読み上げ文が渡る。

**関連ID：** F13 / F18 / R11

**根拠：** [SA19](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/agent.rs) [SA24](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/chat.rs) [SA25](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/src/speech.rs) [SA10](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/tools/search.md)

### B13 [P2] 自動想起の検索語が現在の起動原因と一致しない

**確認版：** A　**確認区分：** 仕様との差を確認。

**確認内容：** `recall_query` は最後のユーザー自身の発言のみを使う。知覚だけで起動したターンでは query を None にする。

**利用者・運用への影響：** 予定が起動したターンは昔のユーザー発言で検索し、知覚ターンは自動想起をしない。記憶仕様が挙げる知覚・スケジュール由来の想起がつながらない。

**最小の修正方針：** UserMessage、Perception、Schedule 等の TurnCause から検索語を組み立てる。不要な観測まで検索するのではなく、原因と検索予算を明示する。

**完了条件：** ユーザー発言のない予定・観測ターンでも、その予定や観測に関連する記憶が選ばれる。

**関連ID：** B14 / F06 / R09

**根拠：** [SA26](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/recall.rs) [SA27](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/memory/README.md)

### B14 [P2] 実際に注入されない記憶まで注入済みとして記録する

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** 想起した ID 一覧を `record_injections` で保存した後、プロフィール・状態・pinned・記憶を連結し、全体を 6,000 文字で切る。

**利用者・運用への影響：** 前のセクションが大きいと、後ろの記憶が切り落とされたのに注入済み記録へ残る。実際の提示内容を根拠とする追跡の意味が崩れる。

**最小の修正方針：** セクション・記憶単位で予算配分して最終的な入力を構築し、その入力に残った ID だけを記録する。

**完了条件：** 大きなプロフィールと pinned を用意しても、記録した ID 集合と最終リクエストに載った記憶集合が一致する。

**関連ID：** B13 / R09

**根拠：** [SA26](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/recall.rs) [SA28](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/mind.md)

### B15 [P2] 複数端末の既定クライアント ID が衝突する

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** Options の既定 ID は `desktop`。登録時には同じ ID の既存 revision を使って更新する。環境変数による回避は可能だが、通常の導入で端末ごとに一意にならない。

**利用者・運用への影響：** 二台が同じクライアントとして登録・能力更新される。複数端末を素直に導入できる完成像と合わない。

**最小の修正方針：** インストールごとに生成して保存する安定 UUID と、利用者向け表示名を分ける。ホスト切替時の関連付けも明示する。

**完了条件：** 初期設定なしの二台が異なる ID を持ち、それぞれ再起動後も同じ ID を維持する。

**関連ID：** B16 / F01 / F10 / R03

**根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA29](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/hosting/README.md)

### B16 [P2] 既存 activation の復元経路では再取得フラグが残る

**確認版：** A　**確認区分：** 静的な状態遷移から確認／実機再現は未実施。

**確認内容：** `may_activate` は初期 true。bind が「既に自端末へ activation 済み」の経路で戻ると、この値を消費しない。他端末へ移動した通知による再接続時に、自端末が再び Activate し得る。

**利用者・運用への影響：** 利用者が別端末へ移した直後、復元状態から起動していた端末が取り戻す経路を作る。

**最小の修正方針：** 自分で明示的に移動を要求した一回の intent と、購読の復元を区別する。既存自端末 activation を採用した時点でも intent を消費する。

**完了条件：** 既存の自端末 activation で起動→別端末へ移動→通知・再購読、という順序で勝手に取り戻さない。

**関連ID：** B15 / F01 / R01 / R03

**根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs)

### B17 [P2] MCP 接続失敗の直後に再起動を繰り返す

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** 失敗イベントで connecting を除去し、同じ同期ループで未接続の wanted 接続を再開する。接続失敗の待ち時間や試行段階がこの経路にない。

**利用者・運用への影響：** 不正なコマンド・停止中のサーバーで、繰り返し起動や通信試行を行う。実測した頻度の主張ではない。

**最小の修正方針：** 接続状態に retry_at、失敗種別、回数を持たせ、指数バックオフと上限を設ける。明らかな設定不正は修正待ちにする。

**完了条件：** 存在しないコマンド、接続拒否、復旧を順に与え、有限頻度で試行し、復旧後に一つだけ稼働する。

**関連ID：** F24 / R08

**根拠：** [SA06](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mcp.rs)

### B18 [P2] 「常に承認」の保存失敗が利用者に分からない

**確認版：** A　**確認区分：** コード経路を確認。

**確認内容：** AlwaysApprove のルール保存が失敗しても警告ログだけで、承認自体は成功として返る。なお、常時許可の保存機能自体は実装されている。

**利用者・運用への影響：** 一回だけ許可された状態を、常時許可の設定に成功したと誤解しやすい。

**最小の修正方針：** 今回の実行許可と恒久ルール保存の結果を別に返すか、一貫した保存単位にする。

**完了条件：** ルール保存に失敗した場合、画面・会話上で恒久許可は保存されなかったことが分かる。

**関連ID：** B06 / R05

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs)

### B19 [P2] Hard Deny の実装対象が採用設計より狭い

**確認版：** A　**確認区分：** 仕様との差を確認。

**確認内容：** 設計はシステムファイル破壊や既知の危険コマンドの無条件拒否を挙げるが、hard_denied は金融系などの能力名 prefix 四種類を判定するだけ。シェル実行側は承認された任意のコマンドを OS 権限で実行する。

**利用者・運用への影響：** 「危険コマンドを機械的に拒否する」と説明できる実装にはなっていない。OS sandbox がないこと自体は採用設計の違反ではない。

**最小の修正方針：** 約束する無条件拒否の範囲を具体的な action/target で定義し、実行直前にも適用する。任意シェルを名前や文字列だけで完全に安全判定できるとは主張しない。

**完了条件：** 定義した禁止対象について、保存 allow や Decision の許可があっても拒否される。未保証の範囲は UI・仕様で正確に説明される。

**関連ID：** B02 / B03 / R04

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) [SA05](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/work_tools.rs) [SA04](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md)

### B20 [P1] 「毎回、人間に確認」と「LLMで補助判定」を区別できない

**確認版：** B　**確認区分：** 第2次監査の静的コード経路。実機再現・最新HEADでの再確認は未実施。

**確認内容：** 保存された明示的な毎回確認ルールと、既定で未判定の能力が同じ Verdict::Confirm に集約され、その後のLLM判定へ進むと報告された。確認を要求する理由が後段の判定へ区別して渡らない。

**利用者・運用への影響：** 利用者が「必ず自分に確認」と設定していても、Decisionモデルのallowによって人間の確認を省略する経路になり得る。LLM判定機能の不存在ではなく、判定結果の意味の不足である。

**最小の修正方針：** 機械的なAllow／Deny／HumanRequired／NeedsDecisionを区別する。HumanRequiredはDecisionモデルのallowで解放せず、ユーザーの明示回答（採用設計で認めた親の代理確定を含む）で解決する。内部管理操作を含めて無差別にLLMを呼ぶ変更にはしない。

**完了条件：** 同じ操作にconfirmルールを設定し、Decisionモデルが必ずallowを返す条件でも実行されない。ユーザー承認後だけ一度実行される。未判定操作の自動判定とエラー時の確認移行は維持される。

**関連ID：** B02 / B03 / B05 / B06 / F16 / R04 / R05

**根拠：** [SB01](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/permission/mod.rs) [SB02](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/permissions.md)

## 05 不足機能・統合未達の詳細

Fは、完成像を利用者の一連の操作として成立させるための不足である。内部API・型・サンプルがあることと、導入から利用・停止まで完了できることを区別する。

### F01 [P2] 同一端末で複数コンパニオンを同時に常駐させる

**確認版：** A　**確認区分：** 統合が不足。

**確認内容：** Session は一つの Binding を持ち、切替時に前の担当を解除する。renderer 自体は複数キャラクターを扱える。

**利用者・運用への影響：** 表示を複数にするだけでは、音声・知覚・能力操作を正しいコンパニオンへ帰属できない。

**最小の修正方針：** 端末の接続と companion ごとの binding/display/audio を分ける。UI の選択中と常駐中を分ける。

**完了条件：** 二体の表示、片方への入力、相互に独立した音声・記憶・停止・切替が成立する。

**関連ID：** B15 / B16 / F02 / F21 / R03

**根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA17](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/avatar.rs) [SA32](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-avatar/README.md) [SA29](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/hosting/README.md) [SA33](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/voice/README.md)

### F02 [P2] コンパニオンの body 設定から描画までの接続

**確認版：** A　**確認区分：** 統合が不足。

**確認内容：** RPC には body があるが、現行 desktop は Options の共通 model を使い、描画 ID も options.id を使う。

**利用者・運用への影響：** コンパニオンごとに別の外見を持つという完成像に到達しない。

**最小の修正方針：** companion_id から BodyBinding とモデル参照を解決し、端末別の位置・大きさとは分離する。

**完了条件：** 二つの companion に別モデルを割り当て、切替・再起動・複数表示で外見が正しい。

**関連ID：** F01 / F03 / F21 / R03 / R12

**根拠：** [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto) [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA34](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md)

### F03 [P2] キャラクターテンプレートと生活インスタンス、配布用往復

**確認版：** A　**確認区分：** 完成像への追加が必要。

**確認内容：** 現在の利用者向け経路は Companion の作成・編集が中心。テンプレート／稼働個体を分離し、心・体・声または組を配布して再利用する一連の導線が不足する。

**利用者・運用への影響：** 会話・個人記憶・秘密を持つ稼働個体と、他人へ配るキャラクター定義を混同しやすい。

**最小の修正方針：** 配布単位とインスタンス単位を定義する。import/export でローカルパス・資格情報・個人記憶を明示的に扱う。

**完了条件：** export→別環境で import が成立し、秘密・個人記憶が勝手に含まれず、依存アセット不足を説明できる。

**関連ID：** F02 / F04 / F05

**根拠：** [SA21](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/companions.rs) [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto) [SA34](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md)

### F04 [P2] 声の複数プリセットとコンパニオンへの割当

**確認版：** A　**確認区分：** 完成像への追加が必要。

**確認内容：** 現在の編集は単一 voice_identifier 中心で、声のプリセットを接続・モデル・設定とまとめて扱う完成像まで届いていない。

**利用者・運用への影響：** 接続を跨ぐ話者選択、配布・復元、声の切替を安全に表現しづらい。

**最小の修正方針：** B11 の VoiceBinding を土台にプリセットを導入する。最初から任意型の設定フレームワークを作らない。

**完了条件：** プリセットを切り替え、保存・再起動・キャラクターの移植後も接続と話者の対応が維持される。

**関連ID：** B11 / F03 / R06 / R13

**根拠：** [SA21](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/companions.rs) [SA22](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/voice.rs) [SA34](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md)

### F05 [P2] キャラクター作成時のプレビューと最初の出会い

**確認版：** A　**確認区分：** 確認した UI 導線では不足。

**確認内容：** 作成フォームは名前・人格の核・話し方を入力して作成する。仕様のプレビューと初回の振る舞いを一続きに確認できる導線がない。TTS の Speak テストとは役割が異なる。

**利用者・運用への影響：** 作成前に声・外見・応答の組合せを確認できず、設定ミスとキャラクター体験の切り分けが難しい。

**最小の修正方針：** 本番の記憶や会話を汚さない試用と、作成完了時の最小の初回体験を定義する。

**完了条件：** プレビューを破棄しても本番の履歴・記憶が増えず、失敗時に不足した接続・素材が分かる。

**関連ID：** F02 / F03 / F04

**根拠：** [SA21](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/companions.rs) [SA34](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md)

### F06 [P2] SKILL.md による手順の学習・再利用

**確認版：** A　**確認区分：** README で未実装と明示。

**確認内容：** ホスト README の未実装項目。記憶の保存・想起があることは、作業手順の技能化を実装したことにはならない。

**利用者・運用への影響：** 一度成功した作業を、検証可能な手順として次回の手へ渡す完成像が欠ける。

**最小の修正方針：** 技能の索引→選択された技能本文の段階的読込→適用→更新、という小さな経路を作る。技能を実行権限の代わりにしない。

**完了条件：** 学んだ手順を次回選べる。無関係な技能全文を常時投入しない。技能中の操作にも通常の権限判定が適用される。

**関連ID：** F19 / F23

**根拠：** [SA35](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/README.md) [SA27](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/memory/README.md) [SA36](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/task/request.md)

### F07 [P2] Push-to-talk と VAD の明示的切替

**確認版：** A　**確認区分：** 機能経路が不足。

**確認内容：** 現行の入力は VAD とミュートを中心とする。確認した Command 契約には PTT 押下・解除またはモードの操作がない。

**利用者・運用への影響：** 発話区間を利用者が明示する運用を選べない。

**最小の修正方針：** 入力モードと録音許可を区別し、押下・解除で一つの区間を確定する。

**完了条件：** PTT 押下中だけ取り込み、解除後に最終化される。ミュート・切断・ウィンドウフォーカス変化で録音が残らない。

**関連ID：** F08 / R07

**根拠：** [SA16](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/update.rs) [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs) [SA33](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/voice/README.md)

### F08 [P2] 自動バージインとエコー対策を伴う同時対話

**確認版：** A　**確認区分：** README で制限を明示。

**確認内容：** 現在は AEC を前提にしない明示停止の経路。自分の出力を拾うことを避ける都合で、自然な発話割込みに制限がある。

**利用者・運用への影響：** ユーザーが話し始めたときに自然に発話を止め、入力へ切り替える完成像が欠ける。

**最小の修正方針：** まず B01/Q01/Q02 のキャンセル契約を直す。その後、再生参照信号を使うエコー対策と VAD の責務を分けて追加する。

**完了条件：** 自分のスピーカー音では割り込まず、利用者の発話では適切に停止・認識し、古い TTS は復活しない。

**関連ID：** B01 / Q01 / Q02 / F07 / F22 / R07

**根拠：** [SA37](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/README.md) [SA35](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/README.md) [SA33](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/voice/README.md) [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs)

### F09 [P3] Realtime 音声モデル用のセッション契約

**確認版：** A　**確認区分：** 完成像への追加が必要。

**確認内容：** 現行公開経路は STT 入力・テキスト・TTS 出力の分離が中心。プラグイン拡張種別にも Realtime セッションの表現がない。

**利用者・運用への影響：** Realtime 接続を STT/TTS の単なる別実装として差し替えるだけでは、双方向セッション・中断・ツール応答などを表現できない。

**最小の修正方針：** 通常の STT/TTS 経路を壊さず、双方向セッションの最小の操作・イベント契約を追加する。

**完了条件：** 音声入力・音声出力・中断・ツール呼出し・再接続が一つのセッションとして整合する。

**統合時の補足：** F22の逐次STT/TTSと、F09の双方向Realtimeモデルは別の契約である。生成APIの形式追加をすべて今すぐ必須にする提案ではない。

**関連ID：** F08 / F22 / R07 / R13

**根拠：** [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto) [SA38](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-plugin/src/manifest.rs) [SA33](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/voice/README.md) [SA39](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/settings/models.md)

### F10 [P2] 外部ホスト接続と採用済みアクセス設計への移行

**確認版：** A　**確認区分：** 移行未完了が明示されている。

**確認内容：** 共通 RPC クライアントは数値の loopback IP と HTTP のみを受け付ける。host-access の外部入口・クライアント側接続方式の設計は現在の接続実装に接続されていない。

**利用者・運用への影響：** URL 入力画面を追加するだけでは HTTPS・ドメイン名の外部入口へ接続できない。

**最小の修正方針：** ローカル discovery とアクセス方式を分離し、採用設計の安全な外部入口に接続する。現行 Bearer を代替境界のない状態で削除しない。

**完了条件：** ローカル運用が維持され、選択した外部方式でも到達・拒否・期限切れ・再接続を扱える。

**関連ID：** B15 / B16 / R03

**根拠：** [SA40](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/src/client.rs) [SA41](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/host.rs) [SA42](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/host-access.md) [SA29](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/hosting/README.md)

### F11 [P2] 目・体のクライアント側プラグイン実行基盤

**確認版：** A　**確認区分：** ホスト側基盤とは別に不足。

**確認内容：** desktop の能力は固定の body.motion/eye.watch など。manifest target は Host/Acp であり、desktop は ene-plugin-runtime に依存しない。provider registry は provider/harness を持たないパッケージを拒否する。

**利用者・運用への影響：** 新しいブラウザー/IDE の観測や身体表現をプラグインとして追加する完成像に、ホストの provider plugin があるだけでは届かない。

**最小の修正方針：** クライアント側の検出・ロード・能力宣言・許可・停止を一つの最小拡張経路として設計する。OS 操作をホストへ戻さない。

**完了条件：** 小さな追加センサー／身体能力を、本体改変なしに追加・停止でき、宣言と実行能力が一致する。

**統合時の補足：** 第2次監査では、ブラウザ／IDE連携、追加モーション、ウィンドウギミックを含む「ロード→能力公開→実行→解除」の不足として具体化した。個別のAPIがあることをdesktopへのプラグイン導入対応と同一視しない。

**関連ID：** F12 / F14 / F17 / F18 / F19 / F21 / R16

**根拠：** [SA43](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/capabilities.rs) [SA44](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/Cargo.toml) [SA38](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-plugin/src/manifest.rs) [SA14](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/mod.rs) [SA07](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/extensions/README.md)

### F12 [P3] プラグイン導入・状態確認・交換の利用者向け導線

**確認版：** A　**確認区分：** 完成像への追加が必要。

**確認内容：** 現行 registry は起動時にディレクトリからパッケージを読む。設定画面とパッケージ導入・失敗理由・有効化の一連の経路が不足する。

**利用者・運用への影響：** 非専門家向けに差し替え可能と説明するには、手動配置と再起動だけでは不足する。

**最小の修正方針：** 最初は安全なインストール・検証結果・再起動要否の表示から作る。ホットリロードを現在の採用設計で必須と決めつけない。

**完了条件：** 正常・不正・非互換パッケージを区別し、失敗しても既存の稼働接続を壊さない。

**統合時の補足：** 導入、検証結果、要求権限、状態、無効化、交換、削除の最小管理を対象とする。接続のenabled切替と、パッケージ自体の実行・削除は別責務。初期版は安全な再起動反映でもよく、マーケットプレイスやホットリロードは前提にしない。

**優先度の扱い：** 元のP3を維持。非専門家による拡張導入を次のリリース条件にする場合は、この項目をP2へ引き上げる。

**関連ID：** F11 / F17 / F19 / F20 / F23

**根拠：** [SA14](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/mod.rs) [SA30](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/mod.rs) [SA45](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/provider-plugins.md) [SA07](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/extensions/README.md)

### F13 [P2] 検索プロバイダを選ぶ設定

**確認版：** A　**確認区分：** 仕様との差を確認。

**確認内容：** 検索機能仕様は利用するプロバイダを選び、API キー等を設定する。実装は Mwmbl 固定。

**利用者・運用への影響：** 利用者が選んだ検索サービス・ローカル検索・既存 MCP 等へ差し替える完成像と合わない。

**最小の修正方針：** 検索の内部契約を小さく定め、選択した接続を利用する。組み込み実装を残すなら選択可能な adapter として扱う。

**完了条件：** 二つの検索接続を切り替え、検索先と資格情報と結果の出典が正しく対応する。

**関連ID：** B04 / B12 / F24

**根拠：** [SA08](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/web_search.rs) [SA10](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/tools/search.md)

### F14 [P3] 視線追従・ウィンドウとの身体的な関わり

**確認版：** A　**確認区分：** 完成像に向けた統合が不足。

**確認内容：** 確認した desktop 能力は主にプリセット motion と expression。ウィンドウに腰掛ける等の行為や、マウス等を対象にした視線の制御を一つの利用経路として宣言・実行していない。VRM 下位機能の有無とは別の指摘。

**利用者・運用への影響：** 動作プリセットを増やすだけでは、環境と相互作用する「暮らすキャラクター」の完成像にならない。

**最小の修正方針：** 視線ターゲット、配置制約、簡単な環境相互作用を必要な能力として追加する。まずクライアント内で実現し、その後拡張可能にする。

**完了条件：** ウィンドウ移動・DPI 変更・対象消滅時にも状態が破綻しない。

**関連ID：** F11 / F17 / F21

**根拠：** [SA43](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/capabilities.rs) [SA32](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-avatar/README.md) [SA46](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/desktop-client.md) [SA47](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/desktop.md)

### F15 [P2] 輪郭どおりのクリック透過

**確認版：** A　**確認区分：** 近似実装との差を確認。

**確認内容：** renderer の hit region はメッシュ AABB から作る矩形。コードにも隙間と morph の変形を厳密には扱わないとある。

**利用者・運用への影響：** 透明な隙間をクリックできない／実際に見えている場所と当たり判定がずれる可能性があり、輪郭単位という仕様は未達。

**最小の修正方針：** 必要な精度・更新頻度を決め、alpha/mask またはより精密な picking へ置き換える。性能を測って方式を選ぶ。

**完了条件：** 髪・腕の間、透過素材、表情変形、複数キャラクター、異なる DPI でクリックと透過が仕様に合う。

**関連ID：** F14 / F21

**根拠：** [SA48](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-avatar/src/desktop/mod.rs) [SA47](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/desktop.md)

### F16 [P2] 自由文ポリシーを設定・保存するデスクトップ導線

**確認版：** B　**確認区分：** 第2次監査で、ホストAPIの存在と確認したUI経路の不足を報告。

**確認内容：** 自由文ポリシーの保存、ListPolicyTexts／UpdatePolicyTexts、Decisionモデルへの注入は存在する。一方、確認したdesktopの権限画面は構造化ルール中心で、自由文ポリシーを編集・保存する経路が見つかっていない。

**利用者・運用への影響：** 設計で想定した「この種類の操作は確認して」等の方針を、通常の利用者がアプリから設定できない。バックエンドにある機能を利用者が使えない。

**最小の修正方針：** 既存RPCを使用して、自由文ポリシーの追加・編集・削除・保存結果を設定画面に接続する。自然文ルールと機械的な強制ルールの保証の違いを表示する。

**完了条件：** UIで保存した内容を再接続後にも取得でき、Decision要求へ最新版が渡る。保存失敗や競合を成功表示しない。必須確認の強制はB20の構造化ルール側で維持する。

**関連ID：** B20 / R04 / R05

**根拠：** [SB03](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-rpc/proto/host.proto) [SB04](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/ui/device.rs) [SB01](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/permission/mod.rs)

### F17 [P2] Character拡張を製品アプリから実行する接続

**確認版：** B　**確認区分：** 第2次監査でSDK・ランタイムと製品側ローダーの差を報告。

**確認内容：** SDKにはActivated／Deactivated、会話開始・終了、TaskChanged、Timer、Custom等のCharacterEventと、Speak／Expression／Motionの戻り値がある。しかしproviderローダーはproviderまたはharnessを持たないパッケージを拒否し、Character専用拡張に製品側のイベント配送・実行経路がない。

**利用者・運用への影響：** 振る舞いプラグインを作れても、eneで会話や作業に反応するキャラクター拡張として利用できない。サンプルや契約テストの存在を製品対応と見なせない。

**最小の修正方針：** Character拡張をロードする所有者を決め、コンパニオンごとの状態・イベント入力・行動出力を接続する。Speakは親の対話窓口と音声制御を迂回せず、身体動作も許可された実行経路を使う。

**完了条件：** 小さなCharacter拡張が会話終了イベントを受け、一回の表情・モーションを対象の一体に返せる。無効化後は動かず、二体間で状態が混ざらない。プラグインが直接ユーザー承認を偽装できない。

**関連ID：** F11 / F18 / F19 / F21 / R03 / R12

**根拠：** [SB05](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/character.rs) [SB06](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/wire.rs) [SB07](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/mod.rs)

### F18 [P2] 設計されたライフサイクルフックの登録・配送

**確認版：** B　**確認区分：** 第2次監査で設計に対する公開契約と実呼出し経路の不足を報告。

**確認内容：** plugins.mdにはon_perception、before_tool_exec、after_tool_exec、before_speak、after_speakがあるが、現在の操作・イベント契約にはその登録・購読がなく、各処理からプラグインへ配送する経路も確認されていない。

**利用者・運用への影響：** 知覚のマスキング、許可後の実行監査、発話加工等を、設計どおりの拡張として追加できない。型だけを増やしても利用経路は完成しない。

**最小の修正方針：** 必要なフックだけを型付きで追加し、実行順・期限・失敗時の扱い・変更可否を定義する。before_tool_execで承認済み引数を改変させず、before_speakは読み上げ投影だけを変更する。after_speakを端末での再生完了と誤解しない。

**完了条件：** 外部送信前の知覚フィルターが実際に適用される。監査フックは引数・許可を変更できない。発話加工後も会話正本が変わらない。停止・失敗・通常終了の通知順が一意になる。

**関連ID：** B03 / B12 / F17 / F19 / F21 / R04 / R11

**根拠：** [SB08](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md) [SB06](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/wire.rs) [SB09](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs)

### F19 [P2] プラグイン向け設定・状態・ツール等のホストサービス

**確認版：** B　**確認区分：** 第2次監査でサービス要求型と実サービスの差を報告。

**確認内容：** SDKにはConfiguration、StateRead／StateWrite、Tool、Log等の要求型がある。provider側BrokerServicesは主にHTTPのopen/read/closeだけを扱い、harness側はDenyServicesを使用する。宣言済みであるだけでは各サービスは実装されない。

**利用者・運用への影響：** 拡張ごとの設定保持・永続状態・既存ツール利用を想定したSDKを書いても、製品上では拒否される。拡張作者が利用可能範囲を誤認しやすい。

**最小の修正方針：** 必要なサービスだけを実装し、plugin ID・entrypoint・companion等の所有範囲と上限を固定する。ツール呼出しを共通認可へ接続する。未提供サービスはmanifest検証と導入画面で明示する。

**完了条件：** 二つの拡張が互いの状態を読めず、再起動後も自分の許可された状態だけ復元できる。未宣言要求は拒否され、Toolサービスから権限判定を迂回できない。ログや設定に秘密情報を漏らさない。

**関連ID：** B07 / F17 / F18 / F20 / F23 / R06

**根拠：** [SB06](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/wire.rs) [SB10](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/services.rs) [SB11](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/work/hand.rs) [SB12](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/invocation.rs)

### F20 [P2] 汎用Wasmツールを公開・登録する拡張契約

**確認版：** B　**確認区分：** 第2次監査でExtensionと公開操作の対応範囲を確認。

**確認内容：** ExtensionにはLlm／Embedding／Tts／Stt／Character／Harnessがあるが、独立した汎用ツールの提供種別と登録経路がない。ServiceRequest::Toolは既存ツールを呼ぶ側であり、提供側の契約ではない。

**利用者・運用への影響：** MCPでツールを増やすことはできても、Wasmパッケージを導入して任意の道具を追加するという拡張方式は完成していない。

**最小の修正方針：** Wasmツール提供を採用範囲に残すなら、定義・入力Schema・実行・取消・結果・登録解除の最小契約を追加する。MCP用とは別の承認機構を作らず、共通のツール登録と認可を使う。

**完了条件：** 一つのWasmツールを導入すると手から呼べ、無効化で公開も消える。名前衝突・不正Schema・実行失敗を明示し、deny／confirmがMCP・端末と同じ意味で働く。

**関連ID：** B03 / F12 / F19 / R04 / R16

**根拠：** [SB09](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs) [SB06](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/wire.rs) [SB08](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md)

### F21 [P2] アバター操作・状態をコンパニオンの反応へ結ぶイベント配送

**確認版：** B　**確認区分：** 第2次監査でrendererの通知とdesktopの消費経路の差を報告。

**確認内容：** rendererはクリック・状態・終了等を通知する。desktopの通常処理はfinishedから待機モーションへ戻すことが中心で、応答待ち中のkeep_finishedは終了以外を保持しない。描画対象にも主にクライアント側IDが使われる。

**利用者・運用への影響：** アバターに触れた操作が仕草や会話へ届かず、非同期の描画失敗・停止もUIや公開能力に一貫して反映されない。二体以上の帰属も未完成になる。

**最小の修正方針：** renderer IDとcompanion IDを対応付け、クリック・読込結果・停止・モーション終了を型付きで配送する。要求の応答待ちと通知処理を分離する。会話・発話・作業のイベントも必要な身体制御へ渡す。

**完了条件：** クリックに対象の一体だけが反応する。応答待ち中の状態通知を失わない。描画失敗時には表示と公開能力が追随する。無効化後の古いイベントは適用せず、通常の口パク・待機復帰は維持する。

**関連ID：** B09 / F01 / F02 / F11 / F14 / F17 / F18 / R03 / R12

**根拠：** [SB13](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-avatar/src/desktop/pointer.rs) [SB14](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-avatar/src/character.rs) [SB15](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/avatar.rs) [SB16](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/session.rs)

### F22 [P2] TTS/STTプラグインの継続ストリーミング契約

**確認版：** B　**確認区分：** 第2次監査で音声プラグインのバッファリング境界を確認。

**確認内容：** TTS/STTプラグインとHTTP brokerは接続済み。ただしTTSはread_audioで音声全体をVec<u8>へ集めて返し、STTもPCMから確定文字列を返す形である。文単位の並行合成や完成音声の分割送信とは別の制限である。

**利用者・運用への影響：** エンジンが音声を生成中に再生を始める経路、継続的な部分認識、処理途中の適切な取消・バックプレッシャーを十分に表現できない。

**最小の修正方針：** 逐次PCMイベントとSTTの部分／確定結果を必要な範囲で表現し、期限・取消・世代・最後のイベントを統一する。文単位エンジンは現在の方式をアダプターとして維持する。Realtime対話セッションF09とは区別する。

**完了条件：** 合成完了前に最初の音声が再生経路へ届く。取消後のチャンクは復活しない。部分認識が確定履歴へ重複保存されず、遅い受信側でも無制限にバッファを増やさない。

**関連ID：** B01 / Q01 / Q02 / F08 / F09 / R06 / R07 / R13

**根拠：** [SB17](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/voice/plugin.rs) [SB18](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/audio.rs) [SB19](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/voice-avatar.md)

### F23 [P2] 外部ACP実行ファイルへの実接続アダプター

**確認版：** B　**確認区分：** 第2次監査でmanifest、runtime、harnessの境界を確認。

**確認内容：** manifestにはTarget::Acpと起動commandがある一方、Instance::loadはTarget::Host以外をUnsupportedで拒否する。WasmハーネスのStart／Prompt／Respond／Cancelは存在するが、外部ACPプロセスの起動・通信を仲介する実行経路が欠けている。

**利用者・運用への影響：** ACP関連の型を使えることを、任意のACPエージェント実行ファイルを設定して利用できることと同一視できない。既存Wasmハーネス自体は未実装ではない。

**最小の修正方針：** 採用するプラグイン責務の下に、プロセス起動、初期化、セッション、追加入力、承認、取消、終了のアダプターを実装する。WASIを無条件に開く回避策にしない。未対応targetを対応済みとして提示しない。

**完了条件：** 隔離したテスト用ACPプロセスで初期化から最終報告まで通る。承認を共通経路で解決し、停止・異常終了・再起動時に孤児プロセスや成功の誤報を残さない。

**統合時の補足：** 「ACPはプラグイン側でラップする」という境界を維持することと、プロセス接続が実装されていることは別。どの実行基盤がnative bridgeを所有するかを設計判断として明示する。

**関連ID：** B08 / F19 / R01 / R05

**根拠：** [SB09](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs) [SB20](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/mod.rs) [SB11](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/work/hand.rs) [SB21](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/work.md)

### F24 [P2] 認証付きMCPとサーバー別起動設定の接続管理

**確認版：** B　**確認区分：** 第2次監査でMCP設定からtransportへ渡す内容を確認。

**確認内容：** stdio起動とHTTP接続は存在するが、Desiredが保持するのはcommand／args／endpoint／allowed等が中心。実接続で資格情報を解決してHTTPヘッダーへ反映する経路や、サーバー別の起動環境を管理する導線が不足する。

**利用者・運用への影響：** 外部プロキシ等に認証を任せた接続とは別に、eneの設定だけでは認証付きMCPを一貫して利用・更新・失効させられない。

**最小の修正方針：** まず必要な認証方式とサーバー別設定だけを接続モデルからtransportへ渡す。秘密情報を引数やログへ露出させない。OAuth等は必要な方式に限定して追加し、認証成功をツール操作の包括許可にしない。

**完了条件：** 未認証では拒否されるテストサーバーに設定済み資格情報で接続でき、更新・失効が反映される。二接続のヘッダー・環境が混ざらず、接続後の個別操作は共通認可を受ける。

**関連ID：** B03 / B17 / F13 / R04 / R08

**根拠：** [SB22](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/mcp.rs) [SB23](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/settings.rs) [SB24](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/features/extensions/README.md)

## 06 追加検証が必要な候補

Qは、静的には懸念があるが、現行経路での発生条件・互換性判断・実影響の検証が残る項目である。試験を行い、修正対象または非該当として確定させる。

### Q01 [P2] 古い音声世代が新しい再生を上書きし得る

**確認版：** A　**確認区分：** 追加検証が必要。

**確認内容：** Player は flushed 以下を拒否するが、現在の deck.generation より古いかどうかは判定しない。異なる世代／utterance なら新しいものとして deck を置き換える。

**利用者・運用への影響：** 配送順序が崩れた場合に古い音声が戻る可能性がある。現行ホストの全経路でその順序が発生するかは未検証。

**最小の修正方針：** サーバー epoch、companion、generation、utterance の識別と順序保証を一意にする。

**完了条件：** flush、音声、Finished、再接続を並べ替え、古いチャンクが復活しないことを確認する。

**関連ID：** B01 / F08 / F22 / R07

**根拠：** [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs) [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) [SA22](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/voice.rs)

### Q02 [P2] リサンプラの flush・発話末尾処理が不足する

**確認版：** A　**確認区分：** コード上の不足を確認／聴感への影響は未測定。

**確認内容：** Player の Filter は端数を pending に保持する。Player::flush は filter を reset せず、新しい発話の判定より前に filter.push する。Finished では端数を排出しない。

**利用者・運用への影響：** wire rate と出力機器の rate が異なる場合、前発話の残留状態の混入や末尾の未出力が起き得る。

**最小の修正方針：** ストリーム区間を明示し、キャンセル時は破棄、正常終了時は filter の遅延・端数を正しく排出する。

**完了条件：** 24k→44.1k/48k、端数フレーム、短い発話、キャンセル直後の発話をサンプル列で検証する。

**関連ID：** B01 / F08 / F22 / R07

**根拠：** [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs) [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs)

### Q03 [P2] 埋め込み空間の ID が異なる実体を区別できない

**確認版：** A　**確認区分：** 設計上の改善候補。

**確認内容：** space_id は model 名・次元・前処理版から作る。provider、endpoint、実モデル revision は含まない。

**利用者・運用への影響：** 同じ model alias と次元を持つ別エンジンへ切り替えても既存ベクトルを同じ空間と見なし得る。docs の現行定義にも同じ弱さがあるため、実装だけの逸脱ではない。

**最小の修正方針：** 明示的な embedding space identity を設定または導出し、互換でない変更で backfill する。認証情報そのものを ID へ含めない。

**完了条件：** 同名・同次元で異なるベクトルを返す二接続の切替時に、互換でないベクトルを比較しない。

**関連ID：** B07 / R06 / R10

**根拠：** [SA13](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/embedding.rs) [SA28](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/mind.md)

### Q04 [P2] 非同期 UI 結果の接続世代・設定 revision の帰属

**確認版：** A　**確認区分：** 追加検証が必要。

**確認内容：** Update には接続を確認するモデル一覧や各種非同期結果があるが、共通の host epoch/request generation はない。個別の ID 検証ですべての古い結果を弾けるかは全経路未確認。

**利用者・運用への影響：** 接続変更や再接続の前に開始した要求が、後から新しい画面状態へ反映される可能性を検証する必要がある。

**最小の修正方針：** 必要な非同期結果に host identity、設定 revision、request ID を付ける。すべてを新しい汎用イベントバスへ移行しない。

**完了条件：** 旧接続の遅いレスポンスを新接続成立後に返し、新しい選択・資格情報状態・モデル一覧を上書きしない。

**関連ID：** R02 / R03

**根拠：** [SA16](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/update.rs) [SA30](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/mod.rs) [SA31](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/state.rs)

### Q05 [P2] ストレージ読取失敗を不存在と見なす復元経路

**確認版：** A　**確認区分：** 追加検証が必要。

**確認内容：** agent::reconcile の一部は work_sessions 読取失敗で continue、存在確認失敗を .ok().flatten() で消す。最初の companions 読取を失敗時停止にしている方針と統一されていない。

**利用者・運用への影響：** 障害を「対象がない」と誤認して復元・維持に失敗する経路を確認すべき。実際の SQLite 障害での再現は未実施。

**最小の修正方針：** 正規状態の不存在、利用不能、破損を区別する。復元を続けられる障害と停止すべき障害を明示する。

**完了条件：** 復元中の各 DB 読取で障害を注入し、正しい agent/run を誤って retire しない。

**関連ID：** B05 / B06 / B10 / R14

**根拠：** [SA19](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/agent.rs)

## 07 リファクタリング候補と変更の境界

Rは独立した機能追加一覧ではなく、上の不具合と不足を直すための変更のまとめ方である。型名・所有者名は提案上の呼び方であり、新しい共通フレームワークや独立crateの新設を必須にしない。

### R01 [P1] Session を「接続の寿命」と「実行の寿命」に分ける

**構造改善の提案：** 接続・購読の監督だけを Session に残し、OperationExecutor、AudioSession、PerceptionSession、companion ごとの binding を別の所有者にする。設定一つの変更で全体を再生成しない。

**完了条件：** 購読再作成・設定変更で受付済み操作が意図せず終了しない。明示停止と切断の意味を区別できる。

**関連ID：** B01 / B08 / B09 / B16 / F01

**提案の起点：** 監査A。**対象根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs)

### R02 [P2] Hub から初期設定・接続検証・RPC ワークフローを外す

**構造改善の提案：** Hub はウィンドウと表示状態の調停役に絞る。接続追加、資格情報登録、モデル取得、初期設定などは独立した手続きにする。画面ファイルを増やすだけでなく責務を移す。

**完了条件：** 画面表示を変えずに接続ワークフローを試験でき、古い非同期結果が新しい選択へ適用されない。

**関連ID：** Q04

**提案の起点：** 監査A。**対象根拠：** [SA30](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/mod.rs)

### R03 [P2] 選択中・常駐中・操作対象・音声所有者を別概念にする

**構造改善の提案：** client_id、companion_id、work_id、表示選択、activation を混同しない。画面で編集対象を変える操作を、担当コンパニオンの移動と同一にしない。

**完了条件：** 二端末・二コンパニオン・二つの同名話者で、編集対象・担当端末・実行対象が混ざらない。

**関連ID：** B11 / B15 / B16 / F01 / F02

**提案の起点：** 監査A。**対象根拠：** [SA01](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs)

### R04 [P1] 共通 CandidateAction と型付き target を導入する

**構造改善の提案：** 内部管理ツールと外部副作用を分類し、MCP/端末/検索を同じ policy evaluator に渡す。action と target の正規化は実行境界で行う。

**完了条件：** MCP・端末・検索・プラグイン由来の外部操作に同じdeny／confirmの意味が適用される。

**関連ID：** B02 / B03 / B04 / B19

**提案の起点：** 監査A。**対象根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs)

### R05 [P1] 承認状態遷移を一か所に閉じる

**構造改善の提案：** policy 評価、Decision 呼出し、承認保存、ECS 待機解除を別責務にする。承認の不変部分と現在の配送先を区別し、永続化失敗の意味を統一する。

**完了条件：** DB保存失敗、LLM失敗、重複回答、取消のどれでも承認状態と実行可否が矛盾しない。

**関連ID：** B05 / B06 / B18

**提案の起点：** 監査A。**対象根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs)

### R06 [P1] ProviderInvocationContext を全モダリティで共有する

**構造改善の提案：** connection ID/revision、endpoint/options、credential grants、broker、deadline/cancellation を一度だけ解決する。LLM・embedding・STT・TTS が各自で意味を再発明しない。

**完了条件：** 同一adapterの複数接続でendpoint・options・資格情報を取り違えず、Embeddingにもbrokerが渡る。

**関連ID：** B07 / B11

**提案の起点：** 監査A。**対象根拠：** [SA15](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/services.rs)

### R07 [P1] 音声を識別子と状態遷移で整理する

**構造改善の提案：** 世代、発話、停止、正常終了、再接続を明示する。capture/playback/resampling の責務を分離する。AEC/VAD は既存の確かなキャンセル契約の上に置く。

**完了条件：** 停止後の次発話、古いチャンク、正常終了の端数、再接続の各試験が同じ契約で通る。

**関連ID：** B01 / Q01 / Q02 / F08 / F09

**提案の起点：** 監査A。**対象根拠：** [SA02](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs)

### R08 [P2] MCP の接続管理とツール実行を分離する

**構造改善の提案：** 接続監督に retry/backoff と構成 revision を持たせる。tool registration・call forwarding・authorization はそれぞれ独立させる。

**完了条件：** 接続の再試行、登録解除、認証更新をツール実行と分離して検証できる。

**関連ID：** B03 / B17

**提案の起点：** 監査A。**対象根拠：** [SA06](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mcp.rs)

### R09 [P2] 記憶検索の起動原因と ContextBuilder を分離する

**構造改善の提案：** TurnCause→検索→予算付きセクション構成→最終注入記録の順にする。最後に巨大文字列を切る方式をやめる。

**完了条件：** ターン起動原因に合う記憶を選び、最終要求と注入済み記憶IDが一致する。

**関連ID：** B13 / B14

**提案の起点：** 監査A。**対象根拠：** [SA26](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/recall.rs)

### R10 [P2] 埋め込み transport・索引・backfill を分離する

**構造改善の提案：** SQLite を正本にする設計は維持する。接続解決とベクトル空間 ID と index の更新責務を分け、同じ値や状態を複数の独立した正本にしない。

**完了条件：** 接続変更による空間切替と索引再構築が独立して検証でき、SQLite以外の第二の正本を持たない。

**関連ID：** B07 / Q03

**提案の起点：** 監査A。**対象根拠：** [SA13](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/embedding.rs)

### R11 [P2] 読み上げ整形を RPC クレートから外す

**構造改善の提案：** RPC は通信契約を中心にする。日本語の表読み上げ、表示整形、モデル出力の漏れ対策は presentation/speech 側の独立した投影として持つ。

**完了条件：** 文字回答のURL・コード・表を維持し、読み上げ加工を変えても通信型や履歴保存が変わらない。

**関連ID：** B12

**提案の起点：** 監査A。**対象根拠：** [SA25](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/src/speech.rs)

### R12 [P2] アバター IPC の DTO と JSON Schema の重複を減らす

**構造改善の提案：** renderer Config を desktop の Shown が手写しし、capability schema も文字列生成している。共有可能な型／生成物と契約テストを用意する。GUI から Bevy 全体をリンクする必要はない。

**完了条件：** 共有DTOまたは契約テストで要求・応答・通知が一致し、IPC待機中の通知を落とさない。

**関連ID：** B09 / F02 / F11

**提案の起点：** 監査A。**対象根拠：** [SA17](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/avatar.rs)

### R13 [P2] ホスト voice.rs の接続・変換・発話制御を分離する

**構造改善の提案：** HTTP provider ごとの差、STT/TTS 設定、PCM 変換、speech floor、文単位キューを混ぜない。通常の呼出しは provider-neutral にし、特定 provider の分岐は adapter に閉じる。

**完了条件：** providerの差分を変更しても発話権・世代・文順序の制御を変更する必要がない。

**関連ID：** B11 / F04 / F09

**提案の起点：** 監査A。**対象根拠：** [SA22](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/voice.rs)

### R14 [P2] 欠落・無効・取得失敗を型で区別する

**構造改善の提案：** 権限、ストレージ、資格情報などの正本で .ok()/unwrap_or_default を安易に使わない。UI の表示だけを縮退させる箇所と、実行を止める箇所を区別する。

**完了条件：** 権限・永続化の取得失敗を「空」と扱わず、表示だけの縮退と実行中止を明確に区別できる。

**関連ID：** B05 / B06 / Q05

**提案の起点：** 監査A。**対象根拠：** [SA19](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/agent.rs)

### R15 [P3] proto をサービス／領域で整理する

**構造改善の提案：** 意味を変えず common/companion/mind/chat/settings/work/voice/client 等に整理する候補。パッケージ名・field number・生成型の互換性を維持し、機能修正と混ぜない。単なる分割だけを最優先にしない。

**完了条件：** field number・package・公開生成型の互換性を確認し、分割と機能変更を同じ差分に混ぜない。

**関連ID：** 契約の見通し

**提案の起点：** 監査A。**対象根拠：** [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto)

### R16 [P2] 能力宣言と予約ツール名の正本を統一する

**構造改善の提案：** desktop テストの HOST_TOOLS 手書き一覧は host の BUILTIN_TOOLS とずれる。生成 fixture または契約テストへ寄せ、宣言した schema/preset と実際の executor が一致するようにする。

**完了条件：** 能力宣言の全Schema・予約名・presetが実行器と一致し、削除・無効化後も古い宣言が残らない。

**関連ID：** F11

**提案の起点：** 監査A。**対象根拠：** [SA43](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/capabilities.rs)

### 避ける変更

行数だけを理由に機械的にファイルを分割しない。汎用Manager・汎用イベントバス・巨大traitを先に導入しない。会話正本や状態の第二の所有者を増やさない。アバターとdesktopのバイナリ統合をイベント連携の前提にしない。既存の単一ホストループや会話を窓口にする体験は維持し、差分を不具合修正と機能追加に分ける。

## 08 設計判断・非採用・文書の不一致

以下は実装者が推測で決めてはいけない採用範囲・保証の問題である。未採用の機能をバグに数えず、採用済みの保証を実装側の都合で弱めない。

### D01 OS Sandboxを現行要件へ戻すか

Wasm隔離は実装あり。OSネイティブ隔離は現行設計で非採用であり、未実装バグとして数えない。必要なら保証対象・対応OS・制限する資源を定義し直す。OS隔離のない実行を隔離済みと説明しない。

**関連ID：** B19 / F19 / F23

**根拠：** [SB02](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/permissions.md) [SB20](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/mod.rs)

### D02 内蔵＋追加providerと、対応生成形式の範囲

全provider外出しの初期提案と、標準Rig内蔵を残す後段の採用記録を区別する。互換形式に限定するLLMプラグインを、任意の生成APIやRealtime対応と宣伝しない。内蔵削除・新形式追加は別の方針変更。

**関連ID：** B07 / F09 / F22 / R06

**根拠：** [SB25](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/provider-plugins.md) [SB26](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/model/plugin.rs)

### D03 承認の不変部分と端末移動後の配送

client bindingを不変とする説明と、移動後の端末へ付け替える説明が併存する。移動継続を維持するなら、承認対象の意味と実行直前に再検証する事項を一意にする。世代や端末認証を根拠なく追加しない。

**関連ID：** B06 / B16 / R03 / R05

**根拠：** [SA04](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md)

### D04 軽いWeb検索を頭と手のどちらへ置くか

完成像は軽い検索をキャラクターAgentが行うが、現行プロンプトは外部調査をstart_workへ寄せる。担当の選択は製品判断であり、共通認可と検索接続の整備とは別に決める。

**関連ID：** B04 / F13

**根拠：** [SA10](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/tools/search.md) [SA19](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/agent.rs)

### D05 保存ルールの対象の意味

action／targetの厳密照合と、引数中の任意文字列による一致の説明を統一する。完全一致、対象範囲、自由文ポリシーを別物として扱い、本文が対象認可になる設計を残さない。

**関連ID：** B02 / B20 / F16 / R04

**根拠：** [SA04](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md)

### D06 フックに許す変更とイベントの終了条件

知覚フィルターは外部送信前、承認済みツールのbeforeフックは読取専用、発話加工は正本とは別の投影、と責務を固定する。after_speakは送出完了であり、端末再生完了報告を新たに必須化しない。

**関連ID：** B12 / F18 / F21 / F22

**根拠：** [SB08](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md) [SB19](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/voice-avatar.md)

### D07 パッケージ交換と実行基盤の採用範囲

手動配置からGUI導入へ進めることと、無停止ホットリロード・ストア運営は別。汎用Wasmツールとnative ACPの対応範囲も明示し、未対応targetやServiceを対応済み表示しない。

**関連ID：** F12 / F19 / F20 / F23

**根拠：** [SB08](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md) [SB25](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/provider-plugins.md) [SB09](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs)

### D08 現行リリースの必須範囲と将来候補

featuresは完成像であり実装済み一覧ではない。Web／スマートフォン、Live2D、Realtime等の位置付けをリリース条件で決める。未合意の機能を、現在のdesktopを壊すP1不具合と同列に扱わない。

**関連ID：** F03 / F09 / F10 / F14

**根拠：** [SA49](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/README.md) [SA11](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/PRODUCT.md) [SA34](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md)

### D09 README・コメント・型の不一致の訂正

AlwaysApproveは保存済みであり「保存しない」の古いコメントを信じて二重実装しない。Activationのrevisionコメント、PRODUCTのplatform表記、provider実装の説明を実際の契約へ合わせる。

**関連ID：** B18 / R15 / R16

**根拠：** [SA03](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) [SA23](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto) [SA11](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/PRODUCT.md)

## 09 実装ロードマップと依存関係

最初に固定するのは「どの版を直すか」と「何を完了とするか」である。既存コードを一括で作り直すのではなく、横断的な契約を小さな変更単位で合わせる。以下は作業順の提案であり、期間や工数の見積もりではない。

| 作業 | 主な対象 | 依存 |
| --- | --- | --- |
| W00 監査対象の更新と再現の固定 | 全ID、D01〜D09 | 着手前に実施 |
| W01 共通認可と承認の整合性 | B02〜B06、B18〜B20、F16、R04・R05・R14 | W00 |
| W02 接続と操作の寿命・音声停止・IPC | B01・B08・B09、Q01・Q02、R01・R07・R12 | W00。W01と独立部分は並行可能 |
| W03 コンパニオン・端末・声・表示対象の帰属 | B11・B15・B16、Q04、F01・F02・F04、R02・R03 | W02の所有者整理 |
| W04 providerとMCPの接続文脈 | B07・B17、Q03、F13・F24、R06・R08・R10・R13 | W01。W03と境界を調整 |
| W05 クライアント拡張とキャラクターの反応経路 | F11・F17〜F21、R12・R16 | W01・W02・W03・W04の該当境界 |
| W06 予定・記憶・原文の整合性 | B10・B12〜B14、Q05、R09・R11・R14 | W01・W04の該当部分 |
| W07 音声対話モードと逐次音声 | F07・F08・F22、F09は採用後、R07・R13 | W02・W03・W04 |
| W08 作業拡張・技能・配布と導入 | F03・F05・F06・F12・F23 | W01・W04・W05の該当境界 |
| W09 遠隔運用と身体表現の完成度 | F10・F14・F15、R15 | W03・W05。製品優先度で採用 |

### W00 監査対象の更新と再現の固定

**依存：** 着手前に実施

**変更の範囲：** A/Bと実装対象HEADの差分を取り、指摘を残存・修正済み・仕様変更・要再現へ振り分ける。修正済みを再実装しない。

**対象ID：** 全ID、D01〜D09

**出口条件：** 以後の修正の前提コミットと、最低限の再現テスト／再現手順が確定する。

### W01 共通認可と承認の整合性

**依存：** W00

**変更の範囲：** 対象の型付け、外部操作の共通ゲート、必須確認とLLM補助判定の分離、DB障害、自由文ポリシーのUIを揃える。

**対象ID：** B02〜B06、B18〜B20、F16、R04・R05・R14

**出口条件：** deny／必須confirmが経路に依存せず、保存失敗で実行されない。

### W02 接続と操作の寿命・音声停止・IPC

**依存：** W00。W01と独立部分は並行可能

**変更の範囲：** 停止を無限大世代で表現しない。受付済み操作を購読の寿命から分離し、アバター応答待ちを独立化する。

**対象ID：** B01・B08・B09、Q01・Q02、R01・R07・R12

**出口条件：** 停止後に次の発話が鳴り、設定変更で作業が死なず、renderer無応答でも終了できる。

### W03 コンパニオン・端末・声・表示対象の帰属

**依存：** W02の所有者整理

**変更の範囲：** client IDを安定一意にし、選択・常駐・作業対象・音声所有者を分け、bodyとvoice設定を実行へ結ぶ。

**対象ID：** B11・B15・B16、Q04、F01・F02・F04、R02・R03

**出口条件：** 二端末・二体・同名話者IDでも意図した対象だけが操作される。

### W04 providerとMCPの接続文脈

**依存：** W01。W03と境界を調整

**変更の範囲：** Embeddingへbrokerを渡し、全モダリティの接続設定を一度解決する。MCPの認証・起動設定・再試行と検索接続を整備する。

**対象ID：** B07・B17、Q03、F13・F24、R06・R08・R10・R13

**出口条件：** 二接続の送信先・資格情報が混ざらず、接続失敗で無制限の再起動をしない。

### W05 クライアント拡張とキャラクターの反応経路

**依存：** W01・W02・W03・W04の該当境界

**変更の範囲：** 拡張の最小レジストリ、必要なService、Characterイベント、設計フック、Wasmツール公開、renderer通知の配送を接続する。

**対象ID：** F11・F17〜F21、R12・R16

**出口条件：** 小さな拡張を本体改変なしで追加し、入力から対象の反応まで通し、停止で動作も公開も消せる。

### W06 予定・記憶・原文の整合性

**依存：** W01・W04の該当部分

**変更の範囲：** 予定の到来と受付を区別し、起動原因から想起、予算内構築後の注入記録、原文と読み上げの分離を行う。

**対象ID：** B10・B12〜B14、Q05、R09・R11・R14

**出口条件：** モデル未設定で予定を失わず、提示した記憶だけを記録し、文字の出典やコードを音声都合で失わない。

### W07 音声対話モードと逐次音声

**依存：** W02・W03・W04

**変更の範囲：** PTT、エコー対策を伴う自動割込み、逐次TTS/STTを追加する。Realtimeは別契約として採用段階を明示する。

**対象ID：** F07・F08・F22、F09は採用後、R07・R13

**出口条件：** 自然な停止・入力・再開と逐次音声が成立し、古い音声や途中文字列を再適用しない。

### W08 作業拡張・技能・配布と導入

**依存：** W01・W04・W05の該当境界

**変更の範囲：** 外部ACPの実通信、SKILL索引と更新、キャラクターパッケージ、作成プレビュー、拡張の導入管理を利用者の導線にする。

**対象ID：** F03・F05・F06・F12・F23

**出口条件：** 設定→実行→結果→停止→再起動までの経路が通り、配布物に秘密・個人記憶を入れない。

### W09 遠隔運用と身体表現の完成度

**依存：** W03・W05。製品優先度で採用

**変更の範囲：** 安全な外部接続、視線・ウィンドウ相互作用、クリック透過精度を整備する。proto分割は機能修正と独立に行う。

**対象ID：** F10・F14・F15、R15

**出口条件：** 遠隔入口の拒否・失効が成立し、表示対象・DPI変化で身体動作が破綻しない。

### 一つの修正単位の進め方

対象HEADとIDを固定し、期待する契約を一文で書く。必要な再現・回帰試験を先に定め、実装と該当する仕様を同時に直す。完了時には観測結果、未保証のOS・経路、影響する関連IDを記録する。内部整理だけを先に大量にマージして不具合を持ち越さない。

## 10 受入テストと完了判定

依存ライブラリの仕様を再試験するためではなく、eneが所有する境界・正本・帰属・失敗時の扱いを検証する。既存の適切な試験を再利用し、同じ事実の重複テストを増やさない。

| 試験 | 対象ID | 受入条件 |
| --- | --- | --- |
| T01 人間確認の強制 | B20・F16 | Decisionを常時allowにしても、明示confirmの操作はユーザーの回答まで実行されない。自由文ポリシーはUIから保存・再取得できる。 |
| T02 認可対象と経路の一致 | B02〜B04・B19 | 許可文字列が本文にあるだけでは一致しない。同じ外部作用はMCP・端末・検索・Wasmで同じdeny／confirmを受ける。 |
| T03 承認と保存障害 | B05・B06・B18 | ルール読取、承認保存、常時許可保存に順に障害を入れる。実行可否とAPI・画面の結果が矛盾しない。 |
| T04 音声の停止と終了 | B01・Q01・Q02 | 停止N→遅延N→N+1、ホスト再起動、24k→44.1k/48k、端数フレームを確認し、古い再生や末尾欠落を防ぐ。 |
| T05 設定変更中の作業 | B08 | 長時間シェルとファイル処理中に目・TTS設定・購読を変更する。明示停止以外で処理と結果報告を失わない。 |
| T06 応答しないrenderer | B09 | 応答を返さない偽rendererでも停止・終了・他のセッション処理が進む。遅延応答を次の要求へ混入しない。 |
| T07 providerの二接続分離 | B07・B11・Q03 | 同じadapter・同じmodel/voice名の二接続で、endpoint・options・credential・埋め込み空間を取り違えない。 |
| T08 端末・コンパニオンの帰属 | B15・B16・Q04・F01・F02・F04 | 二端末・二体を起動し、移動・編集・再接続を行う。IDが安定し、勝手に担当を取り戻さず、古いUI結果を適用しない。 |
| T09 予定の保留と復旧 | B10・Q05 | 発火時にモデルを未設定にし、後で修復する。配送待ちを失わず、受付が重複しない。保存境界直後の再起動も確認する。 |
| T10 想起と注入実績 | B13・B14 | 観測・予定の起動原因に合う記憶を選ぶ。巨大プロフィールでも最終要求に残った記憶IDだけを注入済みとする。 |
| T11 文字と音声の別投影 | B12・F18 | URL・コード・表を履歴・画面で維持し、読み上げ用加工だけを変更できる。内部出力の漏れ対策で原文全体を破壊しない。 |
| T12 SDKから製品への接続 | F17〜F20 | Characterイベント、宣言済みService、Wasmツールの登録を小さなテスト拡張で実行する。未実装Serviceは導入時に分かる。 |
| T13 拡張の状態・許可の分離 | F19・F20 | 別プラグインの状態・秘密を読めず、Toolサービス経由でも認可を迂回できない。無効化で公開と実行を止める。 |
| T14 身体イベントの往復 | F11・F14・F21 | クリック→対象特定→反応→描画通知まで通す。応答待ち中の通知も保持し、失敗をUIと能力に反映する。 |
| T15 ACPとMCPの実接続 | B17・F23・F24 | テスト用ACPプロセスと認証必須MCPへ接続し、初期化・承認・追加入力・取消・資格情報更新・終了を検証する。 |
| T16 PTT・割込み・逐次音声 | F07・F08・F22 | 押下区間だけ収録し、自分の再生では誤割込みしない。生成完了前のPCMと部分認識を扱い、取消後は無効化する。 |
| T17 Realtimeセッション | F09 | 採用後に音声入出力・中断・ツール応答・再接続を同じセッションとして試験する。通常STT/TTSの試験とは別にする。 |
| T18 技能・キャラクター配布 | F03・F05・F06 | 技能を選択して段階的に読み込める。配布物を往復してもAPIキー・個人履歴・記憶・育った関係性を含まない。 |
| T19 拡張の導入と交換 | F12 | 正常・非互換・破損パッケージを区別し、失敗しても現行稼働を壊さない。要求権限と再起動要否を表示する。 |
| T20 外部接続と検索先 | F10・F13 | 許可した外部入口だけから接続できる。検索の未設定・無効・denyでは送信せず、選んだ接続の送信先だけを使う。 |
| T21 描画・当たり判定 | F14・F15 | 透過素材、髪・腕の隙間、変形、二体、異なるDPI、ウィンドウ消滅を検証する。性能は測定して合否を決める。 |

### 最小のリリース判定

まずP1の再現経路を閉じる。次に、採用した主要機能について、設定→公開→実行→結果→停止→再接続・再起動を通す。未採用の機能は未対応と表示し、型やメニューだけで利用可能と見せない。負荷・遅延・描画精度は測定した範囲だけを保証する。

既存CIにはWindows/Linuxのworkspace検査、Wasm回帰、LinuxのGPU試験が記載されている。本書は最新CIの成功・失敗を確認したものではない。[SA50](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/.github/workflows/ci.yml)

## 11 以前の監査との対応・重複整理

### 11.1 第1次監査の継承

| 元の項目 | 本書 | 統合の扱い |
| --- | --- | --- |
| B01〜B19 | 同じID | 全19項目を継承。B07はproviderの不足、B09はイベント基盤の不足とも関連付けた。 |
| Q01〜Q05 | 同じID | 全5項目を継承。実行時の未確認を保持し、確定バグへ昇格させていない。 |
| F01〜F15 | 同じID | 全15項目を継承。F11・F12は第2次監査で具体化したが二重計上していない。 |
| R01〜R16 | 同じID | 全16候補を継承し、完了条件と関連機能を付加。 |

### 11.2 第2次監査との対応

| 第2次監査の論点 | 対応ID | 扱い |
| --- | --- | --- |
| LLM確認の実装と適用範囲 | M01、B03・B04 | 未実装とはせず、既存Bと共通認可として統合。 |
| 必須の人間確認の意味 | B20 | 追加の具体的不整合として採番。 |
| 自由文ポリシーの設定 | F16 | バックエンドは存在、利用者導線の不足。 |
| Sandbox | M02、D01、B19 | Wasm隔離とOS非採用を区別。架空の「Sandbox未実装」項目を作らない。 |
| providerハイブリッドと生成形式 | M03、D02 | 採用範囲を記録。標準内蔵を不具合に数えない。 |
| Embeddingプラグイン | B07 | 既存の不具合へ統合し、別のFを追加しない。 |
| Character・フック・Services・Wasmツール | F17〜F20 | 製品への接続・提供契約の不足として追加。 |
| アバターとの反応の往復 | F21、B09、F01・F02 | 同期IPCの不具合と、利用イベントの不足を分離。 |
| 目・体・ブラウザ/IDE拡張 | F11、F14、F17・F18・F21 | 既存F11を基礎に、接続先を明示。 |
| TTS/STTの逐次出力 | F22、F09 | 逐次音声を追加。Realtimeセッションとは別。 |
| 外部ACP実プロセス | F23 | 既存Wasmハーネスは存在すると明記した上で追加。 |
| 認証付きMCP・起動環境 | F24 | 接続管理の不足として追加。操作認可B03とは別。 |
| プラグイン導入・管理 | F12 | 元の項目を拡充。ホットリロードを必須化しない。 |

### 11.3 重複しやすい項目の関係

| 組合せ | 分ける理由 |
| --- | --- |
| B07 / F19 / R06 | 具体的なEmbedding失敗、汎用Serviceの不足、共通呼出し文脈という修正手段。 |
| B09 / F21 / R12 | セッション停止の不具合、反応イベントの未接続、IPC契約の整理。 |
| B03 / F24 / R08 | 操作認可、サーバー認証、接続監督は別責務。 |
| F08 / F22 / F09 | 自然な割込み、逐次STT/TTS、双方向Realtimeセッションは別機能。 |
| F11 / F17 / F18 / F12 | 実行基盤、振る舞い拡張、イベントフック、利用者向け導入管理。 |

## 12 根拠資料・確認範囲・留保

### 12.1 根拠の読み方

各個票の参照番号は、固定コミットにあるファイルへのリンクである。Aは第1次監査、Bは第2次監査を示す。行番号は今回取得していないため付していない。関連シンボルや判定経路は個票に記載した。ファイル全体の全行を確認済みという意味ではない。

### 監査Aの参照先

コミット：`8a9a6bd4be81ec3e84bf3c30698d06f3af107498`

| 参照 | 固定コミット上のファイル |
| --- | --- |
| SA01 | [apps/ene-desktop/src/session.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/session.rs) |
| SA02 | [apps/ene-desktop/src/audio.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/audio.rs) |
| SA03 | [apps/ene-host/src/host/permission/mod.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/permission/mod.rs) |
| SA04 | [docs/design/permissions.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/permissions.md) |
| SA05 | [apps/ene-desktop/src/work_tools.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/work_tools.rs) |
| SA06 | [apps/ene-host/src/host/mcp.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mcp.rs) |
| SA07 | [docs/features/extensions/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/extensions/README.md) |
| SA08 | [apps/ene-host/src/host/web_search.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/web_search.rs) |
| SA09 | [apps/ene-host/src/host/client/mod.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/client/mod.rs) |
| SA10 | [docs/features/tools/search.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/tools/search.md) |
| SA11 | [PRODUCT.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/PRODUCT.md) |
| SA12 | [apps/ene-host/src/host/mind/plugin.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/plugin.rs) |
| SA13 | [apps/ene-host/src/host/mind/embedding.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/embedding.rs) |
| SA14 | [apps/ene-host/src/host/providers/mod.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/mod.rs) |
| SA15 | [apps/ene-host/src/host/providers/services.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/providers/services.rs) |
| SA16 | [apps/ene-desktop/src/update.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/update.rs) |
| SA17 | [apps/ene-desktop/src/avatar.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/avatar.rs) |
| SA18 | [apps/ene-host/src/host/autonomy.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/autonomy.rs) |
| SA19 | [apps/ene-host/src/host/agent.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/agent.rs) |
| SA20 | [docs/features/autonomy/schedule.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/autonomy/schedule.md) |
| SA21 | [apps/ene-desktop/src/ui/companions.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/companions.rs) |
| SA22 | [apps/ene-host/src/host/voice.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/voice.rs) |
| SA23 | [crates/ene-rpc/proto/host.proto](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/proto/host.proto) |
| SA24 | [apps/ene-host/src/host/chat.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/chat.rs) |
| SA25 | [crates/ene-rpc/src/speech.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/src/speech.rs) |
| SA26 | [apps/ene-host/src/host/mind/recall.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/src/host/mind/recall.rs) |
| SA27 | [docs/features/memory/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/memory/README.md) |
| SA28 | [docs/design/mind.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/mind.md) |
| SA29 | [docs/features/hosting/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/hosting/README.md) |
| SA30 | [apps/ene-desktop/src/ui/mod.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/ui/mod.rs) |
| SA31 | [apps/ene-desktop/src/state.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/state.rs) |
| SA32 | [apps/ene-avatar/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-avatar/README.md) |
| SA33 | [docs/features/voice/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/voice/README.md) |
| SA34 | [docs/features/character/create.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/create.md) |
| SA35 | [apps/ene-host/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-host/README.md) |
| SA36 | [docs/features/task/request.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/task/request.md) |
| SA37 | [apps/ene-desktop/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/README.md) |
| SA38 | [crates/ene-plugin/src/manifest.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-plugin/src/manifest.rs) |
| SA39 | [docs/features/settings/models.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/settings/models.md) |
| SA40 | [crates/ene-rpc/src/client.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/crates/ene-rpc/src/client.rs) |
| SA41 | [apps/ene-desktop/src/host.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/host.rs) |
| SA42 | [docs/design/host-access.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/host-access.md) |
| SA43 | [apps/ene-desktop/src/capabilities.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/src/capabilities.rs) |
| SA44 | [apps/ene-desktop/Cargo.toml](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-desktop/Cargo.toml) |
| SA45 | [docs/design/provider-plugins.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/provider-plugins.md) |
| SA46 | [docs/design/desktop-client.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/design/desktop-client.md) |
| SA47 | [docs/features/character/desktop.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/character/desktop.md) |
| SA48 | [apps/ene-avatar/src/desktop/mod.rs](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/apps/ene-avatar/src/desktop/mod.rs) |
| SA49 | [docs/features/README.md](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/docs/features/README.md) |
| SA50 | [.github/workflows/ci.yml](https://github.com/pexisgle/ene-v2/blob/8a9a6bd4be81ec3e84bf3c30698d06f3af107498/.github/workflows/ci.yml) |

### 監査Bの参照先

コミット：`f28e138aa929e96d92f1a8477f836b10552c4f7f`

| 参照 | 固定コミット上のファイル |
| --- | --- |
| SB01 | [apps/ene-host/src/host/permission/mod.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/permission/mod.rs) |
| SB02 | [docs/design/permissions.md](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/permissions.md) |
| SB03 | [crates/ene-rpc/proto/host.proto](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-rpc/proto/host.proto) |
| SB04 | [apps/ene-desktop/src/ui/device.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/ui/device.rs) |
| SB05 | [crates/ene-plugin/src/api/character.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/character.rs) |
| SB06 | [crates/ene-plugin/src/api/wire.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/wire.rs) |
| SB07 | [apps/ene-host/src/host/providers/mod.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/mod.rs) |
| SB08 | [docs/design/plugins.md](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/plugins.md) |
| SB09 | [crates/ene-plugin/src/manifest.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/manifest.rs) |
| SB10 | [apps/ene-host/src/host/providers/services.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/providers/services.rs) |
| SB11 | [apps/ene-host/src/host/work/hand.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/work/hand.rs) |
| SB12 | [crates/ene-plugin-runtime/src/instance/invocation.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/invocation.rs) |
| SB13 | [apps/ene-avatar/src/desktop/pointer.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-avatar/src/desktop/pointer.rs) |
| SB14 | [apps/ene-avatar/src/character.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-avatar/src/character.rs) |
| SB15 | [apps/ene-desktop/src/avatar.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/avatar.rs) |
| SB16 | [apps/ene-desktop/src/session.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/session.rs) |
| SB17 | [apps/ene-host/src/host/voice/plugin.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/voice/plugin.rs) |
| SB18 | [crates/ene-plugin/src/api/audio.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin/src/api/audio.rs) |
| SB19 | [docs/design/voice-avatar.md](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/voice-avatar.md) |
| SB20 | [crates/ene-plugin-runtime/src/instance/mod.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/crates/ene-plugin-runtime/src/instance/mod.rs) |
| SB21 | [docs/design/work.md](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/work.md) |
| SB22 | [apps/ene-host/src/host/mcp.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/mcp.rs) |
| SB23 | [apps/ene-host/src/host/settings.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/settings.rs) |
| SB24 | [docs/features/extensions/README.md](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/features/extensions/README.md) |
| SB25 | [docs/design/provider-plugins.md](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/docs/design/provider-plugins.md) |
| SB26 | [apps/ene-host/src/host/model/plugin.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/model/plugin.rs) |
| SB27 | [apps/ene-host/src/host/mind/plugin.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-host/src/host/mind/plugin.rs) |
| SB28 | [apps/ene-desktop/src/capabilities.rs](https://github.com/pexisgle/ene-v2/blob/f28e138aa929e96d92f1a8477f836b10552c4f7f/apps/ene-desktop/src/capabilities.rs) |

### 12.2 元資料と外部仕様の扱い

第1次監査の入力は `ene-v2-audit-8a9a6bd.md`、第2次監査の入力は直前の「権限・Sandbox・provider・イベント・拡張」の回答である。今回の統合で、過去の指摘の確認版を新しい版へ書き換えていない。

元監査で参照した外部一次資料には、TokioのJoinSetとselect!、ExtismのHost Functionsがある。これらは元監査の説明根拠であり、今回最新版を再調査したものではない。採用している依存版に対する厳密な再検証は、実装着手時に行う。

[Tokio JoinSet](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html) / [Tokio select!](https://docs.rs/tokio/latest/tokio/macro.select.html) / [Extism Host Functions](https://extism.org/docs/concepts/host-functions/)

### 12.3 このレポートの限界

既存監査で挙げた項目はすべて統合したが、リポジトリ全体の全分岐・全OS・全プラグイン・全テストを網羅した証明ではない。未確認の問題が存在しないとは言わない。実際の修正済み判定には、対象HEADのコード差分、必要な再現試験、および採用した受入条件の結果が必要である。

