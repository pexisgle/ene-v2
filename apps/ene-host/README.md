# ene-host

> このREADMEは現行実装と起動方法を説明します。採用設計は[アーキテクチャ](../../docs/architecture.md)、実装順序と受入条件は[Alpha計画](../../docs/alpha.md)を参照してください。

ホストの会話・設定・権限・生成処理、Connect API、SQLite保存、資格情報管理、プラグイン実行を持つパッケージです。親コンパニオンの会話はRig v0.46.0の実行カーネル（rig-ecs）が所有します。`src/lib.rs` に実装をまとめ、`src/main.rs` から `ene_host::run()` を呼び出します。

## 起動と接続

```sh
nix develop
just run-host
```

ExtismのHost SDKをホストプロセス内に組み込んで実行します。専用runner、自己再起動、bubblewrapは不要です。WASIを無効にし、プラグインの通信や状態変更は認可済みのホスト関数を経由させます。

ホストは `127.0.0.1` の空きポートで待ち受け、Linuxでは `$XDG_RUNTIME_DIR/ene-host/connection.json` に `endpoint` と `token` を書きます。ディレクトリは0700、ファイルは0600です。トークンは起動ごとに更新し、終了時に接続ファイルを削除します。同じランタイムディレクトリでの二重起動は拒否します。

クライアントは `ene_rpc::connect(endpoint, token)` で接続し、生成された `HostServiceClient`、`DeviceServiceClient`、`PluginServiceClient` を利用します。通常のRPC・通知、デバイス認可・操作、プラグイン管理はConnectサービスで扱います。

| 環境変数 | 用途 |
| --- | --- |
| `ENE_HOST_STATE_DIR` | SQLite保存先。Linuxの既定値はXDG stateディレクトリ内の `ene` |
| `ENE_HOST_SESSION_CREDENTIALS` | 設定するとAPIキーをセッション中だけ保持 |
| `ENE_PLUGIN_DIR` | プラグインの検索ディレクトリ。既定値はアプリのdataディレクトリ内の `plugins` |

初期コンパニオンと空の接続設定をSQLiteへ作成します。生成するには接続・資格情報・モデル割当・データ送信許可をAPIで登録します。組み込みプロバイダーはOpenAI Responses APIとOpenRouter（OpenAI互換Chat Completions）です。OpenRouter用Wasmは `examples/plugins/openrouter-wasm` をビルドし、manifestと `provider.wasm` をプラグインディレクトリのパッケージに配置します。資格情報はホスト側で付与し、ゲストには渡しません。

SQLiteには設定・操作の受付記録・プラグイン状態を保存します。OSの資格情報ストアへのアクセス失敗を永続保存成功として扱いません。停止時は新しい処理を受け付けず、実行中の生成・プラグイン・通知を終了し、保存処理を待ちます。スキーマは単一のbaselineで作成し、旧版の履歴migrationは持ちません。baselineより前に作られたDBは起動時に明示的に拒否されるため、開発用のstateディレクトリ（`ENE_HOST_STATE_DIR`）を削除して再作成してください。

### 親の会話（Rig実行カーネル）

親コンパニオンの会話は、ホスト内の専用スレッドで動くRigのヘッドレスAppが所有します。Bevyの描画Appとは別プロセス内の別Appで、フレーム描画・音声・UIは持ちません。

- 使用版は `rig-core` / `rig-http` / `rig-ecs` / `rig-memory` の 0.46.0（公式リリース v0.46.0、2026-10-31）で、`Cargo.lock` に固定しています。自動追従はしません。
- コンパニオンごとに一つの親エージェント（`AgentId` はコンパニオンID）を持ち、受信箱・ターン・モデル呼び出し・ツール呼び出し・停止をRigに任せます。ホストは入力の受理、認可、通知、設定の反映だけを行います。
- 会話・受信箱・ターンの記録はRigのセッションログとして `<state>/rig`（`ENE_HOST_STATE_DIR` 配下）に保存します。`GetHistory`、`GetMessage`、`GetSnapshot` の履歴はこのログから読みます。SQLiteの旧 `messages` 表は撤去し、会話の正本はRigのセッションログだけです。
- 親は1つのホストツール `current_time` を呼べます。ツールの実行・結果の会話への反映・次のモデル呼び出しはRigが行います。
- 実行中の追加入力は `Steer` として受信箱に入り、ターンの次のモデル呼び出しへ届きます。受理は、Rigが入力を受け付け、そのフレームのセッションログ書込みが成功した後に返します。保存できなかった入力は受理として返しません。
- 停止はカーネルへ停止要求を渡した時点で完了し、モデル応答や実行枠の空きを待ちません。停止・失敗・キャンセルは `TurnEvent` の `stopped` / `failed` として届き、部分的な返答は保存された会話のメッセージとして残ります。
- 失敗したターンが入力を会話から取り出した場合は `RecalledInput` で本文を返します。クライアントは入力欄へ戻せます。会話メッセージとしては保存しません。
- モデル設定・資格情報・送信許可が変わると、次回の会話接続時にRigのモデル連接を作り直します。変更時点で実行中のターンは停止し、古い許可で続行させません。
- セッションログへの書き込みに失敗すると、ホストは復旧が必要な状態になり、新しい会話の受理を停止します。以降の保存を成功として通知しません。
- 待機中はRigの完了・ストリーム・期限による起床だけを処理し、固定周期で推論やフレーム更新を回しません。NixOS / x86_64での実測は、初期化1秒後から10秒間でCPU 0.020秒（0.20%）、フレーム10回（1回/秒）、Rigの起床0回でした。1秒周期の期限起床は、遅延した再試行・期限の確認のために残しています。
- 再起動後に未確定の会話レシートが残った場合、同じ `request_key` の再送時にRigの保存済み会話・受信箱を `request_id` で照合します。見つかれば元の受理を返して再配送せず、見つからなければ未配送として503で確定します。クライアントが別の `request_key` で送り直した場合は、元の入力が保存済みなら重複し得ます（この場合は元の受付応答を受け取れなかったことを区別できません）。
- 返答の出力上限は、ストリーミング中の文字数と、確定前のReview段階の両方で判定します。上限を超えた返答は保存・通知せず、ターンを `output_limit` の失敗にし、入力を `RecalledInput` で返します。HTTPレスポンス本文には別途8MiBの上限があります。

### 会話APIの通知と照合

`SendMessage` は受理した入力の `request_id` を返します。返答本文や完了状態は戻り値ではなく、購読と履歴取得で確認します。

| 通知・取得 | 意味 |
| --- | --- |
| `MessageCreated` | 会話に確定したメッセージ（ユーザーの発言、モデルの返答）。`request_id` で受理した入力と対応します |
| `TurnEvent` | 入力から始まる実行の開始（`streaming`）と終了（`answered` / `stopped` / `failed`）。`stream_id` と最終メッセージIDを持ちます |
| `MessageDelta` | 返答の途中の文字片。`stream_id` と文字オフセットを持ち、確定本文ではありません |
| `RecalledInput` | 失敗したターンが会話から取り出した入力本文。クライアントの入力欄へ戻すための通知で、会話メッセージではありません |
| `MindUpdated` | SQLiteへコミットされた著者プロファイル/現在の心のrevision付き状態。心のツール更新とプロファイル編集の成功後にだけ届きます |
| `GetHistory` / `GetMessage` | Rigの会話ログから読む確定内容。ページカーソルは `sequence:companion_id` |

同じ `request_key` の再送は同じ `request_id` を返します。配送を確認できなかった受理は成功として返さず、再起動後は未確認として扱います。`Stop` は停止要求の受付を返し、停止完了は `TurnEvent` で確認します。

親の会話のモデル接続は、組み込みのOpenAI（Responses API）とOpenRouter（OpenAI互換のChat Completions）に対応します。接続はそれぞれのエンドポイント・資格情報・送信先許可を使い、OpenAIの接続設定をOpenRouterへ流用しません。

モデルIDはRigの組み込みカタログを優先します。カタログにないモデルは、接続の `options` に `{"tool_calling": true}` を明示した場合だけ、その宣言を根拠にツール対応として扱います。`false` は非対応、指定なしは能力不明として、それぞれ異なる理由で拒否します。未知のモデルを推測でツール対応にはしません。

Wasmプロバイダーは接続確認・タスク推論の既存経路で継続利用します。親の会話へ接続するには、現行のWasm契約とホスト側アダプターに次の不足があります。ゲスト契約の `GenerationRequest` には `tools` と `Event::ToolCall` がある一方、ホスト側は `tools: Vec::new()` を固定し、`ToolCall` を未対応として拒否し、ホストの `Generation`/`ChatMessage` はroleとtextしか運べません。ツール対応WasmプロバイダーをRigへ接続するには、ツール定義・ツール呼び出し・ツール結果を往復できるホスト側契約と、Rigの `Serve` ハンドラーへの薄い変換が必要です。別のエージェントループは不要で、Rigがターンとツール実行を保持できます。

OpenRouterの接続確認（`CheckConnection`）とタスク推論の単発生成はまだ未接続で、呼ぶと明示的に非対応を返します。文脈の自動圧縮・要約は未実装で、会話がモデルの文脈長を超えた場合はモデル呼び出しの失敗として表面化します。

プラグインの変更・呼び出しとゲストからの状態保存は、既存の `MutationExecutor` が所有します。RPCの切断やゲストのキャンセル後も、受理済みの状態保存は完了まで継続し、保存障害はホスト側に通知します。ホスト終了時は受付を閉じ、プラグインへ停止を通知してから、受理済み処理の保存完了を待ちます。

HarnessはACP v1相当のセッションをbindingごとに1件保持し、`running` / `waiting_for_input` / `completed` / `failed` / `cancelled`を保存します。`session/update`相当のイベントは受信ごとに永続化し、長期のターンでも進捗を失いません。`session/request_permission`相当の相談ではターンを中断し、`Respond`でゲストを再開します。Cancelはゲストを再開せず終端状態を保存し、再送時は保存だけを再試行します。再バインド・無効化・再読み込み・終了では`plugin_stopped`、起動時は`host_interrupted`として未完了タスクを終了します。旧版の保存形式は再開せず置換し、復旧保存に失敗した場合は起動を成功扱いにしません。長いターンでは `PluginResult.events` は最新256件に限り、正本はジャーナルです。ハーネスは権限相談の境界を除き1ターンが1回の呼び出しになるため、インスタンスとサービス待ちの実行上限を30分に延ばしています（CPUはfuelで制限）。

### 心と記憶（ステップ2）

**自動記憶（ステップ3）**：確定した会話のうち処理済みカーソルより後のユーザー発言とアシスタント応答を、有限のホストワーカーがタスク割当・タスクGrantで整理します。記憶の追加・訂正と処理済み位置は同じSQLiteトランザクションで確定し、生成失敗・保存失敗・記憶の版の競合ではカーソルを進めません。記憶の版は記憶の変更だけを対象にし、親の気分だけの変更では進みません。現在の感情・関係・作者設定・許可は自動整理から直接変更できず、公開範囲も広げません。親の応答は整理を待ちません。整理・索引の状態は`GetMind`の`consolidation`/`index`で確認できます。

**検索と自動想起**：記憶の挿入時にFTS5（trigram）と埋め込みの索引待ちを保存し、ホストのワーカーが索引を更新します。埋め込みはRigのOpenAI embedding wireへ認可付きトランスポートを渡し、コンパニオン接続のmemory Grantで送ります。model・次元・前処理を索引に保存し、検索時に同じmodelと前処理だけを使います。OpenRouter接続の埋め込みは未対応です。`mind_recall`は`since`/`until`/`include_forgotten`を受け、結果に`lexical`/`semantic`/`pending`/`reason`を含めます。通常入力の自動想起は語句検索で候補を作り、対象のモデル要求だけを保留して非同期に意味検索します。検索結果はrequest idに対応してモデル要求へ入り、holdを解除します。モデル要求より先に着いた結果はカーネルのpendingに保持し、Gateで適用します。受付は保存確認後に返り、検索の完了は`Signal::Command`でカーネルへ届きます。停止・キャンセル・遅い重複結果は送信済み要求やagent状態を書き換えません。入力が長い場合も200文字で無効にせず、4,000文字の予算と可視の`query_truncated`を使います。想起はmind.update/整理のプロンプト再合成で消えないよう保持し、合成時に最新の心を読み直します。`ControlMemory`でforget/remember/share/unshareできます（忘却は物理削除でなく通常想起からの除外）。共有の公開範囲は本文・説明・appraisal・訂正関係idで、私的な出所は渡さず、訂正関係は所有者の記憶から計算します。

作者が設定する人格の核は `Companion.mind`、側面・会話例・背景・表現語彙は `mind_companions.profile` に保存します。会話例はプロンプト上で「実体験として記憶しない」と区別し、記憶テーブルへ自動では入りません。現在の心は `mind_companions.state`（感情・関係・傾向・気にかけていること、revision、更新時刻）、経験は `mind_memories`（内容・評価・取得方法・出所・時刻・関連・訂正・更新元）、適用結果は `mind_operations`（操作ID・内容hash・結果・更新元・出所）に保存します。心と記憶の正本はSQLiteで、会話はRigのセッションログのままです。

親のRigエージェントには、ホストが所有者・入力出所・操作IDを束縛する `mind_recall` / `mind_read` / `mind_update` を渡します。現在の心の概要は毎回のsystem promptへ入れ、必要な記憶だけを `mind_recall` で検索します（現状はクエリ全体と日本語文字bigramの語句検索。FTS5・意味検索・自動想起はステップ3）。`mind_update` は記憶・現在の心・操作記録を同じSQLiteトランザクションで確定し、同じ操作ID・同じ内容の再試行は元の結果、異なる内容は競合、古いrevisionは競合、変更なしは成功です。ツール結果の記録が失われた後に同じ内容・同じ入力出所で再送された場合は、新しい操作IDでも適用済みの結果を返して二重適用しません。参照する記憶・訂正対象は同一コンパニオンの所有物だけを許可します。更新後は同じターンを止めず、次のモデル呼び出しへ現在の心とrevisionを反映します。モデル接続の変更や再起動ではSQLiteからプロンプトを再構成し、心を初期化しません。

心ツールを持つのはAlphaでは最初の親コンパニオン一つだけです。他の親・子・外部MCPには公開しません。クライアント向けには `GetMind`（核・追加材料・現在の心・profile/stateのrevision）、`UpdateMindProfile`（追加材料の著者編集）、保存後の `MindUpdated` 通知を提供します。核の編集は既存の `UpdateCompanion.mind` です。記憶一覧・編集UIは未実装です。

### ワークスペースとfsツール

`PluginBinding.workspace_root` に絶対パスを指定すると、`tools` を許可したハーネスが `fs.read_text_file` / `fs.write_text_file` を呼べます。ルートはバインド時にcanonical化し、ファイルシステムのルートは拒否します。パスはルート相対のみで、絶対パス・`..`・`.`・シンボリックリンク・通常ファイル以外を拒否し、読み書きは256KiB、書き込みはatomic rename、親ディレクトリは自動作成しません。端末実行は別途オプトインのcapabilityとして扱います。

### 端末（明示オプトイン）

ACPバインディングの`terminal`ポリシーを有効にした場合だけ、ACPの`terminal/create`・`output`・`wait_for_exit`・`kill`・`release`を処理します。コマンドは`allowed_commands`（実行ファイル名または絶対パスの完全一致）に限定し、シェルを介さず、`workspace_root`配下の`cwd`だけを許可します。実行ファイル名はホストの`PATH`で絶対パスに解決し、子プロセスの`PATH`では解決しません。`request.env`による`PATH`・`PATHEXT`・`COMSPEC`・`SYSTEMROOT`・`WINDIR`・`LD_*`・`DYLD_*`・`GLIBC_TUNABLES`の上書きは拒否します。許可は該当ターンの直前の`session/request_permission`に対するone-shotで、未使用のままターン終了・cancel・timeout・失敗した場合は破棄されます。出力は`max_output_bytes`で打ち切り、`max_seconds`で強制終了します。`terminal/output`と`wait_for_exit`は同じ終了監視を参照し、自然終了でも`exitStatus`を返します。**サンドボックスはなく、ホストのOSユーザー権限で実行されます**（コンテナ・seccomp・ネットワーク制限なし）。信頼できるエージェントとコマンドだけを許可してください。

### ハーネス状態の取得と通知

`PluginService.GetHarness` はバインディングIDから保存済みのタスクジャーナル（`running` / `waiting_for_input` / `completed` / `failed` / `cancelled`、`pending`、`tool_calls`、`plan`、`report`、`failure`、`stop_reason`）を返します。ハーネスの呼び出しが成功するたびに `Notification.harness_task`（`binding_id`、`companion_id`、`task_id`、`status`、`stop_reason`）を配信します。これは再取得のヒントであり、正本は `GetHarness` のジャーナルです。失敗した呼び出しはRPCエラーになるため、状態は `GetHarness` で確認します。これはTask APIの最小読み取り面で、コンパニオン単位の一覧・質問・報告APIは未実装です。

### タスク推論（`models` サービス）

`models` を宣言・許可したハーネスは、ホスト経由でタスク用のモデルを呼べます。ホストはコンパニオンの `model_assignments.task` とホスト設定の接続・モデルを使い、`purpose: "task"`・`data_kinds: ["task"]` のデータ送信Grantを検証します。ゲストが指定した endpoint・options・model・tools は採用せず、`max_tokens` はホスト上限（8192）に丸め、出力は `task` 操作の上限（256,000バイト）で打ち切ります。タスク割当が未設定なら `model_unset`、Grantがなければ `permission_required` を返します。推論はキャンセル可能で、ホスト終了時も実行中の送信を止めます。

### 外部ACPエージェント

`target: "acp"` の entrypoint を持つパッケージは、`command` の子プロセスをACPエージェントとして起動します。シェルを介さず、環境変数は許可リスト（`PATH`・`HOME`・`USERPROFILE`・`LANG`・`LC_ALL`・`TERM`・`SYSTEMROOT`）だけを渡し、ホストの資格情報は渡しません。`workspace_root` が必須で、fs client capability はそのルートに限定し、terminal はバインディングのポリシーが有効な場合だけ advertise します。ACP v1のJSON-RPC・要求ID・応答対応付けは公式SDK（`agent-client-protocol 5.0.0`）へ委譲し、ホストは子プロセス、workspace、terminal権限、上限、ターン状態だけを持ちます。`initialize`・`session/new`・`session/prompt`・`session/update`・`session/request_permission` を共通ジャーナルへ写像し、`GetHarness` と通知はWasmハーネスと共通です。未知の`stopReason`は`end_turn`ではなく `acp_failed` として失敗し、`terminal/wait_for_exit` 中でも `terminal/kill`/`release` は処理されます。ACPエージェントとterminalの子プロセスはプロセスグループ（WindowsはJob Object）で起動し、Cancel・無効化・再バインド・終了ではグループ全体を停止して終端化します。initialize等の失敗は `acp_failed` としてタスクを終了します。旧ホストはACP entrypointを無効なmanifestとして拒否します。

タスク推論・接続確認のLLM実行は無効化・生成キャンセル・実行失敗で出力転送も終了し、受信が止まっていても実行管理枠を解放します。完了イベントなしで閉じたストリームは失敗として扱います。

プラグイン契約はv4です。Harnessの入力・イベントはACP v1の語彙（`Start` / `Respond` / `Cancel`、`Update` / `RequestPermission` / `Stop`）で、Rust SDKの型は公式`agent-client-protocol-schema`を再exportします。受理するJSONは従来のv4と同じで、harnessが受け付ける`sessionUpdate`は`agent_message_chunk`・`agent_thought_chunk`・`tool_call`・`tool_call_update`・`plan`だけです。Characterのバインドには `device_id` と `companion_id` を明示し、呼び出しと返された操作の対象が一致することを検証します。旧バインドにはデバイスを自動補完せず、再設定が必要です。

デスクトップUI、音声認識・合成、ファイル操作ワーカー、完成版の自律タスクループはこのホストの実装完了範囲に含みません。

## パッケージの境界

旧 `ene-core` と `ene-outbound` をこのパッケージへ統合しました。旧 core はホスト固有の状態と処理を持ち、旧 outbound は core の通信を Rig に接続する薄いラッパーでした。独立した利用者がなく、認可と通信も一体で変更するため、クレートを分ける必要はありません。

Rig の `HttpClientExt` はホストの `AuthorizedTransport` に直接実装します。クレート間の制約を満たすためだけのラッパーと、その委譲メソッドは不要になりました。ホストから独立した利用者やビルド要件が生まれた時点で、実際に共有する責務を切り出します。

## 内部構成

| モジュール | 責務 |
| --- | --- |
| `application` | メッセージ受付、設定変更、停止、保存と通知の調整、心のプロンプト合成と更新適用 |
| `rig` | Rig実行カーネル（親エージェント、受信箱、ターン、セッションログ）との接続、会話の読み取り、心ツールのブリッジ |
| `domain` | ホストの状態、入力検証、許可と実行可否のルール、記憶・心の型と検証 |
| `ports` | 永続化、資格情報ストア、プラグイン管理、心・記憶のトランザクション境界 |
| `llm` | プロバイダー契約、生成入力、権限付き実行、HTTP通信、Rigとの接続 |
| `api` | 認証、Connectサービス、通知 |
| `infrastructure` | SQLite、資格情報ストア、OpenAI・プラグイン接続、接続情報の公開 |
| `devices` | デバイス操作の認可・寿命管理 |
| `bootstrap` / `server` | 依存の組み立て、起動、シグナルによる停止 |

`llm::invocation` は非公開モジュールです。認証情報を持つ `Invocation` と、その子モジュールの通信実装を同じ境界に置きます。APIキーをリクエストに付与するメソッドは非公開のまま、送信先検証・認証情報の付与・送信を一続きで実行します。送信前と受信中の権限確認、リダイレクト拒否、サイズ制限もここで適用します。

Rig との接続は通信実装の子モジュールに置き、ストリーミングイベントのサイズ制限もそこで扱います。保存先などを差し替えるインターフェースは維持し、SQLや資格情報ストアの実装はアプリケーション処理へ埋め込みません。

テスト用の `Invocation` 生成は `cfg(test)` の内部補助コードです。認可済み実行を任意に生成するための公開 feature は提供しません。

## 検証

```sh
cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown
cargo test -p ene-host -p ene-plugin-runtime --locked
cargo clippy -p ene-host -p ene-plugin-runtime --all-targets --all-features --locked -- -D warnings
# 実プロバイダーの受入（課金あり、既定は無視）。ENE_LIVE_PROVIDER=openai|openrouter
cargo test -p ene-host --test live_acceptance -- --ignored --nocapture
# 待機時CPUの実測（約11秒）
cargo test -p ene-host --lib measure_idle_cpu -- --ignored --nocapture --test-threads=1
```

通常のホストテストに、実際のPDK製WasmとHTTPブローカーの結合検証を含みます。
別のWasm出力先を使う場合は `ENE_TEST_REFERENCE_WASM` / `ENE_TEST_PDK_WASM` で指定します。

会話状態・許可判定に加え、実Connectクライアント、SQLiteの再読み込み、組み込みExtism、ローカルHTTPサーバーを使って認証・取り消し・停止・サイズ制限を検証します。心は、同じ操作の再試行・異なる内容との競合・古いrevision・他コンパニオンの記憶参照・不正フィールド・保存失敗時のロールバック・訂正関係・所有者を越えた検索・変更なし・再起動後の保持・プロファイル通知・スクリプト化親の更新が次のモデル要求へ入ることを検証します。Rig実行カーネルは、スクリプト化したモデルでツール呼び出し・追加入力・停止・待機時のフレーム数・セッションログの保存失敗・再起動後の復元を検証します。ホストAPIの統合テストも同じスクリプト化モデルで、受理、通知、履歴、停止、取消、再起動を通します。組み込みOpenAI Responsesの本番接続経路は、ローカルのSSEサーバーを相手に、リクエスト先・資格情報の付与・ストリーミング解釈を検証します（実モデルは呼びません）。別に、`.env` の鍵を使う明示的な受入テスト（`live_acceptance`、既定は無視）で、OpenAIの `gpt-6-luna` とOpenRouterの `google/gemini-2.5-flash-lite` を実際に呼び、ツール要求・実行・結果の返却・最終応答・履歴・再起動を確認します。`mind_acceptance_through_the_host` は、架空の人格と合成の好みを使い、`mind_update`の保存・`MindUpdated`・訂正、会話を持たない再起動先での`mind_recall`、OpenRouterへのモデル変更後の同じ記憶の利用を確認します。`automatic_memory_acceptance_through_the_host` は、親に保存ツールを使わせない通常のターンから、memory Grantのない自動整理が権限エラーで止まりproviderを呼ばないこと、Grant作成後の新しい入力で同じ未処理範囲を再開すること、Input/Reply出所の記憶、FTS/埋め込み索引、語句の自動想起、候補複数・語彙の重ならない問いへの自動意味想起（`mind_recall`未使用）を確認します。実測（最終ツリー）は、ステップ2の明示`mind_update`経路が9呼び出し・入力15406・出力754・参考コスト0.00186208 USD、自動記憶の受入が親4呼び出し・入力4597・出力397・参考コスト0.00064517 USD、整理1モデル呼び出し（入力158文字、トークンusageは未取得）、埋め込み4呼び出し・431トークンでした。memory Grantなしの整理は`state=error/reason=PermissionRequired/jobs=0`でprovider呼び出し0回、Grant作成後の新しい入力で再開しました。受付は18〜20ms、初回応答は親推論を含み2.0〜3.1秒でした（料金はRigカタログ単価からの参考値で、請求額ではない）。鍵はテストプロセスから通常の資格情報APIへ渡し、表示・保存しません。Linuxの実ホストプロセスのテストでは起動、二重起動拒否、不正トークン拒否、変更の保存、購読中のSIGTERM、再起動後の復元を確認します。通常のテスト実行では有料の外部API・実モデルを呼びません。`live_acceptance` は明示実行した場合だけ `.env` の鍵で実プロバイダーを呼びます。

旧Luauプラグインは非対応です。Rust製プラグインは `ene-plugin-sdk` への依存を `ene-plugin` に変更します。共通契約はバージョン4です。ハーネスはACP v1の入力・イベントへ移行し、`contract_version` を4に更新してください。旧v3の保存ジャーナルは起動時に`host_interrupted`として終端化され、再開しません。revisionが変わるため、再有効化とバインディングの再設定が必要です。保存データは削除しません。旧 `ENE_PLUGIN_RUNNER_BIN` / `ENE_PLUGIN_CACHE_DIR` は不要で、旧コードキャッシュを実行時に読みません。
