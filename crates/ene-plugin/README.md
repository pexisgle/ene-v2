# ene-plugin

プラグイン作者向けの型定義とRust SDKをまとめています。manifest、拡張API、ホストサービスの契約に加え、WasmビルドではExtism PDKと `emit` / `service` / HTTPヘルパーを提供します。
旧 `ene-plugin-sdk` はこのパッケージへ統合しました。`use ene_plugin::*;` で利用できます。
モデル情報・設定項目は `ene-rpc` の生成型を共有しますが、プラグイン通信にConnect RPCを使う意味ではありません。

## 現在の実装範囲

実行と検証は `ene-plugin-runtime`、アプリ固有の認可は各アプリが担当します。
`ene-host` はExtismをプロセス内に組み込んで実行します。`ene-desktop` はまだ雛形で、
OSイベント取得、購読の認可と配送、アバターへの動作反映は未実装です。
`ene-avatar` は `apps/ene-avatar/src/control.rs` のJSON行通信を持ちますが、
プラグインイベントからその通信への変換はまだありません。

Extism の組み込み `extism:host/env`（alloc/store/load、config、log）は
PDK の都合で残ります。実行基盤は `max_var_bytes = 0` にして var 書き込みを
無効化しますが、`log_*` は manifest の `log` サービスとは別に host の stderr へ
出る点に注意してください。

## 共通契約

| 定義 | 用途 |
| --- | --- |
| `Manifest` / `EntryPoint` | 実行入口、提供する拡張、利用するサービス |
| `Operation` | ホストからプラグインへの依頼 |
| `Event` | プラグインからの結果・途中経過 |
| `ServiceRequest` | プラグインからホストへの機能利用依頼 |
| `Failure` | ホストサービス呼び出しの失敗 |

実行方式はExtism固定で、manifestに `runtime` は指定しません。Luauスクリプトは受け付けません。WasmのABIはExtismが担当します。
共通型はクレート直下から利用できます。内部ファイルは `manifest.rs`、`api/`（`wire.rs` の共通enumと、`llm` / `speech` / `character` / `harness` / `http` / `desktop` の型）、`window.rs`、`failure.rs`、`sdk.rs` です。
`provider` は認証方式、ルーティング設定、チャットメッセージを提供します。
HTTPは `HttpOpen` / `HttpRead` / `HttpClose` で依頼し、認証情報と通信の認可はホスト側で管理する設計です。
プラグイン provider の通信は同一 origin に限定し、Generate / ConnectionCheck は
`POST <endpoint>/chat/completions` または `POST <endpoint>/responses`、
ListModels は `GET <endpoint>/models` だけを許可します。公開カタログの取得は
ホスト専用の catalog transport が行います。
`service::<T>` は応答を直接 `T` に復号します。戻り値が不要な操作は `service::<()>`、未保存ならnullを返す状態読取は `service::<Option<T>>` を使います。`http_close` の戻り値は `()` です。
SDKのヘルパーは `Result<T, extism_pdk::Error>` を返します。サービスの `Failure` はPDKエラーの原因として保持され、`error.downcast_ref::<Failure>()` でコードを取得できます。`FnResult` は `#[plugin_fn]` のエントリポイントで使い、ヘルパーのエラーは `?` で変換します。
Providerは生成ごとに新しいInstanceで実行します。Speechはbinding単位でInstanceを再利用し、再利用前に `ene_initialize` を再実行します。`ene_initialize` は再入可能にし、guest globalへ前回リクエスト固有の会話・音声・認証情報を残さないでください。
ハーネスはACP v1のエージェント側セッションとして扱います。詳細は[タスクハーネス](#タスクハーネスacp-v1)を参照してください。
`Log` サービスは認可後、plugin/provider IDを添えてホストの標準エラー出力へ記録します。メッセージは4096バイトまでで、改行・制御文字はエスケープします。
manifestの参照ファイル検証と入出力のサイズ制限も実行基盤の責務です。
子プロセスの起動やworker用IPCはありません。
現在のWasmサンプルはExtismの関数呼び出しと `ene_service` / `ene_emit` を使います。

## タスクハーネス（ACP v1）

`harness` 拡張は、ACP（Agent Client Protocol）v1のエージェント側セッションをWasm ABI向けのサブセットとして実装します。1タスク＝1セッション＝1プロンプトターンで、ホストがACPクライアント相当の制御（権限相談、キャンセル、報告の永続化）を持ちます。

| ene v4 | ACP v1 | 補足 |
| --- | --- | --- |
| `HarnessInput::Start { prompt }` | `session/new` + `session/prompt` | eneではセッション生成と最初のターンを同時に行う |
| `HarnessInput::Respond { request_id, outcome }` | `session/request_permission` の応答 | `RequestPermissionOutcome` は`selected` / `cancelled` |
| `HarnessInput::Cancel` | `session/cancel` 通知 | ホストが消費し、ゲストは再開しない |
| `HarnessEvent::Update { update }` | `session/update` の`update` | `agent_message_chunk` / `agent_thought_chunk` / `tool_call` / `tool_call_update` / `plan` |
| `HarnessEvent::RequestPermission` | `session/request_permission` のパラメーター | 呼び出しの最後のイベントにする |
| `HarnessEvent::Stop { stopReason }` | `session/prompt` の応答 | `end_turn` / `max_tokens` / `max_turn_requests` / `refusal` / `cancelled` |

### モデル推論（`models` サービス）

entrypointの`uses`に`models`を宣言し、バインディングでも`models`を許可した場合だけ、ゲストはホスト経由でタスク用のモデルを呼べます。

```rust
let reply = service::<ModelResponse>(ServiceRequest::Models {
    request: GenerationRequest {
        endpoint: String::new(),      // ホストが上書きする
        options: json!({}),           // ホストが上書きする
        model: String::new(),         // ホストが上書きする
        preamble: "…".into(),
        messages: vec![provider::ChatMessage { role: "user".into(), text: "…".into() }],
        max_tokens: 512,
        tools: vec![],
    },
})?;
```

ホストはコンパニオンの`model_assignments.task`とホスト設定の接続・モデルを使い、`purpose: "task"`・`data_kinds: ["task"]`のデータ送信Grantを検証します。`endpoint`・`options`・`model`・`tools`は無視され、`max_tokens`はホスト上限に丸められます。応答は`ModelResponse { text }`です。タスク割当が未設定なら`model_unset`、Grantがなければ`permission_required`、上限超過は`output_limit`で失敗します。

### 外部ACPエージェント（`acp` entrypoint）

Wasmを使わず、外部のACPエージェントを子プロセスとしてハーネスにできます。

```json
{
  "id": "main",
  "target": "acp",
  "provides": ["harness"],
  "command": ["/usr/local/bin/my-agent", "--acp"]
}
```

- `file`は空にし、`command`は実行ファイルと引数の配列にします。シェルを介しません。
- バインディングには`workspace_root`（絶対パス）が必要で、`services`は空にします（ACPエージェントはホストサービスを使いません）。
- ホストは`initialize`で`fs`のread/writeをadvertiseし、`terminal`はバインディングのポリシーが有効な場合だけadvertiseします。`session/prompt`・`session/update`・`session/request_permission`・`session/cancel`をジャーナルへ写像します。
- 環境変数は`PATH`・`HOME`・`USERPROFILE`・`LANG`・`LC_ALL`・`TERM`・`SYSTEMROOT`だけを渡し、資格情報は渡しません。作業ディレクトリは`workspace_root`です。
- `session/request_permission`は`Respond`で応答し、同じプロンプトターンを継続します。Cancel・無効化・再バインド・終了ではプロセスを停止し、ジャーナルを終端化します（キャンセル応答の待ち合わせはしません）。
- パッケージはWasmコードを持たず、revisionはmanifestと資産だけから計算されます。ACP entrypointはこのホストでのみ利用でき、旧ホストはinvalid manifestとして拒否します。

### 端末（ACP `terminal` capability）

ACPエージェントには、バインディングの`terminal`ポリシーで明示的にオプトインした場合だけ端末を許可します。

```json
"terminal": {
  "enabled": true,
  "allowed_commands": ["git", "rg", "cargo"],
  "max_seconds": 60,
  "max_output_bytes": 262144
}
```

- 未設定または`enabled: false`では`initialize`で`terminal: false`を通知し、`terminal/*`は`-32601`で拒否します。
- `allowed_commands`は実行ファイル名または絶対パスの完全一致で照合します（シェルは介しません）。空・制御文字・64件超はバインド時に拒否します。
- 実行ごとに、直前の`session/request_permission`でユーザーがallowを選んだ場合だけコマンドを受け付けます（allowの直後の1回で消費）。`cwd`は`workspace_root`配下に限定し、シンボリックリンクや脱出を拒否します。
- 出力は`max_output_bytes`まで保持してtruncateし、`max_seconds`で強制終了します。`terminal/output`・`wait_for_exit`・`kill`・`release`に対応します。
- **サンドボックスはありません。** コマンドはホストのOSユーザー権限で直接実行されます。コンテナ・seccomp・ネットワーク制限は提供しません。信頼できるエージェントとコマンドだけを許可してください。

### ワークスペースツール（`tools` サービス）

entrypointの`uses`に`tools`を宣言し、バインディングで`tools`を許可し、`workspace_root`（絶対パス）を設定した場合だけ、次のホストツールを呼べます。

| ツール | 引数 | 戻り値 |
| --- | --- | --- |
| `fs.read_text_file` | `path`、任意の`line`・`limit` | `{"content":"…","truncated":bool}` |
| `fs.write_text_file` | `path`、`content` | `{"ok":true}` |

- `path`はワークスペースルートからの相対パスのみです。絶対パス、`..`、`.`、シンボリックリンク、通常ファイル以外は拒否します。
- 読み取り・書き込みとも256KiB、行数上限は10,000です。親ディレクトリは自動作成せず、書き込みは同一ディレクトリの一時ファイルからのatomic renameです。
- `workspace_root`はバインド時にcanonical化され、ファイルシステムのルート（`/`やドライブ直下）は拒否されます。
- ルート未設定・`tools`未許可は`permission_required`、未知のツールは`unsupported_tool`、ルート外・シンボリックリンクは`permission_required`です。端末実行は含みません。

### 逸脱と制約

- `sessionId` はホストが所有し、ペイロードには含めません。ホストのジャーナルに保持し、ACP通知を外へ出す境界で付与します（通知APIは未実装です）。
- コンテンツは`text`のみです。`_meta`などの未知フィールドは無視しますが、未知の`sessionUpdate`は拒否します。
- `request_permission`は1回の呼び出しの最後のイベントです。ホストはジャーナルを保存して呼び出しを終え、`Respond`でゲストを再開します。Wasmは呼び出し中にブロックできないための表現上の違いです。
- キャンセルはゲストへ`cancelled`応答を届けず、ターンを終端化してインスタンスを破棄します。保存に失敗した場合も、再送は保存だけを再試行しゲストを再開しません。
- `initialize`のクライアント能力折衝、`session/load`・`resume`・`list`・`delete`、MCPサーバー、モード・設定オプション、text以外のコンテンツは未対応です。能力はmanifestの`provides`/`uses`とサービス宣言で表します。`fs/*`はバインディングのworkspace root、`terminal/*`はterminalポリシーで明示オプトインした場合だけ利用できます。
- モデル推論とツール実行はターンのイベントから外しました。モデルはホストの`models`サービス（下記）、ツールはホストサービスまたはクライアント能力（未実装）を使います。旧`ModelRequest` / `ToolRequest` / `Question` / `Answer` / `Result`は廃止しました。
- 最終報告は、最後の`messageId`の`agent_message_chunk`本文の連結です。`messageId`を省略した場合は単一メッセージとして扱います。
- 停止理由はホストが状態へ変換します。`end_turn`は`completed`、`cancelled`は`cancelled`、`refusal` / `max_tokens` / `max_turn_requests`は`failed`です。
- 制限値: 権限オプションは1〜8件、IDは128文字、タイトルは256文字、パスは4096文字、プランは512件、報告は256KiB、ジャーナルの履歴は最新256件です。
- `ToolCall` / `ToolCallUpdate` / `Plan` / `PermissionOption` は表示と進行のためのメタデータです。ツール名・パス・`session/request_permission`の選択結果を認可の根拠にせず、ホストの許可判定とサービスallowlistで実行直前に検証してください。

## 他アプリのウィンドウイベント

購読はentrypointの `window_events` に宣言します。

```json
{
  "id": "character",
  "target": "desktop",
  "file": "reaction.wasm",
  "provides": ["character"],
  "uses": ["window_events"],
  "window_events": [{
    "applications": ["Code.exe"],
    "events": ["focused"],
    "include_title": false
  }]
}
```

`applications` は端末アダプターが示すアプリ識別子の完全一致です。タイトルの部分一致や
ワイルドカードではありません。空配列は購読なしです。複数の購読はOR条件、同じ購読内の
アプリ条件とイベント条件はAND条件で、同一バインディングへの重複配送はしません。

`WindowEventKind` は出現・終了・フォーカス取得／喪失・移動・サイズ変更・最小化・復元を表します。
アプリ内部のボタンクリックやファイル保存は対象外です。
購読開始前の状態の再送は要求せず、開始後の変化を通知します。

イベントは `Operation::Character { target, event: CharacterEvent::Window { event } }` で渡します。
`WindowId` の `device_id`、`generation`、`id` を合わせて識別し、ネイティブハンドルは渡しません。
同じ接続世代内で終了済みウィンドウのIDを再利用しないことをアダプターに要求します。
`sequence` は端末の接続世代内で単調増加し、配送側が古い世代・重複・逆順の通知を破棄します。
購読による絞り込みで番号が飛ぶことは正常です。切断・権限取消・無効化時は購読と待機中の配送を停止します。

`WindowInfo.bounds` はデスクトップ全体を基準とする物理ピクセルです。左側のモニター等では負座標になり得ます。
ene-avatarの論理座標へそのまま渡さず、対象モニターの原点とDPIを使って変換します。
取得不能な座標・タイトルは `None`（JSONでは省略）です。終了イベントは最後に確認した情報でも構いません。

`DesktopCapabilities.windows` は実際に取得できるイベント、タイトル、座標の有無を示します。
未指定時はすべて未対応です。OS名だけで対応を仮定せず、能力情報を用いて購読可否を判断します。
Wayland等で非対応の場合は明示的に利用不可とし、黙って別の監視手段へ切り替えません。

## 動作の対象と認可

`CharacterTarget { device_id, character_id }` で動作対象を明示します。
`Operation::Character`、`Event::Character`、`ServiceRequest::Character`、`DesktopOperation::Avatar`
が同じ対象型を使います。拡張の出力は `emit(Event::Character { target, action })`、
ホストへ依頼して結果・失敗を受け取る場合は `service(ServiceRequest::Character { target, action })`、
端末サービスとしての操作は `ServiceRequest::Desktop` 内の `DesktopOperation::Avatar` を使います。
通知・要求応答・端末サービスの契約は異なります。実際の動作の実装まで三重に持つ意味ではありません。
表情・モーション・発話の定義があっても、レンダラーがすべてを実行できるという意味ではありません。

実行基盤は次を検証する必要があります。このクレートやSDKが自動で強制するものではありません。

- `window_events` は `target: desktop`、`provides: character`、`uses: window_events` の入口に限定する。
- manifestの宣言は要求であり許可ではない。端末とバインディングに対する許可を別途検証する。
- アプリ・イベントの購読条件と実際のOS能力を検証してから配送する。タイトルは要求と許可の両方がある場合だけ渡す。
- `target` は許可されたバインディングから決める。返された対象も照合し、別端末・別キャラクターへの操作を拒否する。
- 世代・呼び出しの有効性を確認し、終了済み・取り消し済みの呼び出しからの動作を実行しない。
- モーションのassetは検証済みmanifest内の資産へ解決し、任意ホストパスとして開かない。
- 移動等の大量イベントに対するキュー上限・集約は実行基盤で設ける。

ウィンドウを見る権限は `window_events` です。クリップボードや画面取得の権限を含みません。
ホストサービス経由では `character_actions` または `desktop` の許可も確認します。
キャラクター拡張の結果としての動作は、許可されたバインディングの対象に限ります。

## バージョンと移行

`CONTRACT_VERSION` は **4** です。manifestと拡張APIの共通互換性番号で、
プラグイン自身の `Manifest.version` とは独立しています。実行基盤は読込時に一致を検証してください。

バージョン4では `harness` の入力とイベントをACP v1の形へ変更しました。
`HarnessInput::{Start, Respond, Cancel}` と `HarnessEvent::{Update, RequestPermission, Stop}`、
およびACP型（`SessionUpdate`、`ToolCall`、`ToolCallUpdate`、`Plan`、`PermissionOption`、
`RequestPermissionOutcome`、`StopReason`など）を使います。
`instruction`は`prompt`（textコンテンツブロック）へ、`Answer`と`Result`は`Respond`へ、
`Progress` / `Completed` / `Failed` / `Question` / `ModelRequest` / `ToolRequest`は廃止しました。
旧manifestは受け付けません。`contract_version` を4に更新し、ハーネスの実装を移行してください。
同じWasmを複数の入口から参照できます。公開関数名は維持しています。

manifestの変更でrevisionも変わるため、更新後は再有効化とバインディングの再設定が必要です。
旧revisionを自動承認する処理や保存データの削除は行いません。
型の定義と実行機能の完成は別です。未実装の機能を利用可能と扱わないでください。

モデル情報は引き続きprotobuf JSON形式です。メタデータのフィールド名はcamelCase、
64ビット整数は文字列、任意のリストは `{"values": [...]}` です。
ウィンドウ契約は通常のSerde JSONで、フィールド名はsnake_case、整数はJSON数値です。
JavaScriptでも扱えるよう、実行基盤は世代・シーケンス値を安全な整数範囲内に保ってください。

## サンプルと検証

[ウィンドウ反応サンプル](../../examples/plugins/window-reaction-wasm/) は模擬入力から
実際のWasm・SDKを通じて対象付き表情イベントを返します。Extismの結合テストをCIで実行します。
[OpenRouterサンプル](../../examples/plugins/openrouter-wasm/) はHTTPとストリーミングの例です。

```sh
cargo check --locked -p ene-plugin --all-targets --all-features
cargo clippy --locked -p ene-plugin -p ene-window-reaction-wasm -p ene-openrouter-wasm -p ene-reference-wasm --target wasm32-unknown-unknown -- -D warnings
```
