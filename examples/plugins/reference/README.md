# 参照 Wasm プラグイン

キャラクター動作、ACP v1 の権限相談、音声契約を実演するプラグインです。

```sh
cargo build --release --locked -p ene-plugin --example reference --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/examples/reference.wasm examples/plugins/reference/reference.wasm
```

設定画面のプラグイン追加で、このフォルダーを指定します。

端末拡張は `ene-desktop --plugin examples/plugins/reference` で読み込みます。「この端末の見守り」が有効になると、前面アプリを端末内で解釈します。体の拡張をホストから使う場合は、ホストの `ENE_HOST_DEVICE_CAPABILITIES` に `body.plugin.reference.desktop` を追加します。既定の能力も使う場合は `body.motion,body.expression,desktop.input` を同じ一覧に含めます。
