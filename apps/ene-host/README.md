# ene-host

AIとの会話、コンパニオンの心と記憶、作業セッション、接続・設定・承認、クライアント能力と端末操作をまとめる常駐ホストです。会話の正本は rig v0.46.0 のセッションログ（`rig-ecs` + `rig-memory::DirStore`）に任せ、SQLite はコンパニオン・設定・承認・受付・作業・クライアント・記憶だけを持ちます。

> このパッケージは [ホストAPI設計](../../docs/api.md)（§7 のカノニカル Protobuf 定義が正本）と [全体アーキテクチャ](../../docs/architecture.md) に沿って書き直した実装です。製品としての振る舞いは [docs/features](../../docs/features/README.md)、知覚・身体・音声の契約は [共通仕様](../../docs/design/voice-avatar.md) を参照してください。**接続経路の認証はOwner Bearerのみ**です（[ホスト接続の設計](../../docs/design/host-access.md) の移行は未完了）。外部入口を経由する場合は、`ENE_HOST_ALLOWED_HOSTS` にその入口の名前を設定します。`client_id` は接続端末の区別であり、認証情報ではありません。

## 起動

```sh
nix develop
just run-host
# または
cargo run --locked -p ene-host
```

停止中のホストは `ene-host --reset-history` で全コンパニオンの会話・作業履歴と記憶を消去できます。接続設定、資格情報、コンパニオン設定、許可ルールは保持されます。

ホストは `127.0.0.1` の空きポートで待ち受け、ランタイムディレクトリの `connection.json` に `endpoint`（`/rpc` 付き）と `token` を公開します。ディレクトリは0700、ファイルは0600で、終了時に削除します。同じ状態またはランタイムディレクトリでの二重起動はロックで拒否します。SIGINT/SIGTERMで購読ストリームを閉じ、保存を終えてから終了します。

環境変数:

| 変数 | 用途 |
| --- | --- |
| `ENE_HOST_STATE_DIR` | `host.sqlite3` とRigセッションログ（`<state>/rig/`）の保存先 |
| `ENE_HOST_RUNTIME_DIR` | `connection.json` とロックの保存先 |
| `ENE_HOST_BIND` | 待受アドレス（既定 `127.0.0.1:0`）。loopback以外は警告を出し、Owner Bearerは必須のまま |
| `ENE_HOST_ALLOWED_HOSTS` | loopback以外で`Host`/`Origin`として受け入れるauthority（カンマ区切り、例 `ene.example.com`）。Tailscale ServeやCloudflare Tunnelが公開名をそのまま転送する場合に設定する。未設定はloopbackのみ |
| `ENE_HOST_ALLOWED_CAPABILITIES` | クライアントに許可する能力名のカンマ区切り。`*`で全宣言を許可。未設定・空は何も許可しない（操作単位の承認は別に必要） |
| `ENE_HOST_SESSION_CREDENTIALS` | 設定時、登録した資格情報をOSキーリングではなくメモリだけに保持する |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` | 登録済み資格情報がない場合のフォールバック |

`connection.json` のendpointとtokenを `ene_rpc::connect` に渡し、生成された `HostServiceClient` / `ChatServiceClient` / `SettingsServiceClient` / `WorkServiceClient` / `ClientServiceClient` を使います。

ホストは起動時と稼働中に Models.dev の公開カタログを取得し、Rig の同梱カタログにないモデルの入出力形式とツール対応を補います。設定画面と「手」のモデル接続は同じカタログを参照します。取得できない場合は、稼働中に取得済みの情報または同梱カタログを使います。

### Google Agent Platform（旧 Vertex AI）

- **Express mode** は初回セットアップまたは接続設定で「Google Agent Platform（旧 Vertex AI）」を選び、[Google Cloud のキー管理画面](https://console.cloud.google.com/agent-platform/api-keys)で取得した Express mode の API キーを登録します。
- **通常の Google Cloud プロジェクト** は接続設定で同じプロバイダーを追加し、プロジェクト ID と必要に応じてロケーションを保存します。ロケーションの既定値は `global` です。Agent Platform API を有効にし、必要な IAM 権限を持つサービスアカウントに紐づいた [Google Cloud API キー](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/start/api-keys?usertype=standard) を登録します。
- 「モデル一覧を取得」は、登録した API キーと接続先を使って、同梱の会話向け Gemini 候補と手入力したモデル ID を Google の Count Tokens API で個別に確認します。応答が確認できたモデルが一覧に表示されます。モデル一覧の確認時にテキスト生成は行いません。

RPC では LLM 接続の `adapter` に `google-agent-platform` を指定し、通常接続の場合は `options` に JSON オブジェクトの文字列（例 `{"project_id":"my-project","location":"asia-northeast1"}`）を設定します。キーは `RegisterCredentials` で登録します。

## 設計との対応

- **1つのヘッドレスRig App** を専用スレッドで動かします。APIからの操作は `Runtime::call`（変更。1フレーム進めて通知を配信）と `Runtime::read`（読み取りのみ。時間を進めない）でこのスレッドへ直列化します。
- **会話ログが正本**: 会話・受信箱・ターン・モデル/ツール呼び出しは `rig-ecs` が所有し、`<state>/rig/` の `DirStore` が正本です。`GetHistory` / `GetMessage` はこのログからの読み取り専用投影です。受理レシート（`ene.accepted`）と要約スナップショット（`ene.summary`）も `Conversation::add_aside` でログ内に置きます。
- **静的な人格と動的な文脈の分離**: 人格の核・作者プロフィールは `SystemPrompt` に固定します。現在時刻・現在の心・Pinned Memory・想起記憶・知覚要約は `PrepareSystems::Edit` で最新ユーザー発話の `ModelRequest.history` にだけ結合し、`SessionLog` には書きません。ターン開始時に一度だけ生成してReAct反復で使い回します。
- **文脈窓の管理**: `PrepareSystems::Fit` で要求全体のトークン数を見積もり、古いツール出力のクリア → 古い完全ターンの削除（ToolCallとToolResultを分断しない）の順で要求ビューを切り詰め、`[System Notice: ...]` を付加します。固定部分だけで上限を超える場合は送信せず `context_exceeded` で拒否します。
- **Compaction**: トークン使用率（75%）がターン境界で閾値を超えたときだけバックグラウンド要約を起動します。要約は `rig-memory` の `CompactionPolicy` / `Summarizer` を使い、`ene.summary` Aside（本文と `through_message_id`）として追記保存し、`GetHistory` には現れません。要約のモデル呼び出しには60秒のタイムアウトを置き、同じジョブが同じ会話区間の記憶整理・関係性・感情/成長の振り返りまで1パスで実行します。間に合わない場合は `recent::view` で古いターンを要求ビューから切り詰めて対話を続けます。
- **承認ゲート**: `Stage::Gate`（排他システム）で、ホスト管理ツール以外の端末操作を対象クライアントに束縛し、ポリシーと能力宣言（`AUTO` / `CONFIRM` / 未指定）から `ApprovalRequest` を作るか拒否します。保留は `HoldOf` で表現し、`SettingsService.SubmitApproval` か「頭」の `resolve_approval` ツールが `Approved` マーカーを付けて解放します。要求の引数ハッシュが変わっていれば承認は失効します。
- **クライアント能力**: `RegisterClient` / `SetCapabilities` の宣言（能力名・JSON Schema・プリセット・ツール名・承認要否・承認対象テンプレート）を検証し、ホストが許可を決めます。`tool_name` を持つ許可済み能力だけが `world.spawn_tool` で親エージェント向けのツールとして動的に登録され、能力変更・有効化の変更・切断に追随して入れ替わります。`tool_name` のない能力は汎用 `device_request` からのみ呼べます。入力・出力スキーマは `jsonschema` で検証し、承認ハッシュは RFC 8785 の canonical JSON で計算します。能力の許可は操作単位の承認を省略しません。
- **端末操作**: `ClientService.DeviceStream` 上の `DeviceOperation` として配送します。対象端末はモデルが指定せず、ホストが配送時点のアクティブクライアント（手からの操作は `StartWork` 時に保存した `client_id`）へ届け、結果は `operation_id` で照合します。端末移動は世代を持たず最後の `Activate` が勝ち、移動済みでも保留中の承認は失効させません。切断では未応答の操作を `uncertain` とし、再接続時に未配送分だけ再送します。
- **作業（手）**: `WorkSession`（文脈）と `Run`（1実行）を分け、`RunInfo` は `RunStatus` と `pending_request_id` を返します。入口は `start_work` / `send_work` / `stop_work` で、モデルから見る限りバックエンドの差はありません。親のtask接続が `CONNECTION_KIND_ACP` で、`endpoint.adapter` が名指すハーネスプラグイン（ABI 8。`Start` が `cwd` を運び、`Prompt` が生存中のセッションへ追加ターンを届ける）を導入済みなら、そのセッションはプラグインの「手」で走ります。進捗・計画・ツール呼び出しの更新は既存の報告経路で親へ届き、プラグインの `RequestPermission` は通常の承認カード（`SubmitApproval` / `resolve_approval`）になります。アダプターごとに同時に1セッションまで（2つ目の `start_work` は `unavailable` で拒否）、`stop_work` は実行中の呼び出しを中断するか保留中の承認へ `Cancel` を届け、その場でRunを `Cancelled` にします。再起動で開いていたプラグイン側Runは失敗として確定します。それ以外の割り当てはWorkSessionごとに1つのプロセス内Rigエージェントを維持し、複数Runで会話文脈を共有します。`send_work` は実行中のセッションへも同じくキューとして届けます（プラグイン側は次の `Prompt` ターンとして）。`report` ツール（activity / message / completed / failed）の完了・失敗は一人称の報告として親へ配送し、90秒の進捗途絶を1回だけ親へ通知します。ACPプロセスの起動はプラグインの内側の責務で、ホストはACPプロセスを持ちません。
- **心と記憶**: SQLite（WAL、`busy_timeout=5000`、`synchronous=NORMAL`）に保存します。FTS5 trigram（3文字未満はLIKE）とEmbedding接続によるベクトル検索をRRF（k=60）で融合し、`CompactVectorIndex` でBLOBの再読込を避けます。並び順は `Score = min(1, Importance/0.3) * (0.5*Relevance + 0.3*Importance + 0.2*Recency)` です。忘却は `importance` の引き下げであり、削除や除外フラグではありません。`mind_read` などの明示利用だけが `last_accessed_at` を更新します（引用ガード）。ConsolidationはCompactionと同じジョブ・同じ会話区間で1回だけ走り、位置は最新の `ene.summary` Aside が持つので別のカーソルを持ちません。`mind_update` は追加・訂正・忘却・`supersedes` を単一トランザクションで適用します。ユーザープロファイルは動的文脈へ注入します。埋め込み・整理のエンドポイントは、loopback以外の平文HTTPやプライベート/リンクローカルIPを拒否します。
- **設定と接続**: `Connection` は共通ヘッダー（name / enabled / kind / revision / last_check）と種別ごとの config（LLM / TTS / STT / Embedding / MCP / ACP）を持ちます。LLMはrig-coreのカタログからモデル仕様を引き、`ConnectOptions` で資格情報・base URL・HTTPクライアントを渡してエージェントの `Connection` を組み立てます。資格情報はOSキーリング（`ENE_HOST_SESSION_CREDENTIALS` でメモリのみ）に保存し、読み出しAPIは状態だけを返します。
- **知覚（目）**: `ClientService.ReportPerception` で担当端末の観測を受信します。「目」用モデル（未指定時は会話モデル）がウィンドウ、活動情報、対応モデルでは画面画像も評価します。日常の小さな変化も短く要約して「頭」へ報告できます。`eye.watch` の一致も「頭」に配送し、返答に取り入れるか、無言の仕草にするか、静観するかは「頭」が判断します。
- **知覚の診断画像**: `screen_received` と `evaluated` の `frame_id` で、受信した画像とモデルリクエストに含まれた画像を照合できます。`HostService.GetPerceptionFrame` で有効な知覚セッションの直近8枚の受信画像と最後の評価画像を、取得から最大3分間取得できます。画像は通知履歴や永続ストレージに書き込みません。
- **通知**: 1つのアウトボックスに溜め、フレームごとに起動epochと連番カーソルを付けて配信します。直近512件はカーソル再開用に保持し、`SubscribeRequest.cursor` が同epochで途切れなく続く場合は差分を再送、それ以外は `initial_state` に戻します。`client_id` 宛ての通知は他の購読者へ流しません。取りこぼしでは `resync_required` を送り、保存済み状態が正本です。
- **音声**: インストール済みのWasmプロバイダーがTTS/STT接続のadapterを提供していれば `VoiceService` を提供します。`Stage::Review` の文字片を文単位（`。！？\n`）で切り出して先行合成し、`generation_id`・`sequence_no`・`UtteranceFinished` を流します。発話権はターン終了（またはFlush・所有者の失敗）で解放し、合成中の音声は`is_final_chunk`まで続けて送ります。`generation_id`と`FlushControl`で割り込み時の旧世代を破棄し、端末移動時は旧世代を失効させます。音声のサンプルレート変換は `rubato` に委譲します。音声からの承認確定はホストのキーワード判定ではなく「頭」が会話文脈から`resolve_approval`で行います。音声認識の`transcription_id`単位で`start_work`を原子的に調停し、ターン終了時は他のコンパニオンへ`speech_completed`を同報してユーザー入力なしの掛け合いを促します（往復回数の上限は設けません）。接続がなければ各RPCは明示的な `unimplemented` を返し、接続はあるがadapterを提供するプラグインがない場合は `failed_precondition` を返します。

## 内部構成

| パス | 責務 |
| --- | --- |
| `src/server/` | 起動、プライベートディスカバリ、Owner Bearer、Connectサービス |
| `src/host/mod.rs` | Appスレッド、`Runtime`、スナップショット、購読、失敗時の停止 |
| `src/host/agent.rs` | コンパニオン/作業エージェントの作成、モデル束縛、プロンプト、ターン・文字片の投影 |
| `src/host/chat.rs` | 受理レシート、停止、履歴、メッセージ |
| `src/host/mind/` | コンパニオンCRUD、記憶、想起スコア、埋め込み索引、自動整理、動的文脈とFit、`mind_*` ツール |
| `src/host/compaction.rs` | アイドル要約（`ene.summary`）と、単一パスの要求フィット（`fit`） |
| `src/host/autonomy.rs` | `noop`、スケジュール登録ツール、発火ループ（croner/run_at） |
| `src/host/delegation.rs` | 音声`transcription_id`単位の作業委譲調停 |
| `src/host/voice.rs` | TTS/STTプロバイダー境界（Wasmプラグイン）、文分割・世代管理・フロア制御 |
| `src/host/permission/` | `Stage::Gate`、承認保留と解決、ポリシー判定 |
| `src/host/client/` | クライアント登録、能力ツールの公開、アクティブクライアント、知覚、`DeviceStream` と操作台帳 |
| `src/host/work/` | WorkSession/Run、1セッション1エージェント、`start_work` などのツール、`report`、停止・途絶検知 |
| `src/host/model.rs` | 資格情報、LLM接続、モデル一覧、接続チェック |
| `src/host/net.rs` | HTTPクライアントの共有、ループバック/SSRF検証、サイズ上限付きボディ読み取り |
| `src/host/db/` | typed SQLite store とbaseline schema |
| `src/host/tests.rs` | ECSループを含む挙動テスト（受理、ゲート、端末束縛、作業報告） |

## データベース

`host.sqlite3` は `PRAGMA user_version = 5` の単一baselineです。主なテーブルは `companions` / `connections` / `settings_state` / `policy_rules` / `approval_requests` / `clients` / `activations` / `work_sessions` / `work_runs` / `device_operations` / `memories` / `memories_fts` / `memory_edges` / `mind_operations` / `memory_injections` / `schedules` です。`approval_requests` は端末能力の承認を `client_id` / `work_id` に束縛します。未知の新しい版のDBは起動時に拒否します。開発用のstateディレクトリを削除して作り直してください。

- `activations` がコンパニオンごとのアクティブクライアントを持ちます。移動は最後の `Activate` が勝ち、旧クライアントには音声の `FlushControl` を送ります。
- 承認は `approval_requests` に残し、起動時に未解決分を失効させます。端末能力の承認は `client_id` / `capability` も保存します。
- `memory_edges` は `remembers` / `cited_in` / `estimated_cited_in` / `supersedes` の高々1ホップ関係です。

## API契約

`crates/ene-rpc/proto/host.proto` が正本で、`docs/api.md` §7 と一致します。有限の状態はenum、イベント詳細は `oneof`、自由形式の値は `JsonDocument`（schema + JSON text）で運びます。`Subscribe` は `Notification` の単一ストリームで、`activation_updated` や `device_operation` もここを流れます。実際の端末操作の配送は `ClientService.DeviceStream` だけです。

## 現時点で実装していないもの

- スキルファイル（`SKILL.md`）は未実装です。Wasmプロバイダーはホストへ組み込み済みで、`ene-plugin-runtime` のExtism実行基盤を使用します。MCPサーバーのツール登録は `rig-rmcp` と `ToolServerHandle` を使用します。
- スケジュール発火は`Queue`で親へ直接注入し、`cron_expr`/`run_at`はホストのローカル時刻で評価して`next_run_at`を保持します。

- Barge-inは明示操作で行います。
- クライアント能力ツールは `world.spawn_tool` で動的登録し、能力変更・有効化変更・切断に追随して入れ替えます。同名ツールは宣言（説明・入力スキーマ）が一致する限り1つの定義を複数コンパニオンで共有し、一致しない宣言は暗黙に切り捨てず明示的な競合エラーにします。親エージェントには会話用の組み込みツールと、アクティブクライアントが許可した能力ツールを公開します。組み込みの「手」は `report`、`device_request`、`mind_read`、`mind_recall`、`web_search` と、接続済みの MCP ツールを使えます。
- `web_search` は [Mwmbl の公開検索 API](https://developer.mwmbl.org/) に検索語を送り、タイトル・出典 URL・抜粋を返します。API キーは不要です。既定は5件、最大10件で、匿名利用枠のレート・件数上限が適用されます。

## 検証

```sh
cargo test -p ene-host --locked
cargo clippy -p ene-host --all-targets --all-features --locked -- -D warnings
```

実プロバイダーを呼ぶ受入テストは通常のテストでは無視します。テストは専用の一時stateディレクトリとホストプロセスを使います。
