# OpenRouterのプロバイダープラグイン

`manifest.json`はRust製の`/plugin/provider`を起動します。通信型とNDJSON処理は`ene-protocol`を共有します。APIキーとHTTP通信はホストが扱います。

OpenRouter専用のプラグインです。実処理プロバイダーはOpenAI限定、フォールバックは禁止です。生成、逐次テキスト、モデル一覧、取消、終了を扱います。プラグインIDは`openrouter`です。ベースURL変更による他サービスへの接続には対応していません。

manifestの`models`は省略しています。`/models?providers=openai`でOpenAI配信に絞った応答を共通の`Model`へ正規化し、コンテキスト上限・出力上限・入出力形式・対応パラメーターを返します。`context_window`は取得できなければ`null`です。取得元と`scope: catalog`を付け、配信先によって上限が変わるためOpenAI固定ルートの上限とは断定しません。このプラグインは外部カタログを使いません。別の情報源で補完する場合もプラグインが取得・正規化を担当し、ホストにカタログ設定は追加しません。公開情報は、宣言・許可済みの別originへ一覧取得中だけ資格情報なしのGETを送れます。[共通仕様](../../docs/design/api.md#モデル一覧とメタデータ)を参照してください。

## ビルドと配置

[ルートREADMEの手順](../../README.md#プロバイダープラグイン)でビルドし、実行ファイルとmanifestだけをプロバイダーディレクトリへ配置します。Cargoはビルド時だけ必要です。ソースや`target/`をプラグインフォルダへコピーする必要はありません。

隔離環境から見えるのは`/plugin`とOSの`/usr`・`/lib`・`/lib64`・`/bin`です。静的リンクしたmuslバイナリを推奨します。Nixの動的リンク先（`/nix/store`）は公開されないため、Nixで開発する場合もルートREADMEの`rustup run`で静的版をビルドしてください。他のCPUでは対応するmuslターゲットとリンカーを用意し、ターゲット名・配置元を変更します。OSのライブラリだけで動くバイナリは通常の`cargo build --release --locked -p ene-provider-openrouter`でも利用できます。

開発中のコンパイル確認は`cargo check --locked -p ene-provider-openrouter`で実行できます。ホストのプラグイン通信の検証手順は[ルートREADME](../../README.md#nixを使わずに開発する)を参照してください。

配置後はホストの設定画面で再走査し、有効化してください。実装を更新した場合も送信許可を設定し直します。

旧IDの`openai-compatible`で導入済みの場合は、新しい`openrouter`で接続・モデル割当・送信許可を登録し直してください。
