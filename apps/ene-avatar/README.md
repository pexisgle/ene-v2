# ene-avatar

VRM拡張は[ene-vrm](../../crates/ene-vrm/README.md)で扱い、glTF・シーン・アニメーション・描画はBevyの標準APIを使います。

VRM 1.0モデルをモニターごとの透過ウィンドウへ描画します。
WindowsはBevy/winit、LinuxはWaylandのlayer-shellとxdg-outputを使います。
1つのプロセスで複数のキャラクターを扱います。

```sh
cargo run --release --locked -p ene-avatar
```

## JSON Linesによる操作

標準入力へ1行につき1つのJSONを送り、標準出力を継続して読み取ってください。
ログは標準エラーへ出ます。モデルとモーションには絶対パスを推奨します。
従来のRemoc接続は廃止しました。

```json
{"id":1,"op":"add","character":"ene","config":{"model":"/absolute/path/model.vrm"}}
{"id":2,"op":"get"}
{"id":3,"op":"set","character":"ene","config":{"model":"/absolute/path/model.vrm","motion":"/absolute/path/motion.vrma","playing":false}}
{"id":4,"op":"center","character":"ene","monitor":null}
{"id":5,"op":"fps","value":60}
{"id":6,"op":"remove","character":"ene"}
{"id":7,"op":"quit"}
```

Windowsのパスは`C:/models/model.vrm`のように書けます。
`character`は空でない128バイト以内の文字列で、追加時の重複は拒否します。

| 操作 | 内容 |
| --- | --- |
| `add` | IDを登録し、`config`のモデルとモーションを非同期で読み込む |
| `set` | 設定全体を置き換える。省略した項目は既定値に戻る |
| `get` | キャラクターの状態・設定・再生時刻、モニター一覧、FPS設定を返す |
| `center` | 指定モニターの既定位置へ戻す。`null`は最初の出力を使う |
| `fps` | 全体の更新頻度を0～1000で指定する。既定値は30、0は制限なし |
| `remove` | 指定キャラクターとその子エンティティを削除する |
| `quit` | 描画を終了し、受理済みの出力を送信してプロセスを終了する |

`config`は次の項目を持ちます。`model`は`.vrm`、`motion`は`.vrma`のパスです。

```json
{"model":"/absolute/path/model.vrm","motion":null,"playing":true,"looping":true,"monitor":null,"position":null,"size_percent":100}
```

`position`はモニター左上から足元までの論理ピクセル座標`[x,y]`です。
`null`はモニター幅の50%、高さの80%に配置します。`size_percent`は50～200で、
100ではモデルの1mを160論理ピクセルとして描きます。
未知のモニター名は最初の出力へフォールバックします。
`get`の設定には解決後のモニター名と位置が入ります。

`playing`はVRMAの再生とSpringBoneの進行を制御します。停止中は最後にシミュレーションしたSpringBoneの姿勢を保持し、再開すると続きから進みます。MToonのUVアニメーションは材質のグローバル時計で進むため、`playing=false`でも継続します。

## 応答と通知

成功は`{"id":1,"ok":null}`、拒否は`{"id":1,"error":"理由"}`です。
`get`の`ok`には`characters`、`monitors`、`fps`が入ります。
読み込み開始の成功は描画完了を意味しません。状態は通知または`get`で確認します。

```json
{"event":"state","character":"ene","status":"ready","error":null}
{"event":"state","character":"ene","status":"error","error":"読み込み失敗の理由"}
{"event":"finished","character":"ene"}
{"event":"click","character":"ene"}
```

状態は`loading`、`ready`、`suspended`（出力なし）、`error`です。
失敗したファイルを修復した後は同じ設定を`set`すると再試行します。
`finished`はループしないモーションの終了、`click`はドラッグせずにクリックした通知です。

入力は1行64KiB以内です。不正なJSONなど要求IDを取得できない入力のエラーは`id:0`で返します。
標準入力のEOFでも終了します。終了時も標準出力を読み続けてください。
出力キューが満杯または切断された場合は失敗終了します。
終了時の送信待ちは最大2秒で、送信失敗・タイムアウトも失敗終了として返します。

## 検証

```sh
cargo test --locked -p ene-avatar
cargo clippy --locked -p ene-avatar --all-targets --all-features -- -D warnings
```
