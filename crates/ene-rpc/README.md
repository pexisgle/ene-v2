# ene-rpc

`proto/host.proto` を正本とするConnect RPCの契約です。ビルド時にbuffa/Connectの型とサービスを生成し、ホスト、SDK、テストが生成型を直接使います。

## サービス

| サービス | 用途 |
| --- | --- |
| `HostService` | スナップショット、購読、コンパニオン、心・記憶 |
| `ChatService` | 受理、停止、履歴、メッセージ |
| `SettingsService` | 接続、モデル、資格情報、送信許可、可用性 |
| `WorkService` | 作業セッションの一覧・取得・追加指示・停止 |
| `ClientService` | 端末登録、アクティブ端末、能力、端末操作 |

## 境界

- 認証は接続経路のOwner Bearerだけです。`client_id` は接続端末の区別であり、認証ではありません。
- 同じ `request_key` の再送は元の受理結果を返し、内容が違えば競合です。受理は実行完了ではありません。結果は購読・履歴・状態取得で確認します。
- 通知には起動epochと単調なsequenceを付けます。取りこぼし・遅延時は再購読し、スナップショットから再同期します。
- `Snapshot` はコンパニオン・端末・設定・許可・作業概要・healthを持ち、会話履歴は含めません。履歴は `GetHistory` でページングします。
- 端末操作は `ClientService.DeviceStream` で受け取り、`CompleteOperation` で結果を返します。ホストは成否不明の操作を再送しません。
- プロバイダーのモデル記述型（`Model` / `ModelMetadata` / `ProviderCapabilities` / `SettingField`）はプラグインSDKとの共有のためクレート直下にも再exportします。

ネイティブのクライアントは `connect(endpoint, token)` でループバックHTTP/2のBearer設定を作り、生成クライアントへ渡します。Wasmではメッセージ型だけを生成し、Connectのブラウザーruntimeを含めません。
