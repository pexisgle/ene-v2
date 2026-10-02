# OpenRouterのプロバイダープラグイン

`manifest.json`はRust版の`/plugin/provider`を起動します。Rustの通信型とNDJSON処理は`ene-protocol`を共有します。Python版の`provider.py`も同じ契約を実装しています。APIキーとHTTP通信はどちらもホストが扱います。

OpenRouter専用の実装例です。実処理プロバイダーはOpenAI限定、フォールバックは禁止です。生成、逐次テキスト、モデル一覧、取消、終了を扱います。プラグインIDは`openrouter`です。ベースURL変更による他サービスへの接続には対応していません。

manifestの`models`は省略しています。`/models`からID・名前と、コンテキスト上限・出力上限・入出力形式・対応パラメーターを取得します。Rust/Pythonともに取得元と`scope: catalog`を付け、不明な項目は省略します。配信先によって上限が変わるため、OpenAI固定ルートの上限として断定しません。[共通仕様](../../../docs/api.md#モデル一覧とメタデータ)を参照してください。

## Rust版（デフォルト）

[ルートREADMEの手順](../../../README.md#プロバイダープラグイン)でビルドし、実行ファイルとmanifestだけをプロバイダーディレクトリへ配置します。Cargoはビルド時だけ必要です。ソースや`target/`をプラグインフォルダへコピーする必要はありません。

隔離環境から見えるのは`/plugin`とOSの`/usr`・`/lib`・`/lib64`・`/bin`です。静的リンクしたmuslバイナリを推奨します。Nixの動的リンク先（`/nix/store`）は公開されないため、Nixで開発する場合もルートREADMEの`rustup run`で静的版をビルドしてください。他のCPUでは対応するmuslターゲットとリンカーを用意し、ターゲット名・配置元を変更します。OSのライブラリだけで動くバイナリは通常の`cargo build --release --locked -p ene-provider-openrouter`でも利用できます。

開発中の検証は`cargo test --locked -p ene-provider-openrouter`で実行できます。HTTP応答をホスト側から送る形で、外部APIを呼ばずに実プロセスの通信を検証します。

## Python版を選ぶ

Rust版と同じプロバイダーIDを使う代替実装です。隔離環境の`/usr/bin/python3`が必要です。

```sh
provider_dir="${ENE_PROVIDER_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/ene/providers}/openrouter"
install -d -m 700 "$provider_dir"
install -m 600 examples/providers/openrouter/provider.py "$provider_dir/provider.py"
install -m 600 examples/providers/openrouter/manifest.json "$provider_dir/manifest.json"
```

配置した`manifest.json`の`command`を次の配列に変更します。接続先などの設定は共通です。

```json
["/usr/bin/python3", "-I", "-u", "/plugin/provider.py"]
```

配置後はホストの設定画面で再走査し、有効化してください。Rust版とPython版の切り替えも実装変更になるため、送信許可を設定し直します。

旧IDの`openai-compatible`で導入済みの場合は、新しい`openrouter`で接続・モデル割当・送信許可を登録し直してください。
