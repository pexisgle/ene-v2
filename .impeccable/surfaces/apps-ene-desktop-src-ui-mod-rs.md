---
version: 1
slug: "apps-ene-desktop-src-ui-mod-rs"
primary_target: "apps/ene-desktop/src/ui/mod.rs"
related_targets: ["apps/ene-desktop/src/ui/pod.rs","apps/ene-desktop/src/ui/studio.rs","apps/ene-desktop/src/ui/theme.rs"]
---

# ene-desktop 表面ブリーフ

- **モード**: Operate
- **対象ターゲット**: `apps/ene-desktop/src/ui/mod.rs`（関連: `pod.rs`, `studio.rs`, `theme.rs`, `icons.rs`, `widgets.rs`, `companions.rs`, `connections.rs`, `device.rs`）
- **ビルド方式**: `code-led`（参考スケッチ: `.impeccable/mocks/decision/ene-r3-milk.html`）
- **対象と範囲**:
  - 常駐する透過フローティング対話UI（`CompanionPod` — 枠なし吹き出し＋送信付きカプセル1行入力バー＋操作メニュー）
  - 必要時のみ開く独立設定ウィンドウ（`SettingsStudio` — `コンパニオン` / `接続` / `この端末` の3章）
- **置かないもの**: 記憶一覧、作業一覧・子エージェント質問カード、常設停止ボタン、アバター動作テスト用ボタン、生のRMS数値、説明文の羅列、左上の製品バッジ、砕けすぎた子供向け文言。

## Direction contract

- **THESIS**: 3Dアバターが主役のデスクトップ体験を守るため、常駐UIを枠なしの丸い吹き出し＋カプセル入力バー＋独立丸ボタンへ崩し、設定だけをいちごミルク×クリーム白の明快な3章OSウィンドウへ分離して、記憶一覧・作業一覧・7役割スロットの常時露出を排する。
- **OWN-WORLD**: クリーム白 `#FFFDFD` 主面、いちごミルク `#FADCE6` タイトルバー・サイドバー・種別タグ地、ソフトブラッシュ `#FFF4F8` 補助面、ストロベリーローズ `#CF2E68` 主操作・アクティブピル（白文字 4.95:1）、ミント `#DDF5EC` / `#16694F` 接続中ピル、ベリーココア `#3B232C` 本文インク（純黒不使用）、`#F09BB8` 2pxフローティング縁線、`#F3C4D5` 1px区切り罫、丸ピル制御と描画SVGアイコン統一。
- **STORY**: 非エンジニアの利用者が初回3ステップ（プロバイダ選択＋キー入力→任意の音声→対話開始）だけで迷わず話し始め、日常は最小カプセルと確認ふきだし（`許可` / `常に許可` / `拒否`）だけで安心して作業まで任せられる。
- **FIRST VIEWPORT**: 画面右下の透過ポップアップ領域に、きらめきSVG付きストロベリーローズの名前ピルを重ねた非対称丸角ふきだし（`24/24/8/24px`＋右下12px丸テール）と、送信付きカプセル型1行入力バー。入力欄の右の丸ボタンから、コンパニオン切替・マイク・音声・履歴・設定・閉じるを開ける。別ウィンドウの設定（`780×580`）は38pxいちごミルク色タイトルバー＋左196pxサイドバー（`コンパニオン`・`接続`・`この端末`）＋右クリーム白ペイン。
- **FORM**: ストロベリーミルク・カプセル（grounded list #3, safer reroll 2） · `seed:f586ad297087ed26`
- **FINISH**: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
