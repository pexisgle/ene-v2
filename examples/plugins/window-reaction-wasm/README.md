# ウィンドウイベントに反応するWasmプラグイン

他アプリのウィンドウイベントを `Operation::Character` で受け取り、対象キャラクターへ
`CharacterAction::Expression` を返す最小例です。ウィンドウタイトルや画面内容は使いません。
OS監視・権限管理・ene-avatarへの配送はまだ実装していません。
テストは模擬ホストからイベントを渡し、実際のWasmとSDKを通る動作指示を検証します。

## 宣言と呼び出し

`manifest.json` の `window_events` は `Code.exe` の `focused` のみを要求します。
`Code.exe` はサンプルの識別子です。実接続では端末のアダプターが提示するアプリ識別子を
manifestとサンプルの判定に設定してください。文字列は完全一致で、空配列は全件ではなく購読なしです。

配送側は許可されたバインディングから `target` を設定します。

```json
{
  "type": "character",
  "target": {"device_id": "desktop-a", "character_id": "ene-2"},
  "event": {
    "type": "window",
    "event": {
      "sequence": 7,
      "kind": "focused",
      "window": {
        "id": {"device_id": "desktop-a", "generation": 3, "id": "window-42"},
        "application": "Code.exe"
      }
    }
  }
}
```

返すイベントは次のとおりです。

```json
{
  "type": "character",
  "target": {"device_id": "desktop-a", "character_id": "ene-2"},
  "action": {"type": "expression", "name": "happy", "duration_ms": 1200}
}
```

プラグインは呼び出し対象をそのまま返します。別アプリ・別端末・別種別のイベントは無視します。
ただし、この判定を認可の代わりにはできません。配送側も購読・権限・対象・接続世代を検証する必要があります。
`happy` はプラグイン契約の表情名です。現在のene-avatarには表情設定コマンドがないので、表示への反映は次段階です。

## ビルドとテスト

リポジトリ直下で実行します。WasmターゲットとNode.jsが必要です。

```sh
cargo build --release --locked -p ene-window-reaction-wasm --target wasm32-unknown-unknown
npm install --no-save --package-lock=false @extism/extism@2.0.0-rc13
node examples/plugins/window-reaction-wasm/test.cjs target/wasm32-unknown-unknown/release/ene_window_reaction_wasm.wasm
```

パッケージ化時は生成されたWasmをmanifestと同じディレクトリに `reaction.wasm` として置きます。
API仕様と実行基盤側の責務は [ene-plugin](../../../crates/ene-plugin/README.md) を参照してください。
