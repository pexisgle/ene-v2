# 参照Wasmプラグイン

状態保存付きキャラクター動作、ACP v1準拠の権限相談付きハーネス、音声プロトコルの結合テスト用です。本番の音声認識・合成や自律タスクエンジンではありません。旧Luauの3実装を、同じ操作・イベント契約を持つRust製Extismプラグインへ移しました。

```sh
cargo build --release --locked -p ene-reference-wasm --target wasm32-unknown-unknown
```

`target/wasm32-unknown-unknown/release/ene_reference_wasm.wasm` を `reference.wasm` としてmanifestの隣に置くと読み込めます。ホストのテストは生成されたWasmを一時パッケージへコピーします。別の出力先を使う場合は `ENE_TEST_REFERENCE_WASM` で指定します。`just test` はこのWasmを先にビルドします。
