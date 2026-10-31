# ene-plugin-runtime

アプリ側のWasm実行ライブラリです。Extism公式Host SDKの `PluginBuilder` / `Plugin::call` を直接使い、同一プロセス内で実行します。`ene-plugin` はプラグイン作者向けの型・SDK、このパッケージは実行側です。

`Package::load` でmanifestと各入口の `file` が参照するコードを検証し、`Instance::load(package, entrypoint, target, generation, cancel)` で読み込みます。`invoke` には操作、アプリが実装する `Services`、出力イベントの送信先、キャンセルトークンを渡します。Host/Desktopの対象は読み込み時に照合します。同期的なWasm実行はTokioのblocking poolに移すため、アプリのイベントループを塞ぎません。

Providerはpackage/entrypoint、Speechはbinding単位でInstanceを再利用し、再利用前に `ene_initialize` を再実行します。guestの静的状態にリクエスト固有のデータを残してはいけません。

- WASIは無効。Extismの直接HTTPアクセス先を設定せず、通信は認可済みホストサービスに限定します。Extismの組み込みvarは `max_var_bytes = 0` で無効化し、config/log系の組み込みenvは残ります。
- Wasmの線形メモリは64MiB、tableは1モジュール1個・20,000要素までです。Extismの設定フックからWasmtimeのpooling allocatorを使い、初期割当と `table.grow` の両方を制限します。呼び出しfuel・時間・イベント数・サービス回数・JSONサイズにも上限を設定します。
- ホスト関数呼び出し前後の権限確認、manifestでのサービス宣言、操作ごとの返却イベント・対象・終端・動作上限・音声形式を検証します。要求は1MiB、個々のイベント・応答は128KiB、1呼び出しのサービス入出力は16MiB、イベント総量は64MiBまでです。HTTP本文の上限はホストが別途検証します。
- キャンセル時はExtismのCancelHandleとホスト関数の待機を停止します。呼び出しFutureが破棄された場合も停止を要求します。
- コードは検証時にメモリへ取り込み、読込時にrevisionを照合します。実行中はそのスナップショットを使い、各チャンクでは権限・世代を確認します。ディスクの変更はreload、list/descriptor、enable、bindで検出します。Unixではmanifest/code/assetのサイズ・mtime・inode・ctimeが変わったときだけ再検証し、Windowsでは同じ保証がないため毎回内容を再検証します。
- packageのassetは実バイトのSHA-256を検証し、revisionにも含めます。providerの生成ごとのインスタンス生成は残りますが、Extismの既定のWasmtimeコンパイルキャッシュでネイティブコンパイルを再利用します。pooling allocatorのengine単位上限があるため `CompiledPlugin` 自体は共有しません。

WasmのサンドボックスはOSプロセス隔離とは異なります。公開したホスト関数の認可はアプリ側の責務です。ネイティブコンパイラーの処理は途中で強制終了できず、キャンセル後も完了まで実行枠を保持します。ランタイムやネイティブコードの不具合・プロセス全体のメモリ枯渇はアプリへ影響し得ます。

```sh
cargo test --locked -p ene-plugin-runtime
cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown
cargo test --locked -p ene-host infrastructure::plugins
```

設計の基準: [Extism公式FAQ](https://extism.org/docs/questions/)、[Rust Host SDK](https://docs.rs/extism/)。
