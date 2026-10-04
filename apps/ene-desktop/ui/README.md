# Slint UIの配置

`app.slint`はビルド入口とRust向けの公開型の再exportを担当します。ウィンドウ全体の構成は`shell/`、テーマとアイコンは`foundation/`、画面に渡す型と状態は`state/`、再利用する表示・入力部品は`widgets/`へ置きます。

画面固有の部品は`pages/`の対象画面にまとめます。`pages/conversation/`は会話表示・入力・メッセージ、`pages/settings/`は外観設定を担当します。共通部品から画面への依存を増やさず、Rustとの境界は`app.slint`に維持します。

`pages/settings/view.slint`はカテゴリの切替と公開メソッドの転送を担当し、外観設定は`appearance.slint`と`appearance/`の文字表示・アバター表示・モデルとモーション・再生操作へ分けます。編集ページの共通テンプレートは`widgets/settings/`です。`state/ui-state.slint`はRustが管理する状態とコールバックの投影であり、各ページにコントローラーを分散させません。
