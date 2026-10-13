# ene-avatar 操作用コンソール

`ene-avatar` はVRM 1.0／VRMAを描画する子プロセスです。PR2の段階では `ene-desktop` は骨組みなので、開発用の対話CLIで実際のRemoc IPCを操作します。LinuxではWayland layer-shellとVulkan、WindowsではDX12を利用できるデスクトップセッションが必要です。

## 起動

リポジトリのルートで実行します。Nixを使わない場合は固定nightlyとOSの開発ライブラリを用意してください。

```sh
nix develop
just fetch-assets
just avatar-console
```

`avatar-console` は描画バイナリをビルドし、同じビルド先にある `ene-avatar` を起動するコンソールを実行します。最初は空のアバター一覧です。`add` だけで取得済みのSeed-sanを追加できます。独自のモデル・モーションはパスを指定してください。Linuxの既定のキャッシュ先は `~/.cache/ene`、Windowsは `%LOCALAPPDATA%/ene/cache` です。

独自の描画バイナリを指定する場合:

```sh
cargo run --locked -p ene-avatar --example renderer-console -- --renderer /absolute/path/to/ene-avatar
```

ログを増やす場合は `RUST_LOG=info just avatar-console` で起動します。コンソールは描画プロセスのstderrをそのまま表示し、stdoutはIPC専用にします。

## 操作

コマンドは**コンソール内**に入力します。次の例は `just fetch-assets` で取得したサンプルを使います。`~/`（Windowsでは `~\` も）をホームディレクトリに展開できます。シェル変数とワイルドカードは展開しません。空白を含むパスは引用符で囲み、Windowsのパスは `'C:\Models\Seed san.vrm'` のように単引用符で囲むと確実です。

```text
add
list
motion 1 sample
pause 1
play 1
loop 1 off
size 1 150
fps 60
recenter 1
state
motion-clear 1
remove 1
quit
```

| コマンド | 操作 |
| --- | --- |
| `add [MODEL] [--monitor NAME]` | 専用ウィンドウを追加。省略時は取得済みSeed-san。IDは1から増加し、削除後も再利用しない |
| `remove ID` | 指定したアバターを削除 |
| `list` | 描画側が管理しているID・状態・モニター・位置を一覧表示。送信済みIDとの食い違いも表示 |
| `model ID PATH` | モデルを変更。他の設定を保持 |
| `motion ID PATH` / `motion-clear ID` | モーションを変更／解除。他の設定を保持 |
| `play ID` / `pause ID` | 再生／一時停止 |
| `loop ID on\|off` | 繰り返しを変更 |
| `size ID PERCENT` | サイズを50〜200%で変更 |
| `fps VALUE` | 全体のFPS上限を変更。0はアプリ側の上限を解除 |
| `recenter ID` | 現在のモニター中央へ移動 |
| `state` / `help` / `quit` | 状態表示／操作案内／終了 |

モデルの短縮名は `seed`、モーションは `sample`／`dance`／`walk`／`slow-run` です。例えば `model 1 seed`、`motion 1 walk` で選択できます。サンプルの取得先は `XDG_CACHE_HOME`／`LOCALAPPDATA` の設定にも従います。ファイルは自動ダウンロードしないので、先に `just fetch-assets` を実行してください。同名の自作ファイルを指定する場合は `./walk` のようにパスを明示します。

```text
model 1 "~/Models/自分のモデル.vrm"
motion 1 walk
```

再生中に `loop 1 off` にすると、現在の再生が終わるまで待ちます。停止・終了後の再生挙動も `play`／`pause` と報告状態で確認してください。モニター指定にはOSの出力名（例: `DP-1`）を使います。稼働中のモニター変更はCLIコマンドでは提供せず、OSの表示設定変更やモニター切断で確認します。

### 出力の読み方

`list` は描画IPCの最新 `RendererState.avatars` を参照します。読み込み中・エラー状態のアバターも含み、ID順に表示します。`REGISTRY` が `both` なら描画側と送信済み設定の両方にIDがあり、`renderer-only`／`sent-only` なら片方だけにあります。追加・削除の反映中は一時的に食い違う場合があります。現在のIPCは各アバターの適用済み設定を返さないため、モデルパス・サイズなどの設定値が一致しているかは判定しません。詳細やエラー内容は `state` で確認してください。

- `[sent request]` と `[sent avatar ID]` は最後に送った要求・設定です。描画側での成功を意味しません。存在しないファイルも送信できるため、読み込み失敗と復旧を試せます。
- `[renderer avatar ID]` の自動通知は、読み込み状態・モーション・モニター・位置・エラーを1行で表示します。モデルの利用条件は変更時に `[model avatar ID]` で表示します。`state` で全状態と送信済み設定の詳細を確認できます。状態通知は集約されるため、すべての中間状態が表示されるとは限りません。
- `[event avatar ID]` は順序付きの通知です。`MotionFinished`、クリック時の `ShowChat`、リソース解放後の `AvatarRemoved` を表示します。`ShowChat` は通知の確認だけで、コンソールからチャット画面は開きません。
- `[error avatar ID]`／`[renderer error]` はIPCのエラー通知、`[command error]` は入力・設定値の検証エラーです。

入力待ち中も通知を表示するため、プロンプトと通知が同じ行に混ざることがあります。入力内容を編集する高度な行エディタはありません。

接続・要求送信の待ち時間は5秒です。これはモデル読み込みの完了待ちとは別で、描画側の表示タイムアウトは90秒です。`quit`、EOF（Linuxでは空行でCtrl+D）、Ctrl+Cで `Shutdown` を送ります。送信から終了まで5秒を超えた場合や送信できない場合は子プロセスを停止し、エラー終了します。通信切断・描画プロセス終了もコンソールに表示します。

## 実機チェックリスト

描画側の状態と目視結果を両方記録します。`loading: false` は描画処理の準備完了を確認できますが、見た目やクリック透過の確認は目視・マウス操作が必要です。

| 確認項目 | 手順と期待結果 |
| --- | --- |
| モデル表示・利用条件 | `add` して表示を待つ。背景が透明で、最新状態の `loading` がfalse、`error` がNone、`model` に利用条件がある |
| モーション切替 | サンプル、ダンス、歩行を順に指定。動作と `Playing` を確認 |
| 一時停止・再開 | `pause`／`play` で動作と `Paused`／`Playing` が一致 |
| 繰り返し・単発 | `loop on` で繰り返し、`loop off` で `Finished` と `MotionFinished` 通知を確認 |
| モーション解除 | `motion-clear` で動作が解除され、`None` になる |
| サイズ・FPS | サイズ50／100／200%で表示を確認。FPS変更後に全体の状態が更新される。実際のFPSは状態値だけで断定しない |
| 入力・位置 | ドラッグ後に `placement` が変化。`recenter` 後に現在のモニター中央へ移動 |
| クリック透過 | 透明部分をクリックして背後のアプリを操作できる。キャラクターをドラッグせずクリックすると `ShowChat` が届く |
| 複数体の独立性 | 2体追加し、片方の設定変更・削除中も他方の表示・再生・位置が維持される |
| モデル読み込み失敗・復旧 | `model ID /missing.vrm` でエラーを確認。有効なモデルへ戻して表示・状態が復旧 |
| モーション読み込み失敗・復旧 | 存在しないモーションで `Failed` を確認。有効なモーションへ戻して再生が復旧 |
| モニター・表示設定 | `add MODEL --monitor NAME` の表示先を確認。表示倍率変更・モニター切断後も表示と操作を確認 |
| 終了 | `quit`、EOF、Ctrl+Cを別々の起動で確認。終了後にウィンドウ・描画プロセスが残らない |

Windowsの描画・入力・DirectCompositionはWindows実機で別途確認します。既存のDX12明示テストは通常のテスト実行では無視されます。

## 自動検証

```sh
cargo test --locked -p ene-avatar --example renderer-console
cargo test --locked -p ene-avatar --test ipc
just verify
```

exampleのテストはワークスペースの `just test` でも実行されます。引用符付きパス、サンプル短縮名・ホームパスの展開、短い状態表示、個別変更による他の設定・他のアバターの保持、入力値の検証、IDを再利用しないことを検証します。既存IPCテストはWayland接続を意図的に失敗させてライフサイクルを確認するもので、描画成功を保証しません。

## レビュー記録

確認日、OS／コンポジター、GPU、表示倍率・モニター構成、対象コミット、操作、報告状態、目視結果を記録してください。未実施は合格扱いにしません。

2026-10-13のローカル確認: PR2の `0924b70` にこのCLIを追加した作業ツリー、NixOS 26.11、KDE／KWin 6.7.5のWaylandセッション、`eDP-1`（1920×1200、60Hz、倍率1）を使用しました。環境のGPUデバイスは `/dev/dri/renderD128`（amdgpu、PCI ID `1002:1681`）です。サンプルは `just fetch-assets` でチェックサムを確認しました。以下は実プロセスのIPCとGPU表示準備の報告状態を使った確認で、目視・マウス操作は含みません。ローカルの実行ログは `target/avatar-console-review/` にあります（Git管理対象外）。

| 確認日・環境 | 項目 | 結果 |
| --- | --- | --- |
| 2026-10-13・上記Linux環境 | Seed-sanのGPU表示準備・利用条件 | `loading: false`、`error: None`、利用条件の報告を確認 |
| 同上 | サンプルVRMAの再生・一時停止・再開・単発再生 | `Playing`→`Paused`→`Playing`→`Finished` と `MotionFinished` を確認 |
| 同上 | 無効なモーション・モデルからの復旧 | モーションの `Failed` から歩行の `Playing` へ復旧。モデルの `error` が有効なモデルの再指定で消えることを確認 |
| 同上 | 設定変更・複数体 | サイズ50／100／200%の送信、FPS60の報告、中央移動による位置更新を確認。2体目の状態が1体目のモデル変更・削除で変わらないことを確認 |
| 同上 | 終了・切断 | `quit`／EOF／Ctrl+C、描画プロセスの強制終了、接続5秒タイムアウト、接続中Ctrl+Cを確認。子プロセスの残留なし |
| 同上 | 自動検証 | CLIテスト3件・既存IPCテスト2件、全workspaceの整形・Clippyに成功。操作用コンソールのビルド・起動・EOF終了も成功 |
| 同上 | 操作性改善後の確認 | CLIテスト5件・既存IPCテスト2件、整形・全workspaceのClippyに成功。実プロセスで `add`、`motion 1 walk`、`pause 1`、ホームパス展開、`state` の詳細表示、正常終了を確認。ログは `target/avatar-console-usability.log` |
| 同上 | 目視・マウス操作による描画、実測FPS、透過、ドラッグ、クリック通知、モニター切替 | 未実施 |
| 未記録・Windows | 描画・入力・DirectComposition | 未実施 |
