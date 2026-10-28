# ene-rpc

`proto/host.proto` を正本とし、Connect RPCのメッセージとサービスをビルド時に生成します。
生成型は通常のビルドで常に利用でき、クレート直下と `rpc` モジュールから参照できます。
API・保存・クライアントで同じ型を直接使い、手書きDTOや相互変換を挟みません。

サイズ制限はConnect RPCの標準設定を使います。このクレートでは独自のサイズ計測、
履歴の自動切り詰め、クライアント状態の同期処理を提供しません。
履歴はAPIのページングで取得し、通知の適用・再購読はクライアント側で実装します。

モデル情報・設定項目・Provider能力も、このクレートの生成型を直接使います。
JSON表現もprotobufの形式に統一され、フィールド名はcamelCase、64ビット整数は文字列です。
メッセージフィールドは未指定と空のメッセージを区別し、`JsonDocument` はJSON文字列を保持します。

`UpdateSettingsRequest.connections` は `ConnectionInputs` です。省略は一覧の維持、
`{"connections":{}}` は空一覧への置換、`{"connections":{"items":[...]}}` は指定一覧への
置換を表します。省略だけの要求はrevisionを検証して現在値を返し、保存・revision加算・通知を行いません。
旧repeatedフィールドの番号2は予約済みです。クライアントはwrapperを使う新しい生成型へ更新してください。旧番号のprotobuf要求はホストが拒否します。

`UpdateCompanionRequest.body` と `voice` は `JsonDocumentPatch` です。省略は維持、
`{"set":{"json":"..."}}` は設定、`{"clear":{}}` は解除を表します。
JSONの`null`は省略と同じで、解除には`clear`を使います。空パッチはホストが拒否します。
設定値は非nullのJSONオブジェクトで、体は`format`・`environment_id`・`model`、
声は`connection_id`・`voice_id`を持ちます。体の所在環境と形式、声の接続参照をホストが検証し、
体のファイル存在と描画可否はクライアントが検証します。

Wasmゲストは同じ `host.proto` のメッセージ型をBuffaの公式ビルダーで生成します。
Connect RPCのブラウザー用runtimeをExtismへ持ち込まないよう、Wasmではサービスを
生成せず、`connectrpc` を実行時依存に含めません。nativeのclient・server APIはそのままです。
