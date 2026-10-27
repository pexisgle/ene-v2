# ene-plugin

プラグインシステムの共通契約を提供します。manifest、拡張API、ホストとworker間の
通信契約をここにまとめ、runtime・runner・SDKから同じ型を使います。

定義は内部の `manifest.rs`、`api.rs`、`protocol.rs` にまとめ、`lib.rs` から再公開します。
パッケージmanifest、拡張の要求・イベント、ホストサービス、`HostMessage`／`WorkerMessage` は
従来どおりクレート直下から利用できます。`provider` モジュールは認証・ルーティング設定、
チャットメッセージと共通の生成型を提供します。生成要求はクレート直下の `GenerationRequest` を使います。

このクレートは共通契約型を提供します。実際の通信処理とRemocへの依存は
プラグインruntime側にまとめ、接続・codec選択・チャネル・送受信を実装します。
メッセージサイズ、HTTP・音声のチャンクサイズ、イベント数・キュー容量などの制限も
runtime側で定義・適用し、runnerは同じ設定を使います。
Rust製runnerもruntime側の通信処理を利用し、SDKは共通契約型を利用します。
Luau／Wasmのプラグイン本体は実行器のAPIを使います。Luauの実行はmlua、WasmのABIはExtismに任せます。
manifestのruntimeは `luau` と `extism` の2種類です。このクレートは実行器を実装しません。

モデル情報・設定項目・Provider能力は [`ene-rpc`](../ene-rpc/) の生成型を直接使います。
この依存は `host.proto` の共通データ型を共有するためのものです。プラグインruntimeとSDKは
モデル情報などをそのまま使い、プラグイン通信でConnect RPCのクライアントやサービスを呼び出しません。
共通型の定義は `host.proto` に一元管理し、別クレートへの切り出しや重複DTO・変換処理を設けません。
manifestに埋め込むモデル情報もprotobuf JSON形式です。メタデータのフィールド名は
camelCase、64ビット整数は文字列、任意のリストは `{"values": [...]}` のメッセージで表します。

manifestはプラグインシステムの読み込み時にSerdeで契約型へデシリアライズします。
バージョン、識別子、entrypointの整合性、ファイル参照などの検証はプラグインシステムが担当します。
検証用のJSON Schemaは生成・出力しません。

manifest、拡張API、runner通信の互換性は `CONTRACT_VERSION` の1つで管理します。
`Manifest`、`HostMessage::Initialize`、`WorkerMessage::Ready` は共通の `contract_version` を使い、
runtimeはmanifestの読み込み時とrunner接続時に対応する値との一致を検証します。
互換性を壊す契約変更ではこの番号を更新します。`Manifest.version` はプラグイン自体の
リリース番号として別に管理します。

runnerの制御は `HostMessage`、イベント・サービス要求・完了通知は `WorkerMessage` で渡します。
HTTPは `ServiceRequest` の `HttpOpen`／`HttpRead`／`HttpClose` を使い、認可と資格情報は
ホストが管理します。キャンセルと終了は `HostMessage::Cancel`／`Shutdown` で要求します。

manifestの読み込み・検証、権限の判定、Luau／Wasmの実行、runner通信、キャンセル・終了の
実際の振る舞いはプラグインruntime側でテストします。この共通契約クレートでは、
SerdeやRemocの動作を再確認する単体テストを持ちません。

```sh
cargo check --locked -p ene-plugin --all-targets --all-features
cargo clippy --locked -p ene-plugin --all-targets --all-features -- -D warnings
```
