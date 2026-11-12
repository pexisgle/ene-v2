# ene-rpc

`proto/host.proto` を正本とするConnect RPCの契約です。ビルド時にbuffa/Connectの型とサービスを生成し、ホスト、SDK、テストが生成型を直接使います。

## サービス

| サービス | 用途 |
| --- | --- |
| `HostService` | スナップショット、購読、コンパニオンCRUD、心・記憶一覧 |
| `ChatService` | 受理、停止、履歴、メッセージ |
| `SettingsService` | 設定、承認、ポリシー、プロバイダー／モデル、資格情報、接続確認 |
| `WorkService` | 作業セッションの一覧・取得・開始・追加指示・質問回答・停止・クローズ |
| `ClientService` | 端末登録、アクティブ端末、能力、端末操作 |
| `VoiceService` | 音声入出力（契約のみ。ホスト実装は未実装） |

## 境界

- 認証は接続経路のOwner Bearerだけです。`client_id` は接続端末の区別であり、認証ではありません。
- 同じ `request_key` の再送は元の受理結果を返し、内容が違えば競合です。受理は実行完了ではありません。結果は購読・履歴・状態取得で確認します。
- 通知には起動epochと単調なsequenceを付けます。`Subscribe` は `Notification` のストリームを返し、開始時に `initial_state`（Snapshot）、取りこぼし時に `resync_required` を送ります。現在状態は `GetSnapshot` でも取得できます。
- `Snapshot` はコンパニオン・端末・設定・作業セッション・承認・ポリシー・healthを持ち、会話履歴は含めません。履歴は `GetHistory` でページングします。
- 任意JSONはラッパー型ではなくJSON textの `string` で運びます。ホストは境界で1回だけparseし、保存するのはドメイン型です。
- `UpdateCompanionRequest` は `companion` / `task` / `perception` / `compaction` / `consolidation` / `decision` / `embedding` の7 assignment patchと `persona` / `voice_identifier` を持ちます。`optional string` の未指定は変更なし、空文字は `{}` へのクリアです。
- `MemoryControlResult` は `importance`（0〜10）と `audiences`（共有先IDの配列）を返します。
- 旧 `JsonDocument` / `JsonDocumentPatch`、`Grant` / `Consent` / `Availability`、`Target` などは削除済みです。旧クライアントとは互換性がありません。

ネイティブのクライアントは `connect(endpoint, token)` でループバックHTTP/2のBearer設定を作り、生成クライアントへ渡します。Wasmではメッセージ型だけを生成し、Connectのブラウザーruntimeを含めません。
