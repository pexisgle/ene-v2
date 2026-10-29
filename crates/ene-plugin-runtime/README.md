# ene-plugin-runtime

アプリ側のWasm実行ライブラリです。Extism公式Host SDKの `PluginBuilder` / `Plugin::call` を直接使い、同一プロセス内で実行します。`ene-plugin` はプラグイン作者向けの型・SDK、このパッケージは実行側です。

`Package::load` でmanifestと各入口の `file` が参照するコードを検証し、`Instance::load(package, entrypoint, target, generation, cancel)` で読み込みます。`invoke` には操作、アプリが実装する `Services`、出力イベントの送信先、キャンセルトークンを渡します。Host/Desktopの対象は読み込み時に照合します。同期的なWasm実行はTokioのblocking poolに移すため、アプリのイベントループを塞ぎません。

- WASIは無効。Extismの直接HTTPアクセス先を設定せず、通信は認可済みホストサービスに限定します。
- Wasmメモリは64MiB、呼び出しfuel・時間・イベント数・サービス回数・JSONサイズに上限を設定します。
- ホスト関数呼び出し前後の権限確認、manifestでのサービス宣言、返却イベントの拡張種別・対象・終端・動作上限・音声形式を検証します。JSONのバイト数はWasm境界で制限します。
- キャンセル時はExtismのCancelHandleとホスト関数の待機を停止します。呼び出しFutureが破棄された場合も停止を要求します。
- コードは検証時にメモリへ取り込み、呼び出し前にrevisionを照合します。OSサンドボックス用のディスクコピー、Remoc IPC、専用runnerはありません。

WasmのサンドボックスはOSプロセス隔離とは異なります。公開したホスト関数の認可はアプリ側の責務です。ネイティブコンパイラーの処理は途中で強制終了できず、キャンセル後も完了まで実行枠を保持します。ランタイムやネイティブコードの不具合・プロセス全体のメモリ枯渇はアプリへ影響し得ます。

```sh
cargo test --locked -p ene-plugin-runtime
cargo build --release --locked -p ene-openrouter-wasm --target wasm32-unknown-unknown
ENE_TEST_PDK_WASM="$PWD/target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm" cargo test --locked -p ene-plugin-runtime --test pdk -- --ignored
```

設計の基準: [Extism公式FAQ](https://extism.org/docs/questions/)、[Rust Host SDK](https://docs.rs/extism/)。
