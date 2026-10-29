# ene-plugin-sdk

Rust製Extism/Wasmプラグイン用の薄いSDKです。共通型は `ene-plugin` から再公開します。
`service`、`emit` とHTTPヘルパーはWasmビルドでのみ利用できます。
ネイティブビルドでは共通型を使った処理の単体テストができます。

責務は次のように分けています。

| 層 | 責務 |
| --- | --- |
| `ene-plugin` | 共通型、操作・イベントの分類、manifestと通信の契約 |
| `ene-plugin-sdk` | ホスト関数の呼び出し、サービス失敗の伝達、型付きHTTPヘルパー |
| Extism PDK | WasmのABI・メモリ転送・`Json<T>` によるJSON変換 |
| 実行基盤 | 認可、通信・実行、キャンセル、端末やアバターへの配送 |

SDK内部のホスト関数は `Json<ServiceRequest>` / `Json<Event>` を直接受け取り、
応答も `Json<Result<T, Failure>>` に直接読み込みます。SDK独自の文字列変換やcodecは持ちません。
ホスト側の関数名・JSON形式とSDKの公開関数は変更していません。

```rust,ignore
use ene_plugin_sdk::*;

// Operation::Character で受け取った target をそのまま使う。
emit(Event::Character {
    target,
    action: CharacterAction::Expression {
        name: "happy".into(),
        duration_ms: 1200,
    },
})?;
```

受理されたイベントが画面に反映されたことまで保証するものではありません。
配送側の認可と実装が必要です。ホストへの要求応答は `service(ServiceRequest::Character { target, action })`、
端末サービスへの要求は `ServiceRequest::Desktop` で行います。イベント通知とは役割が異なります。

公開関数 `service()` は従来どおり `Value` を返します。型引数・型注釈は不要です。
内部の `service_result<T>()` に共通処理を集約し、HTTPヘルパーでは `HttpChunk` に直接読み込みます。
HTTP応答を汎用JSONツリーへ展開して再変換する処理はありません。
ホストの失敗はコードとメッセージを含むエラーになります。

外部ウィンドウへの購読はmanifestの `window_events` に宣言します。SDKがOSを直接監視することはありません。
[ウィンドウ反応サンプル](../../examples/plugins/window-reaction-wasm/) と
[HTTPを使うOpenRouterサンプル](../../examples/plugins/openrouter-wasm/) を参照してください。
