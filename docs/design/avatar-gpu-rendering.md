# VRMのGPU表示と入力領域

通常経路はBevyと同じWGPU 29.0.4デバイスでWayland surfaceへ描画する。RGBA画像とalpha画像をCPUへ戻さず、GPUが最終画像から生成した水平区間の座標だけを読み戻す。描画画像はIPCへ転送しない。

## フレームと入力領域

BevyのRGBA8 SRGB描画先を3枠の再利用可能なGPU画像へコピーする。各枠にはフレーム番号、世代、物理寸法、整数DPI、座標バッファ、非同期読み戻し状態を保持する。GPUは論理画素に対応する物理画素のいずれかのalphaが8/255を超えた場合に、その論理画素を入力領域へ含める。従来の`alpha_runs`と同じ条件である。

論理画素の判定を256スレッドのworkgroupへ分散し、区間の先頭だけが終点を調べる。透明な背景を行ごとに直列走査しない。座標の出力順序はGPU上では不定でも、CPUで行・始点順に並べることで同じ領域を得られる。

座標列の容量が足りない場合は容量を拡張し、その枠の同じGPU画像から再生成する。切れた座標列や処理途中の結果は提示しない。CPUは完了した座標を順序づけ、隣接行の同じ区間を矩形にまとめてWaylandへ登録する。隙間を埋める矩形近似は行わない。表示用シェーダーは従来の色変換・premultiplied alphaに合わせる。対応しない透明合成形式では明示的に失敗する。

入力領域とbuffer scaleを設定し、同じ枠の画像の`SurfaceTexture::present()`でattachとcommitを行う。途中で入力領域だけをcommitしない。枠の参照が残る間は再利用しない。モデル、モーション状態、サイズ、DPI、出力、出力の再接続で世代を更新し、前世代の結果を破棄する。リサイズ中は、次の対応する画像が用意できるまで、以前の表示画像とその入力領域を保持する。

Waylandの入力領域はcommitで画像とともに反映される。[Wayland仕様](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_surface)。WGPUのWayland上のpresentはbufferのattachとcommitを行う。[WGPU仕様](https://docs.rs/wgpu/29.0.3/wgpu/struct.SurfaceTexture.html#method.present)。実装に使う29.0.4のローカルソースと、実機のWaylandプロトコルログでもこの順序を確認した。

## 開発時の起動

`just run-avatar`・`just run-desktop`はdebugビルドで起動する。開発中のコードはdevプロファイルで`opt-level = 1`、Bevyなどの外部依存ライブラリは`opt-level = 3`で最適化する。

## 設定と状態を分けて同期する

通信は子プロセスの標準入出力上のRemocで行う。型は `ene-ipc::renderer` を使い、`provide`／`consume` で `RendererClient` を一度渡して通信タスクの管理をRemocへ任せる。要求とイベント・エラーはremote MPSC、共通設定とIDごとの最新状態はremote watchで送る。異なるチャネルの到着順には依存せず、状態はwatchのスナップショットから読む。子プロセス側は標準入出力の匿名パイプをTokioの `pipe::Receiver`／`pipe::Sender` として渡し、キャンセルできない読み取りを避ける。stdoutにはRemocの通信だけを出し、ログはstderrへ出す。`--standalone`では通信接続を作らず、端末からも起動できる。

接続設定・codec・項目上限・バッファはRemoc標準を使う。通信失敗は接続を終了してUIに通知する。

| 方向 | メッセージ | 用途 |
| --- | --- | --- |
| desktop → renderer | `Initialize { config, avatars }` | 共通描画設定とID・個別設定・初期配置の一覧（空一覧も可） |
| desktop → renderer | `SetRenderConfig { config }` | 全ウィンドウのFPS。0はアプリ側の上限なし |
| desktop → renderer | `AddAvatar { avatar }` / `RemoveAvatar { avatar_id }` | 対象ウィンドウの作成と資源解放 |
| desktop → renderer | `SetAvatarConfig { avatar_id, config }` | 個別設定の更新。配置と共通FPSは含めない |
| desktop → renderer | `Recenter { avatar_id }` / `Shutdown` | 対象の中央配置とプロセス全体の終了 |
| renderer → desktop | remote watchの `RendererState` | 共通描画設定と、IDごとの配置・読み込み・再生・エラーの最新スナップショット |
| renderer → desktop | `Event { avatar_id, event }` | モーション完了の一度ごとの通知 |
| renderer → desktop | `AvatarRemoved { avatar_id }` / `Error { avatar_id, message }` | 資源解放後の削除完了と対象付きエラー |

設定はdesktop、左上基準の配置と実行状態はrendererが所有する。RemocのMPSCが要求・通知の順序とバックプレッシャーを、watchが最新状態の集約を担う。Waylandへの操作命令はcalloopの64件の有限チャネルへ渡し、受信と送信側の終了でイベントループが起きる。desktopのUI通知は有限キューを使い、イベントを一度に最大32件処理する。

描画制御はBevyのPluginへまとめる。メインスレッドのcustom runnerはfinish／cleanupを行い、登録済みシステムへ要求を直接渡す。Tokio runtimeは非同期通信を担当する。各アバターはEntityとウィンドウを所有するComponentで管理する。共通描画設定はResourceに置き、Bevyの変更検知で更新時だけ配信する。ウィンドウごとのWayland接続と描画Appは専用スレッドで動く。Entityの削除時にcalloopの送信側を閉じ、スレッド終了と資源解放を待ってwatchから状態を削除し、`AvatarRemoved`を送る。他のウィンドウは継続する。UIの非表示は削除、再表示は保存してある個別設定と最新配置による再追加で扱う。モーション失敗は`MotionState::Failed { message }`で通知する。

desktopの終了・再起動は通常の要求より優先し、すべてのremoteチャネルを閉じて子プロセスの終了を待つ。子プロセスは3秒以内に終了を待ち、期限を過ぎた場合は停止させる。再起動では旧プロセスとの通信を終了して通知を破棄し、最新の設定と配置で初期化する。

新しい通知を追加するときは、現在値だけで足りる情報を `AvatarState`、各発生を処理する必要がある情報を `AvatarEvent` に追加する。

実装の基準：[Bevy 0.19.1のcustom runner例](https://github.com/bevyengine/bevy/blob/v0.19.1/examples/app/custom_loop.rs)、[登録済みシステム](https://docs.rs/bevy/0.19.1/bevy/ecs/world/struct.World.html#method.run_system_with)、[TokioのUnixパイプ](https://docs.rs/tokio/latest/tokio/net/unix/pipe/struct.Receiver.html)。
