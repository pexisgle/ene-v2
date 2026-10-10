# ホストAPI v1

ホストのAPIはConnect RPC v1です。契約の正本は[`host.proto`](../../crates/ene-api-types/proto/host.proto)、接続・認証・通知・端末操作・音声の実装規約は[Connect RPC v1](connect-rpc.md)にまとめています。

## 通信と責務

- 接続先は`/rpc`。`ene.host.v1.HostService`が会話、履歴、設定、許可、資格情報、接続確認、プロバイダー管理を提供します。
- `ene.host.v1.DeviceService`が端末登録、承認、権限、端末操作、表示先の管理を提供します。
- 通知はConnectのサーバーストリームです。購読の先頭でsnapshotを取得し、その後の通知を順番に適用します。
- 音声の双方向通信は`/audio`のWebSocket、Webクライアント向けのログイン・ログアウトは`/session`です。
- Rustクライアントは`ene-api-client`を使います。保存・プラグイン向けの共通型は`ene-api-types`に残し、生成された通信型との変換を境界で行います。

## 更新と失敗

設定更新はrevisionで競合を検出します。再送する操作は同じrequest keyと入力を保持し、受付結果が不明なときに別のキーで再実行しません。更新と生成はホストが所有し、接続の切断だけでは取り消されません。

Connectエラーの詳細には`ene.host.v1.ApiFailure`を使い、アプリケーションのエラーコードとrequest IDを返します。受付後の生成失敗は保存されたMessageの状態と通知で伝えます。秘密情報は応答、ログ、接続URLに含めません。

## 機能拡張

未実装のTask、相談、フォルダ操作、発話調停、記憶、スケジュールなどの要件は[機能拡張方針](api-roadmap.md)へ分離しています。機能を追加する際は、[共通境界](framework-boundaries.md)の認可・保存・取消・再送規則を引き継ぎます。
