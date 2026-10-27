# ene-avatar

VRM拡張は[ene-vrm](../../crates/ene-vrm/README.md)で扱い、glTF・シーン・アニメーション・描画はBevyの標準APIを使います。

VRM 1.0モデルをモニターごとの透過ウィンドウへ描画します。
WindowsはBevy/winit、LinuxはWaylandのlayer-shellとxdg-outputを使います。
1つのプロセスで複数のキャラクターを扱います。

Windows・Linuxともに、少数のアバターを60 FPSで動かす際のCPU負荷を優先した共通設定です。
間接描画を使わず、照明のクラスタリングはCPU上の1区画にまとめ、Computeワーカーは1本に制限しています。
描画・スキニングは引き続きGPUで行い、アセット読み込みとパイプライン化された描画のスレッドは独立しています。

```sh
cargo run --release --locked -p ene-avatar
```

## 対話式 CLI の example

JSON を手で書かずに操作する Rust example です。本体を子プロセスとして起動します。

```sh
just fetch-assets
just run-avatar-cli
```

本体のビルドも自動で行います。Cargo から直接実行する場合は次のとおりです。

```sh
cargo build --release --locked -p ene-avatar
cargo run --release --locked -p ene-avatar --example control
```

```text
> add ene
> motion ene ./wave.vrma
> pause ene
> move ene 800 600
> size ene 120
> play ene
> loop ene off
> center ene
> list
> quit
```

`help` で全コマンドを確認できます。`list` はキャラクター・モニター・FPSを表示し、
読み込み完了やクリックなどの通知は操作中も表示します。
`motion ene none` でモーションを解除できます。
`add ene` は取得済みの Seed-san モデルと three-vrm サンプルモーションを使います。
`model ene` / `motion ene` でもそれぞれの既定アセットへ戻せます。
`add ene ./model.vrm ./wave.vrma` で両方を指定でき、
`add ene ./model.vrm none` ならモーションなしで追加します。
モデルだけ指定した場合も既定モーションを使います。
既定アセットは取得スクリプトと同じキャッシュから探します
（Linux: `$XDG_CACHE_HOME/ene` または `~/.cache/ene`、Windows: `%LOCALAPPDATA%/ene/cache`）。
未取得の場合は取得コマンドを案内します。
部分変更は毎回 `get` した設定を引き継いで `set` します。
そのため、位置・モニターは `get` が返した解決済みの値になります。

空白を含むパスや名前は `add ene "./my models/model.vrm"` のように引用符で囲みます。
バックスラッシュはそのまま扱い、シェルの変数展開などは行いません。
ファイルの相対パスは CLI の作業ディレクトリを基準に絶対パスへ変換します。

本体の実行ファイルは、既定では example と同じビルドプロファイルの出力先から探します。
別の実行ファイルを使う場合は、`--example control -- /path/to/ene-avatar` と指定します。
`quit` または標準入力の EOF で本体も終了します。

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
| `fps` | 全体の更新頻度を0～1000で指定する。既定値は60、0は制限なし |
| `remove` | 指定キャラクターとその子エンティティを削除する |
| `quit` | 描画を終了し、受理済みの出力を送信してプロセスを終了する |

`config`は次の項目を持ちます。`model`は`.vrm`、`motion`は`.vrma`のパスです。

```json
{"model":"/absolute/path/model.vrm","motion":null,"playing":true,"looping":true,"monitor":null,"position":null,"size_percent":100}
```

`position`はモニター左上から足元までの論理ピクセル座標`[x,y]`です。
`null`はモニター幅の50%、高さの80%に配置します。`size_percent`は50～200で、
100ではモデルの1mを320論理ピクセルとして描きます。
50を指定すると従来の100と同じ大きさになります。
未知のモニター名は最初の出力へフォールバックします。
`get`の設定には解決後のモニター名と位置が入ります。

照明は白色の平行光80%と環境光20%を基準に、カメラの露出と合わせて設定します。
MToonの拡散反射はthree-vrmと同じく平行光の照度をπで割り、
線形出力からsRGBへ変換することで、肌や白い服の色・陰影を保ちます。

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

## 構成

- `character.rs`: 1体分の設定・状態・アセットを`Character`へまとめ、Bevyのシーン生成完了通知でモーションを接続する。
- `control.rs`: JSON Linesの読み書きと操作。受信した操作は順番にその場で反映する。
- `desktop/`: モニター配置、カメラ、描画境界による入力領域、ドラッグ。
- `platform/`: WaylandとWindowsのウィンドウ・入力・更新頻度の調整。

## 検証

```sh
cargo test --locked -p ene-avatar
cargo clippy --locked -p ene-avatar --all-targets --all-features -- -D warnings
```

テストは入力制限、操作順序と不正な変更時の状態保全、応答の出力・終了処理、
モニター間ドラッグと削除・画面切断時の後始末を確認します。Windowsの入力順序と出力管理は
Linuxでも検査し、ネイティブAPIと実際の透過表示は対象OS上で確認します。
