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

## 共通契約

| 定義 | 用途 |
| --- | --- |
| `Manifest` / `EntryPoint` | 実行入口、提供する拡張、利用するサービス |
| `Operation` | ホストからプラグインへの依頼 |
| `Event` | プラグインからの結果・途中経過 |
| `ServiceRequest` | プラグインからホストへの機能利用依頼 |
| `Failure` | ホストサービス呼び出しの失敗 |

実行方式はExtism固定で、manifestに `runtime` は指定しません。Luauスクリプトは受け付けません。WasmのABIはExtismが担当します。
共通型はクレート直下から利用できます。内部ファイルは `manifest.rs`、`api.rs`、`window.rs`、`failure.rs`、`sdk.rs` です。
`provider` は認証方式、ルーティング設定、チャットメッセージを提供します。
HTTPは `HttpOpen` / `HttpRead` / `HttpClose` で依頼し、認証情報と通信の認可はホスト側で管理する設計です。
manifestの参照ファイル検証と入出力のサイズ制限も実行基盤の責務です。
子プロセスの起動やworker用IPCはありません。
現在のWasmサンプルはExtismの関数呼び出しと `ene_service` / `ene_emit` を使います。

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

`CONTRACT_VERSION` は **3** です。manifestと拡張APIの共通互換性番号で、
プラグイン自身の `Manifest.version` とは独立しています。実行基盤は読込時に一致を検証してください。

バージョン3ではmanifestの `runtime` と `code_files` を削除しました。実行コードは
`entrypoints[].file` の重複を除いた一覧から読み込み、サイズ制限とrevision計算に含めます。
同じWasmを複数の入口から参照できます。操作・イベントの形と公開関数名は維持しています。
旧manifestは受け付けません。`contract_version` を3にして不要なフィールドを削除してください。

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
cargo clippy --locked -p ene-plugin -p ene-window-reaction-wasm --all-targets --all-features -- -D warnings
```
