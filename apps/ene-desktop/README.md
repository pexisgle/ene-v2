# ene-desktop

デスクトップクライアントのリファレンス実装です。[デスクトップクライアント設計](../../docs/design/desktop-client.md)が定めるプロセス構成を、実際に動く1つのプロセスとして提供します。

`ene-host` へConnect-RPCで接続し、自身が提供できる能力を宣言して、コンパニオンのアクティブ端末になります。アバター描画は子プロセスの [`ene-avatar`](../ene-avatar/) に委ね、音声デバイスとOSのウィンドウ監視はこのプロセスが所有します。会話・記憶・モデルはホストにあり、このプロセスはそれらを持ちません。

```text
[ene-host] ◀── Connect-RPC ──▶ [ene-desktop] ── stdin/stdout JSON Lines ──▶ [ene-avatar]
```

## 起動

ホストを起動し、公開された `connection.json` を指して起動します。

```sh
just run-host
# 別の端末で
cargo run --locked -p ene-desktop
```

`ENE_HOST_CONNECTION` を設定しない場合は、ホストの既定のランタイムディレクトリにある `connection.json` を読みます。

環境変数:

| 変数 | 用途 |
| --- | --- |
| `ENE_HOST_CONNECTION` | ホストが公開した `connection.json` のパス |
| `ENE_DESKTOP_CLIENT_ID` | ホストに登録するクライアントID（既定 `desktop`）。再起動しても同じIDを使う |
| `ENE_DESKTOP_CLIENT_NAME` | ホストの一覧に出す表示名（既定 `Desktop`） |
| `ENE_DESKTOP_RENDERER` | `ene-avatar` の実行ファイル。未設定なら描画なしで動く |
| `ENE_DESKTOP_MODEL` | 表示するVRMモデル。未設定なら `just fetch-assets` が置く既定モデル |

## 宣言する能力

ホストは能力名・入力スキーマ・プリセットだけを保持し、実装を解釈しません。宣言は [`src/capabilities.rs`](src/capabilities.rs) の表がすべてです。

| capability | tool_name | 入力 | approval |
| --- | --- | --- | --- |
| `body.motion` | `body_motion` | `{motion, expression?}` | `AUTO` |
| `eye.watch` | `set_eye_watch` | `{target_pattern, match_kind, notify_on, enabled}` | `AUTO` |

ホストが許可した能力だけがモデル向けのツールとして公開され、操作は `ClientService.DeviceStream` の `DeviceOperation` として届きます。許可は `ENE_HOST_ALLOWED_CAPABILITIES` で決まります（未設定なら何も許可されません）。

## 構成

- `host.rs`: ディスカバリの読み込みと、生成された各サービスクライアントの構築。
- `client.rs`: 登録・有効化・世代の保持、届いた操作の適用と結果の報告、知覚の送信、音声の再生。
- `capabilities.rs`: 宣言する能力と、そのJSON Schema・プリセット。
- `avatar.rs`: `ene-avatar` の起動とJSON Linesでの操作、口パク振幅の一方向通知。
- `audio.rs`: `cpal` による入出力、`rubato` による24kHzモノラルとの相互変換、単一再生とFlushの世代破棄。
- `perception.rs`: 前面ウィンドウとアイドルの観測、`eye.watch` ルールの適用と優先割り込み。

## 動作の要点

- **世代**: 有効化の `revision` を保持し、知覚・音声・操作結果に付与します。別端末へ移動した後の旧世代のデータはホストが破棄します。
- **単一再生**: 同時に鳴らす発話は常に1件です。新しい発話のチャンクが届いたら旧音声を捨て、`FlushControl` の世代以下を破棄します。
- **口パク**: 再生中のPCMから求めたRMSを、応答を待たない一方向通知として `ene-avatar` へ送ります。
- **知覚**: 20秒ごとに前面ウィンドウとアイドルを観測します。`eye.watch` に合致した遷移では、定期サンプルを待たずに優先割り込みを送ります。
- **縮退**: 描画・音声出力・マイクのいずれかが無い環境でも起動し、残りの能力で動きます。取得できないウィンドウ情報を推測で補いません。

## 検証

```sh
cargo test --locked -p ene-desktop
cargo clippy --locked -p ene-desktop --all-targets --all-features -- -D warnings
```

単体テストは、能力宣言のスキーマ整合、`eye.watch` の一致と遷移、PCMの往復変換とリサンプリング、レンダラの応答と通知の区別を確認します。

実ホストに対する結合確認は、[ene-hostの検証手順](../../.agents/skills/verify-ene-host/SKILL.md)と同じ形で行います。登録・有効化・`DeviceStream` の開設、モデルが `body_motion` を呼んだ際の操作の受領と `GetOperation` での完了確認までを、実際の `ene-host` とこのバイナリで確認済みです。描画を伴う確認には `ene-avatar` のビルドとVRMアセットが必要です。
