# Connect RPC v1の契約

eneの主APIはConnect RPC、双方向の音声通信はWebSocketとする。契約は[`host.proto`](../../crates/ene-rpc/proto/host.proto)に集約した。Rustの型・サービス・クライアントはbuffaとConnect Rustで生成する。AxumはHTTP API、認証、WebSocketの入口を担当する。Webクライアントは将来実装する予定で、現在は配信しない。

この資料は、ホストやクライアントを実装する開発者向けである。導入手順と通信ごとの契約を記す。機能拡張の要件は[APIの機能拡張方針](api-roadmap.md)に分離する。

## ホストへ接続する

`just run-host`が公開する`connection.json`のendpointは`http://127.0.0.1:<port>/rpc`になる。デスクトップは、このendpointと接続トークンから生成されたConnectクライアントを使う。サービスのpackageは`ene.host.v1`。クライアントは`/rpc`だけを受け付ける。

Rustは`ene-rpc`のCargoビルドで常に自動生成する。protocも依存に同梱しているため、別途インストールする必要はない。バージョンはConnect Rust 0.9.1、生成器0.9.0、buffa 0.9.2で固定した。

将来のWebクライアント用に、`/session`のHttpOnly・SameSite=StrictセッションCookie認証と同一Origin検証をホスト側に保持する。セッションは8時間で失効し、ログアウト時には進行中の通知・音声購読も終了する。ホストは起動時にOriginを固定し、既定値は公開したループバックURLとなる。公開Originは`ENE_WEB_ORIGIN=https://host.example`で指定できる。Originはパス・末尾のスラッシュを含めない。ホスト自身の待受はループバックのままであり、リモート公開時のTLS・プロキシ設定は別途必要になる。

## 通常の操作と通知は生成したRPCを使う

`/rpc/ene.host.v1.HostService/<Method>`に、会話、履歴、設定、許可、資格情報、接続確認、プロバイダー管理の操作を登録する。Rustでは`ene-rpc`の`HostServiceClient`から直接呼ぶ。desktopの`SyncedSession`は画面状態の同期を管理する。

更新時のrevisionと、再送時のrequest keyはクライアントが保持する。同じキーを異なる入力へ使うと競合になる。SQLite receiptで同じ操作の二重受付を防ぐ。保存済みreceiptの操作識別キーは通信経路から独立した保存契約として維持し、APIの改名で再送判定を失わない。Connectエラーには生成型`ApiFailure`を詳細として付け、アプリケーションのエラーコードとrequest IDを引き継ぐ。

`Subscribe`の先頭は`InitialState`で、subscription IDとsnapshotをまとめて返す。購読登録後に既存のコミット境界でsnapshotを取得し、そのcursor以下の重複を除く。その後は`Notification`のoneofで、メッセージ、Unicodeスカラー位置による返答差分、設定、許可、保存失敗を区別する。通知の欠落や未対応の通知を黙って無視しない。

ホスト側の通知リングは256件。遅い受信側で欠落した場合は購読を無効化して`ResyncRequired`を送り、接続を終了する。同期・キュー管理はdesktopが所有し、Connectの送受信制限はライブラリの既定値を使う。新しい購読は新しいsnapshotから始まる。

受け付けた更新と生成はホストが所有する。通知を閉じたりHTTPの待ち手を破棄したりしても、操作の取消にはならない。明示的な`Stop`は通常更新とは別の受付枠で生成へ取消を通知する。返答の終了状態をDBへ保存する処理は後続で完了させるため、Stopの応答だけを保存完了と解釈しない。

## 設定更新の省略と解除

`UpdateSettingsRequest.connections`は任意の`ConnectionInputs`です。省略（JSONの`null`を含む）は接続・資格情報・許可を維持し、保存・revision加算・通知を行いません。`{"connections":{"items":[]}}`は明示的な全削除、itemsがある場合は一覧の置換です。コンパニオン・声・有効な許可・実行中の操作から参照される接続は削除できません。省略時も現在のrevisionが必要です。

旧repeatedフィールドの番号2は予約済みで、wrapperは番号3を使います。新しい生成型へクライアントを更新してください。旧番号2を送るprotobuf要求は`invalid_request`で拒否します。

`UpdateCompanionRequest.body`と`voice`は`JsonDocumentPatch`です。省略（JSONの`null`を含む）は維持、`{"clear":{}}`は解除、`{"set":{"json":"..."}}`はJSON文書の設定です。空のpatchは不正です。

bodyの文書は`{"format":"vrm1","environment_id":"<snapshotのenvironment_id>","model":"<VRMファイルの参照>"}`、voiceは`{"connection_id":"<既存のtts接続ID>","voice_id":"<声ID>"}`を使います。ホストは文書の構造・対応形式・環境・接続種別を検証します。モデルの実ファイルと描画の可否はクライアント側で検証します。デスクトップの保存画面はファイルの存在と拡張子を確認し、実際のVRM読込は描画側が担当します。ホストに保存したモデル参照と端末の表示モデル設定は現時点では独立しています。音声エンジンの接続状況は音声節のとおりです。

name・mind・body・voice・モデル割当を含む一回のコンパニオン更新は、全項目を検証してから一括保存します。一項目でも不正ならrevision・保存データ・通知を変更しません。デスクトップは未編集の文書patchを省略し、通知が届いても編集中の表示値とrevisionを維持します。

## 端末内の操作は承認・許可・操作IDで管理する

`DeviceService`は端末登録、承認、失効、能力別の許可、操作要求と結果送信を提供する。登録時だけ専用トークンを返し、SQLiteへはSHA-256ハッシュを保存する。承認しても操作権限は増えない。承認済み端末はsnapshot・履歴・通知を取得し、会話と停止を操作できる。端末操作の購読・結果送信・表示権・音声準備は自分の端末に限り、ホスト全体の設定・許可・資格情報の管理APIは拒否する。端末の失効・権限変更で購読を終了し、再接続時に認証し直す。

端末は自分から`Subscribe`へ接続する。先頭でsubscription IDを受け取り、その後に操作を受け取る。結果には操作ID、端末ID、subscription IDをすべて付ける。同じ結果の再送は保存済み結果を返す。入力の異なる同一操作IDや、旧購読からの結果は拒否する。

要求はSQLiteへ保存してから有限のキューへ渡す。キューは端末ごとに32件、操作の入力・結果は各64KiB、履歴は2048件かつ16MiB、端末数は64件まで。上限では受付を拒否する。切断・再起動後の未完了操作は`Uncertain`、許可取消後は`Revoked`になる。これらを再接続時に再配信しない。成否不明の操作を実行し直すかどうかは、呼び出し側の明示的な判断が必要になる。

表示権は15秒のleaseで、一つのコンパニオンに一端末だけを認める。別端末へ移すには、現在の端末が表示を止めて返却するか、leaseの失効を待つ。ホスト再起動後も15秒待ち、旧ホストのleaseとの重複を防ぐ。表示側は要求送信時刻から期限を計算し、1秒の余裕を差し引く。5秒ごとに更新し、失敗時・期限切れ時は表示を止める。表示を止めた後にだけ返却RPCを送る。

このAPIで任意の端末内ツールが実装済みになるわけではない。端末は実装した能力だけを登録し、実行中の処理も権限取消・切断で止める必要がある。既存デスクトップのアバター表示を複数端末へ移すUIは、今回の通信移行には含まない。

## 音声は同じ.protoのフレームをWebSocketへ載せる

`PrepareAudio`でコーデックと端末権限を確認し、session IDと取消世代を発行する。準備は30秒で失効し、一度しか使えない。最大128件まで保持し、ホスト再起動や権限変更後の旧Startも拒否する。その後に`/audio`へ接続し、`AudioStart`を送る。WebSocketの一つのバイナリメッセージが一つの`AudioFrame`になる。Start、Chunk、End、Failureはoneofで区別する。

現在の形式はmonoの`pcm_s16le`、16/24/48kHz。chunkは16KiBまでで、上下それぞれ0から増える連番を付ける。上りと下りのキューは各8件。入力のEndはマイク入力だけを終了し、下りの読み上げは続けて受け取れる。入力キューが満杯の場合も下り音声と取消を処理しながら空きを待つ。キューと送信の待ちは2秒、入力終了後の待ちは30秒で打ち切る。

Stopは取消世代を進め、停止前に準備したStartと遅着chunkを拒否する。端末の失効・音声権限の取消でも既存セッションを終了する。クライアントはStop前に再生キューを破棄し、旧session IDの音声を新しい再生へ混ぜない。RustではWebSocketの標準の`split()`で入力・出力を分け、`ene-rpc`の生成型`AudioFrame`を直接encode/decodeする。独自の音声イベント型やストリームラッパーは設けず、自動再接続は行わない。

実際のSTT/TTSエンジンは今回接続していない。`Audio::Engine`が接続口で、未接続時は`audio_available=false`と`Unimplemented`を返す。試験用エンジンで双方向通信、入力終了、停止、権限取消を検証した。実際の音声認識・合成の遅延は未計測である。

## 保存とプラグインの互換性を境界で保つ

RPCと保存の値は`ene-rpc`の生成型を直接使う。messageの任意フィールドはbuffaのMessageField、変更はoneof、cursor一覧は生成map、拡張JSONはJsonDocumentの文字列で表す。モデル情報はプラグイン側も同じ生成Modelを共有する。プラグインのmanifest・拡張API・worker通信は`ene-plugin`が所有し、PluginServiceの管理RPCと混同しない。独自のDTO変換クレートやRPC容量制限は設けず、Connectの既定値を使う。

生成型に直接接続する新規クライアントでは、URL組立て、要求のシリアライズ、通知フレームの判別を手書きしない。SDKは生成クライアントを直接呼び、認証、状態同期、音声を補助する。REST送受信とプロトコル選択の層は削除した。

タスク、相談回答、記憶、スケジュール、素材のHTTP転送は、従来から未実装の業務機能として残る。機能を追加するときは生成契約とホストの永続処理を一緒に追加する。通信のキャンセルだけで作業停止や永続化を保証しない。

契約の変更判断はこの資料と[`framework-boundaries.md`](framework-boundaries.md)を確認する。外部ライブラリの仕様は[Connect Rust](https://github.com/connectrpc/connect-rust)を参照する。
