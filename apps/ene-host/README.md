# ene-host

AIとの会話、コンパニオンの心と記憶、作業セッション、接続・送信許可、接続端末の操作をまとめる常駐ホストです。会話の実行管理はRig v0.46.0（`rig-core` / `rig-ecs` / `rig-memory`）に任せ、`ene` 固有の状態だけをSQLiteへ保存します。

> 現行実装の説明です。製品としての振る舞いは [docs/features](../../docs/features/README.md)、採用設計は [docs/architecture](../../docs/architecture.md) を参照してください。**接続経路の認証は未だOwner Bearerのみ**です。外部アクセス制御への移行が完了するまで、この認証を外さないでください。`client_id` は接続端末の区別であり、認証情報ではありません。

## 起動

```sh
nix develop
just run-host
# または
cargo run --locked -p ene-host
```

ホストは `127.0.0.1` の空きポートで待ち受け、ランタイムディレクトリの `connection.json` に `endpoint`（`/rpc` 付き）と `token` を公開します。ディレクトリは0700、ファイルは0600で、終了時に削除します。同じ状態またはランタイムディレクトリでの二重起動は拒否します。

環境変数:

| 変数 | 用途 |
| --- | --- |
| `ENE_HOST_STATE_DIR` | SQLiteとRigセッションログの保存先 |
| `ENE_HOST_RUNTIME_DIR` | `connection.json` とロックの保存先 |
| `ENE_HOST_SESSION_CREDENTIALS` | 設定時、登録した資格情報をメモリだけに保持する |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` | 接続に登録済み資格情報がない場合のフォールバック |

`connection.json` のendpointとtokenを `ene_rpc::connect` に渡し、生成された `HostServiceClient` / `ChatServiceClient` / `SettingsServiceClient` / `WorkServiceClient` / `ClientServiceClient` を使います。

## 仕組み

- **1つのヘッドレスRig App** を専用スレッドで動かします。APIからの読み書きは `Runtime::call` でこのスレッドへ直列化し、SQLiteは1接続です。モデル・接続確認・自動整理の外部通信だけが非同期です。
- 会話・受信箱・ターン・モデル／ツール呼び出しはRigが所有し、セッションログ（`<state>/rig`）が会話の正本です。SQLiteはコンパニオン、設定、許可、受付レシート、端末、作業、心・記憶を保持します。
- ホストが束縛するツール（`mind_*`、`start_work` など）はRigの開いたツール（open tool）として登録し、ホストのスレッド上で確定します。モデル経由の端末操作も同じ経路で、端末が結果を返すまで呼び出しを保持します。
- 人格の核と作者プロファイルは安定したsystem promptに置きます。現在の心と自動想起は `Inform` メッセージとして渡し、プロンプトキャッシュを毎回壊しません。
- 自動記憶は、確定したターンだけを前向きに読み、`memory` 許可とコンパニオン接続でJSON出力を得て、記憶と処理済みカーソルを同じSQLiteトランザクションで確定します。
- 端末操作はアクティブ端末1台へ配送します。オフラインなら `pending`、配送後に切断したら `uncertain` とし、自動再送しません。成否不明を成功として扱いません。
- モデル出力の上限、入力長、HTTP/ストリームサイズ、履歴ページ数を制限します。ストレージまたはセッションログの書込みに失敗した場合、ホストは新規受理を止め、未保存の状態を通知しません。

## 内部構成

| パス | 責務 |
| --- | --- |
| `src/server/mod.rs` | 起動、シグナル、終了 |
| `src/server/access.rs` | プライベートディレクトリ、ロック、`connection.json` |
| `src/server/rpc.rs` | 生成されたConnectサービス、Owner Bearer認証 |
| `src/host/mod.rs` | Appスレッド、`Runtime`、`Owner`、通知、スナップショット、失敗・復元・停止 |
| `src/host/kernel.rs` | RigのPrepare/Gate/Review/SessionLog拡張、共通ツールディスパッチ |
| `src/host/chat.rs` | 会話エージェント管理、入力受理（冪等レシート）、履歴 |
| `src/host/work.rs` | 作業セッション、親への完了報告、`start_work` / `send_work` / `stop_work` / `list_work` / `report` ツール |
| `src/host/settings.rs` | 接続設定、送信許可（Grant）、可用性（Availability） |
| `src/host/db/` | SQLite WALドキュメントストア、記憶テーブルとFTS5 trigram検索 |
| `src/host/model/` | Keyring資格情報、組み込みプロバイダー、モデル能力、認可トランスポート |
| `src/host/mind/` | コンパニオン、心（プロファイル・状態）、記憶制御、自動整理、`mind_*` ツール |
| `src/host/client/` | 端末登録、排他アクティブ化、デバイス操作ストリーム、`device_request` ツール |

## データベース

スキーマは `PRAGMA user_version` を版とする連番migration（`src/host/db/migrations/`）で管理します。起動時は未適用分だけを1件ずつトランザクションで適用し、1件でも失敗したらその版を適用前の状態に戻して起動を中止します。2件目以降を適用する前には、同じディレクトリへ `host.sqlite3.bak-v<適用前の版>` を作ります。失敗した場合はこのバックアップとログを確認してください。

2026-11-09の書き直し直後のDB（`meta.schema_baseline` を持ち `user_version` が0）は、その場で `user_version = 1` として引き継ぎます。それ以外の未知のDBと、このホストより新しい版のDBは起動時に拒否します（書き換えません）。

## 現時点で実装していないもの

意味検索・埋め込み索引、スケジュール／自発実行、Web検索ツール、音声STT/TTS、キャラクターパッケージ入出力、ACP/MCP/Wasmの外部アダプター、ホスト側のfs/terminalツール、汎用の承認ホールドUIは未実装です。コンテキスト窓（Fit）と履歴の圧縮（要約）は実装済みです。これらを追加する際の境界と、先に直すべき前提は次節にまとめます。

## 監査の修正状況

2026-11-10の[実装監査](../../docs/reviews/ene-host-audit-2026-11-10.md)のA1–A5、B1–B12、C1–C14を修正しました。項目ごとの採った案と回帰テストは監査レポートの「状態」欄に記録しています。残るのは次の1点だけです。

| 項目 | 状態 |
| --- | --- |
| C13（Ownerトークンの生成） | 変更なし（決定済み）。`Uuid::new_v4` 2個（同一CSPRNG由来で実効244ビット）を連結しています。差し替えるなら `src/server/access.rs` の `token` の1箇所です。 |

### 運用上の注意

- RPC本文の上限は Connect の既定（4 MiB）です。アプリケーション層の `DefaultBodyLimit` は経路に効かないため置いていません。入力を65,536バイトに制限するのは受理側です。
- 受理レシートの `Accepted` は、Rigのセッションログ追記が成功した後に書きます。Rigの追記は `fsync` しないため、電源断では `Accepted` だけが残る可能性があります。その場合は同じ `request_key` の再送で会話を照合してください。
- 資格情報は `keyring 4` の `v1` 互換ファサードでOSのキーチェーンに保存します。`keyring-core` と個別ストアへの移行は、配布形態が決まってから行います。
- 実プロバイダー（OpenAI）で、接続作成・資格情報登録・`CheckConnection`（モデル一覧）・Grant作成・取り消し・SIGTERM停止までを一時stateで確認済みです。会話の往復は、実在するモデルIDをコンパニオンへ割り当てていないため未確認です。
- Bevyのタスクプールは最大3スレッドに制限しています（1エージェントの実行にコア数分は要らない）。この環境（x86_64-linux、`--release`しない開発ビルド）でアイドル10秒のCPU時間は1 tick・0.10%でした。

## 将来機能の追加余地

機能の一覧は [docs/features](../../docs/features/README.md) にあります。次の表は、各領域を追加するときに現行の構造を作り直さずに済むかを示します。「先に直す前提」は、まだ実装していない必要な準備です。Web検索のようにツール登録1件で済むものは対象外です。

| 機能領域 | 利用できる現行の境界 | 先に直す前提 |
| --- | --- | --- |
| 長期の会話継続 | 会話の正本はRigのセッションログ。`GetHistory` はカーソル式。`PrepareSystems::Fit` の窓と、落ちた範囲の要約（エージェントのコンポーネント）で文脈を保つ | 明示的な圧縮操作と要約の取り消し（[ADR](../../docs/design/compaction.md)の残り） |
| キャラクターパッケージ | `Companion.body/voice` とプロファイル（`CompanionRecord.profile`）は、交流で育つ状態（`state`・記憶）と分離済み | VRM等の大きな資産を置くBLOB領域と配信経路（JSONドキュメントには入らない） |
| 記憶の共有・忘却・訂正 | `audience`・`recall_preference`・`supersedes`。忘却・共有は `ControlMemory`（デッドロック修正済み） | なし |
| 意味検索・埋め込み | FTS5 trigram。再検証は正本の `audience` / `recall_preference`。スキーマ変更はmigrationで足せる | 埋め込み接続の `kind` とその送信先Grant（[権限](../../docs/design/permissions.md)の追記に従う） |
| 承認・保留・質問 | `kernel::gate` が単一の判定点。Rigの `Decision` / `HoldOf` / `Ask` を使える | 保留要求（request id・対象・版）のSQLite保存と再起動後の復元、回答RPCの新規追加 |
| 送信許可の拡張（埋め込み・音声・検索の送信先） | `Grant`（purpose / destinations / data_kinds / fingerprint）。`data_kinds` は purpose が要求する種類を包含することを検証済み | 新しい `kind` の送信先と、それを要求するGrantの対応づけ（[権限](../../docs/design/permissions.md)） |
| 外部の作業相手（ACP/MCP/Wasm） | `WorkSession.backend` を `Backend` の enum で解釈し、Rig固有の参照はその分岐の内側に閉じている | 新しいバックエンドの分岐。MCP公開はツールハンドラを `Owner` 基準にするか代理エージェントを使う |
| スケジュール・自発実行 | `Deliver`（`Queue` / `Steer` / `Inform`、`Queue` はアイドルの親を起こす）、1秒周期の起床は停止経路と分離済み | 期限は壁時計で保存し、スリープ後の期限超過は一度だけ評価する |
| 画面見守り・端末操作 | `client_id`、端末の `granted_capabilities`、`device_request`（アクティブ端末のみ）、`uncertain` 状態、client別の通知フィルタ | 送信先のGrant（[権限](../../docs/design/permissions.md)） |
| 音声 | Connectの双方向ストリームを新サービスとして追加できる | TTS/STTの送信先Grant（[権限](../../docs/design/permissions.md)） |
| 外部アクセス制御（Tailscale / Cloudflare） | `server/rpc.rs` の `authenticate` が単一の差し替え点 | `docs/design/host-access.md` の検証完了まではOwner Bearerを外さない |

追加するRPCは既存フィールドを再利用せず、新しいフィールド・サービスとして足します。スキーマを変えるときは `src/host/db/migrations/` に連番のmigrationを足します（`user_version` が版です）。

## 検証

```sh
cargo test -p ene-host --locked
cargo test -p ene-host --test process --locked
cargo clippy -p ene-host --all-targets --all-features --locked -- -D warnings
```

実プロバイダーを呼ぶ受入テストは通常のテストに含めません。必要な場合は一時stateディレクトリと資格情報を用意し、手動でホストを起動して確認します。
