# ene-desktop

デスクトップクライアントのリファレンス実装です。[デスクトップクライアント設計](../../docs/design/desktop-client.md)が定めるプロセス構成を、GPUIのフローティングコンパニオンポッド（`CompanionPod`）と独立設定ウィンドウ（`SettingsStudio`）で提供します。

`ene-host` へConnect-RPCで接続し、自身が提供できる能力を宣言して、コンパニオンのアクティブ端末になります。会話・記憶・モデル・作業のオーケストレーションはホストにあり、このプロセスはそれらを持ちません。アバター描画は子プロセスの [`ene-avatar`](../ene-avatar/) に委ね、音声デバイスとOSのウィンドウ監視はこのプロセスが所有します。

```text
[ene-host] ◀── Connect-RPC ──▶ [ene-desktop(GPUI: Pod + Studio)] ◀── stdin/stdout JSON Lines ──▶ [ene-avatar]
```

## 起動

`just run` でホスト・`ene-avatar`・デスクトップクライアントをまとめて起動できます。別々の端末で起動する場合は、ホストを起動してから接続します。

```sh
just run

# または別の端末で個別に起動する場合
just run-host
just run-desktop
```

`ENE_HOST_CONNECTION` を設定しない場合は、ホストの既定のランタイムディレクトリにある `connection.json` を読みます。ホストは起動のたびにポートとトークンを書き直すため、このファイルは接続を試みるたびに読み直します。

環境変数:

| 変数 | 用途 |
| --- | --- |
| `ENE_HOST_CONNECTION` | ホストが公開した `connection.json` のパス |
| `ENE_DESKTOP_CLIENT_ID` | ホストに登録するクライアントID（既定 `desktop`）。再起動しても同じIDを使う |
| `ENE_DESKTOP_CLIENT_NAME` | ホストの一覧に出す表示名（既定 `Desktop`） |
| `ENE_DESKTOP_RENDERER` | `ene-avatar` の実行ファイル。未設定なら描画なしで動く |
| `ENE_DESKTOP_MODEL` | 表示するVRMモデル。未設定なら `just fetch-assets` が置く既定モデル |
| `ENE_DESKTOP_OPEN_SETTINGS` | 起動時に `SettingsStudio` を同時に開く指定（`1` / `companions` / `connections` / `device`） |

## 画面

画面右下に浮かぶ透明フレームレスウィンドウの `CompanionPod` を起点に、対話・初期設定・承認確認と独立した `SettingsStudio`（`設定 — ene`）ウィンドウを提供します。ポッドを閉じるとアプリ全体が終了し、設定ウィンドウだけ閉じてもポッドは動き続けます。

| 面 | 内容 |
| --- | --- |
| コンパニオンポッド (`CompanionPod`) | 透過背景のミニ対話モード（`444x218`）と履歴展開モード（`444x540`）を切替。非対称吹き出し・左上のステータスバッジ・初期設定（`1 AI接続 → 2 音声 → 3 対話開始`）・承認確認・下部のカプセル1行入力欄＋仲間切替・マイク切替・3つの独立円形ボタン（履歴 / 設定 / 閉じる）を配置 |
| 承認カード | 承認待ちはポッドのメイン吹き出しに最優先で表示。「「…」の実行を許可しますか？」の確認と、許可・常に許可・拒否の3操作 |
| コンパニオン (`Companions`) | 3項目の簡易新規作成（名前・人格の核・話し方）、62px行リスト、ヘアライン区切りの基本編集＋声（話者ID）設定、`MindProfile`（側面・セリフ例・背景・表現のクセ）の折りたたみ開示 |
| 接続 (`Connections`) | LLM / TTS / STT / 埋め込み / MCP / ACP の接続追加と62px単一行一覧、マスク済み資格情報・疎通確認・`options_schema` 編集・MCPツール検索フィルタ、および折りたたみ開示による7役割モデル割当の詳細設定 |
| この端末 (`Device`) | ホスト接続状態・対話の担当端末切替・マイク/スピーカー入力出力トグル・3Dアバターの表示サイズ（75〜150%）と中央戻し・公開機能タグ・許可ルールの一覧/手動追加・削除 |

承認を独立したタブにしないのは、承認カードをコンパニオンの窓口に集約し、内部のエージェントをユーザーに見せない、という設計に合わせるためです。

## 構成

- `main.rs`: 起動、ディスカバリのパス決定、Tokioランタイムとセッションスレッド、GPUIアプリ（埋め込みSVGアセット登録・透過 `CompanionPod` 起動・`ENE_DESKTOP_OPEN_SETTINGS` 対応）、終了時の順序。
- `session.rs`: 接続、登録、有効化、DeviceStream、音声ストリーム、再生報告、知覚、再接続。ローカル資源（アバター子プロセス、音声デバイス、感知器）の所有者。
- `state.rs`: ウィンドウが表示する状態と、通知を表示へ畳み込む規則。ホスト状態の表示用コピーはここに1つだけ置く。
- `ui/`: ウィンドウ群。`mod.rs` が共有エンティティ `Hub` とRPC発行、`pod.rs` が `CompanionPod`、`studio.rs` が `SettingsStudio` シェル、`companions.rs`・`connections.rs`・`device.rs` が各設定章、`theme.rs`・`icons.rs`・`widgets.rs` がストロベリーミルク・カプセルのトークン・埋め込みSVGアイコン・共通UI部品。
- `input.rs`: GPUIの `EntityInputHandler` による入力欄。日本語IMEの変換中は送信しません。
- `capabilities.rs`: この環境で実行できる能力だけを宣言する表。
- `avatar.rs`: `ene-avatar` の起動とJSON Lines。`set` は設定全体の置換なので、直前の設定を保持して変更分だけ差し替えます。
- `audio.rs`: 24kHzモノラルのワイヤー形式とデバイス形式の変換。継続ストリームは1つのリサンプラーを保持します。
- `perception.rs`: 前面ウィンドウとアイドルの観測、`eye.watch` の適用。
- `host.rs`: ディスカバリの読み込みと生成クライアントの構築。

## 状態とタスクの所有者

- ホストが正本: 設定、コンパニオン、承認、作業、クライアント、有効化、カーソル。ウィンドウは `Snapshot` を表示用に1部だけ持つ。
- ウィンドウが所有: 選択中のコンパニオン、履歴の表示、確定前のプレビュー、下書き、未送信の入力、通知。
- セッションが所有: 接続、有効化の世代、購読カーソル、DeviceStream、音声デバイス、アバター子プロセス、感知器、再生報告の状態。
- 境界を越えるのは2つのチャネルだけ: セッション→ウィンドウの `Update` と、ウィンドウ→セッションの `Command`。ウィンドウ自身の要求は生成クライアントを直接呼ぶ単発のfutureで、結果は同じ `Update` で戻る。

`MessageDelta` はホストの `stream_id` ごとのプレビューとして保持し、確定本文としては保存しません。確定は `MessageCreated` で届く `Message` だけです。

## 音声

- ワイヤー形式はホストの契約どおり 24kHz / 16-bit 符号付きリトルエンディアン / モノラル。デバイス形式は取得した設定から選び、24kHzの32-bit floatを優先し、32-bit floatが無い場合は16-bit PCM（`I16` / `U16`）へ変換します。変換が必要な場合だけ `rubato` を使い、ストリームごとに1つのリサンプラーを保ちます。
- 音声コールバックはRPC・ファイルI/O・待機構造を呼びません。再生側はサンプルを取り出して文の進捗を数えるだけ、録音側は事前に確保したリングへ `try_lock` で写すだけです。変換はセッションスレッドで行います。
- 同時に再生する発話は1つです。新しい発話のチャンクは旧発話を捨て、`FlushControl` の世代以下を破棄します。同じ文の複数チャンクは `sequence_no` が同じでも重複ではないため捨てません。
- `ReportPlaybackStatus` は実際にデバイスへ出た分だけを報告します。受信完了では報告せず、`UtteranceFinished` を受けた発話が実際に空になった時点で `COMPLETED`、Flush では `INTERRUPTED` を送ります。
- エコーキャンセルは使えないため、マイクの音量による自動バージョインは行いません。停止は `Esc` の明示操作だけです。再生中に収録した音声には `is_companion_speaking` を立て、ホストが承認の判定から除外できるようにします。

## アバターと知覚

- `ene-avatar` は別プロセスで、標準入出力のJSON Linesだけを使います。標準出力は常に読み続け、標準エラーはログとして扱い、応答とは混同しません。終了時は `quit` を送って待ちます。
- 知覚は Windows に加え、Linux（Wayland の `wlr-foreign-toplevel-management`＋`ext-idle-notify-v1`、または X11 の `_NET_ACTIVE_WINDOW`＋ScreenSaver）でも有効です。対応しない環境（GNOME/KDE 等）では `eye.watch` を宣言せず、取得できないウィンドウ情報を推測で補いません。

## 宣言する能力

実行できるものだけを宣言します。描画プロセスかモデルが無ければ `body.motion` は宣言せず、モーションのアセットが無いプリセットは入力スキーマのenumから外します。`body.motion` の入力は `{motion, expression?}` で、表情のenumには `neutral`（消す）を含みます。`granted_capabilities` は決して送らず、ホストの判断だけを表示します。

検証スクリプトは `verification/avatar-integration`（描画プロセスの契約）と `verification/desktop-generation-binding`（世代束縛の全数検査）。
## 音声合成（VOICEVOX）

設定の「音声読み上げ」から「VOICEVOX を追加」を押すと、ローカルの VOICEVOX ENGINE（`127.0.0.1:50021`、話者3）が TTS 接続として登録されます。エンジンは別途起動してください。

```powershell
& "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\HiroshibaKazuyuki.VOICEVOX.CPU_Microsoft.Winget.Source_8wekyb3d8bbwe\VOICEVOX\vv-engine\run.exe" --host 127.0.0.1 --port 50021
```

ホストはアシスタントの発話を文単位でこのエンジンへ送り、24kHz mono PCM に直して購読中のクライアントへ流します。クライアントは実際に再生した分を ReportPlaybackStatus で返し、その報告でホストが発話を閉じます。接続を追加・変更するとセッションは再接続し、音声ストリームを開き直します（開いたままだとフッターの「出力」が利用不可のままになります）。

音声の入出力は 32-bit float を優先し、非対応デバイスでは 16-bit PCM（`I16` / `U16`）へ変換します。マイクは常時取得しますが、自動バージョインはせず、停止はボタンと Esc の明示操作だけです。
## 検証

```sh
cargo test -p ene-desktop
cargo clippy -p ene-desktop --all-targets --all-features -- -D warnings
```

描画プロセスとの契約（表情・口パク・クロスフェード・静止）は `verification/avatar-integration/run.ps1` が実プロセスで検査します。実ホストに対する確認は、隔離したstateで `ene-host` を起動し、スクリプト化したOpenAI互換エンドポイントをモデルとして接続して行います。このクライアントのウィンドウへ実際にキー入力（日本語を含む）を送り、次の経路を確認済みです。

- 登録と有効化、宣言した能力の許可。
- 送信、モデル呼び出し、`body.motion` の DeviceStream 配送、アバター子プロセスへの `add`／`set`、`COMPLETED` の報告、確定メッセージの受信。
- ホスト停止からの再接続（1秒・2秒・4秒の待機）と、再起動後の再登録・再有効化。
- ウィンドウを閉じたときの終了: セッションが停止し、アバターへ `quit` が届き、子プロセスが残らない。

## 既知の制限

- リップシンクは、実際に再生した音声バッファのRMSを約60Hzで `lip_sync` として描画プロセスへ送ります（値が変わらない間は送りません）。`ene-avatar` が `aa` へ平滑化して反映し、再生が止まれば口も閉じます。
- 表情は `body.motion` の `expression` で指定します。`ene-avatar` の `expression` が `ene-vrm` の表情ウェイトを動かし、150〜200msでフェードします。`neutral` は表情を消します。モデル側が表情を指定しない限り顔は変わりません。
- 入力欄は1行です。IMEの変換中は送信せず、改行は貼り付け時に空白へ畳みます。長文はホストの上限（32,000文字）まで送れます。
- 音声出力と入力は32-bit float（または16-bit PCM `I16` / `U16`）のデバイス設定のみを扱います。他のサンプル形式しか無い場合は理由を表示して、その機能だけを無効にします。
- アバターは実プロセスで確認済みです。起動、`add` の受付、状態イベント、読み込み失敗の報告、終了時の `quit` を確認しました。実際のVRM/VRMAアセットはこの環境に無いため、描画そのものは未確認です。読み込み失敗は操作の失敗として返し、成功としては返しません。
- `CheckConnection` の結果はホストが通知しないため、接続確認の結果は呼び出した端末だけが表示します。

## 注意

GPUIは `comctl32!TaskDialogIndirect` をインポートします。これはマニフェストで要求したCommon-Controls 6にしか無いため、`build.rs` が `ene-desktop.rc` からアプリケーションマニフェストを埋め込みます。これが無い実行ファイルは `main` より前に `STATUS_ENTRYPOINT_NOT_FOUND` で落ちます。
