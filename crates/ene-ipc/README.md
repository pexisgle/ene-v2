# ene-ipc

Rust製の親子プロセス間IPCの契約型と、Remocを提供します。

- `renderer`: 描画の要求・通知型。
- `transport`: Remocの再公開。
- `rpc`: RemocのRTCマクロ。

描画では `remoc::Connect::io` の `provide`／`consume` で `RendererClient` を一度渡し、
通信タスクの駆動と終了をRemocへ任せます。接続設定とcodecはRemoc標準を使います。
要求とイベント・エラーはremote MPSC、共通設定とIDごとの最新状態は
`RendererState`としてremote watchで送ります。
別チャネル間の到着順には依存せず、状態の正本はwatchのスナップショットです。

描画プロセスは標準入出力の匿名パイプをTokioの `pipe::Receiver`／`pipe::Sender` として
Remocへ渡します。キャンセルできない標準入力の読み取りを避け、`Shutdown`は親側が
接続を保持していても終了します。切断による停止・再起動では親側のチャネルを閉じます。
[TokioのパイプAPI](https://docs.rs/tokio/latest/tokio/net/unix/pipe/struct.Receiver.html)。
通信中のstdoutは専用とし、ログはstderrへ出します。
`examples/worker.rs` は `provide`／`consume` でRTCクライアントを渡し、
RPC結果と進捗を返す子プロセスの例です。

`#[remoc::rtc::remote]` を使うクレートは、生成コードが `::remoc` と `::serde` を
参照するため、RemocとSerdeへの直接依存も宣言してください。

```toml
remoc = { version = "0.20.4", default-features = false, features = ["rch", "rtc"] }
serde = { version = "1", features = ["derive"] }
```

```sh
cargo test --locked -p ene-ipc --all-targets --all-features
cargo clippy --locked -p ene-ipc --all-targets --all-features -- -D warnings
cargo run --locked -p ene-ipc --example worker --features rpc
```

[比較と採用理由](../../docs/research/references.md#internal-ipc)

`provider-plugin`はmanifestと`Provider`／`HttpBroker`のRTC契約を提供します。
プロバイダーは`ProviderServerShared`のclientを`provide`し、ホストは`consume`します。
モデル一覧と生成終了理由はRPCの戻り値、生成テキストとHTTP応答はremote MPSCで渡します。
呼び出しをdropするとRemocがキャンセルし、clientをdropするとサーバーが終了します。
HTTP broker clientはホストが呼び出しごとに発行し、認可と資格情報をホストに保持します。

RTCのcodecはMessagePackです。動的なoptionsやmetadataに必要な自己記述形式を使い、
NDJSONの独自行長制限やBase64変換はありません。接続設定はRemoc標準です。
Pythonプロバイダーは[ネイティブSDK](../ene-python/README.md)経由で同じRTC契約を使います。
