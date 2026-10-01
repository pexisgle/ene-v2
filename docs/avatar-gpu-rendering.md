# VRMのGPU表示と入力領域

通常経路はBevyと同じWGPU 29.0.4デバイスでWayland surfaceへ描画する。RGBA画像とalpha画像をCPUへ戻さず、GPUが最終画像から生成した水平区間の座標だけを読み戻す。公開IPCと保存設定の形式は変更しない。

## フレームと入力領域

BevyのRGBA8 SRGB描画先を3枠の再利用可能なGPU画像へコピーする。各枠にはフレーム番号、世代、物理寸法、整数DPI、座標バッファ、非同期読み戻し状態を保持する。GPUは論理画素に対応する物理画素のいずれかのalphaが8/255を超えた場合に、その論理画素を入力領域へ含める。従来の`alpha_runs`と同じ条件である。

論理画素の判定を256スレッドのworkgroupへ分散し、区間の先頭だけが終点を調べる。透明な背景を行ごとに直列走査しない。座標の出力順序はGPU上では不定でも、CPUで行・始点順に並べることで同じ領域を得られる。

座標列の容量が足りない場合は容量を拡張し、その枠の同じGPU画像から再生成する。切れた座標列や処理途中の結果は提示しない。CPUは完了した座標を順序づけ、隣接行の同じ区間を矩形にまとめてWaylandへ登録する。隙間を埋める矩形近似は行わない。表示用シェーダーは従来の色変換・premultiplied alphaに合わせる。対応しない透明合成形式では明示的に失敗する。

入力領域とbuffer scaleを設定し、同じ枠の画像の`SurfaceTexture::present()`でattachとcommitを行う。途中で入力領域だけをcommitしない。枠の参照が残る間は再利用しない。モデル、モーション状態、サイズ、DPI、出力、非表示からの復帰で世代を更新し、前世代の結果を破棄する。リサイズ中は、次の対応する画像が用意できるまで、以前の表示画像とその入力領域を保持する。

Waylandの入力領域はcommitで画像とともに反映される。[Wayland仕様](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_surface)。WGPUのWayland上のpresentはbufferのattachとcommitを行う。[WGPU仕様](https://docs.rs/wgpu/29.0.3/wgpu/struct.SurfaceTexture.html#method.present)。実装に使う29.0.4のローカルソースと、実機のWaylandプロトコルログでもこの順序を確認した。

## 比較経路

開発用環境変数`ENE_AVATAR_BACKEND`で、同じバイナリから比較する。設定ファイルやIPCには追加しない。

| 値 | 表示 | 入力領域 |
|---|---|---|
| `gpu`（通常） | GPUからWayland | 最終画像のGPU走査 |
| `geometry` | GPUからWayland | CPUで変形後の頂点を投影し三角形被覆を合成 |
| `empty` | GPUからWayland | 空領域。表示だけの負荷を測る |
| `shm` | 画像読み戻し、CPU変換、wl_shm | 従来のCPU alpha走査 |

CPUメッシュ候補はスキニング、モーフ、現在の骨変形、クリッピング、カリングを反映する。テクスチャalpha、素材の半透明、アウトライン、MSAAを再現しないため、誤差があれば採用しない。

## 再現する測定

依存ライブラリを含むreleaseビルドを使い、描画プロセスを1つずつ起動する。`just run-avatar`と`just run-desktop`でも描画プロセスはreleaseビルドを使う。通常のworkspace検査と会話UIの開発ビルドは従来どおり。Nix環境では次のように実行する。

```sh
direnv exec . cargo build --release --locked -p ene-avatar
direnv exec . python3 scripts/benchmark-avatar.py --fps 60 --sizes 100 --scales 1 --output-dir /tmp/ene-avatar-benchmark
```

標準比較はReady後10秒のウォームアップ、60秒の測定を3回。モデルとモーションは端末の設定を読み取り、Configure IPCでその実行だけに適用する。設定ファイルは書き換えない。各実行の終了とパイプの回収が終わるまで次を起動しない。正確性検査は別プロセスで行う。

```sh
# 短時間の動作確認
direnv exec . python3 scripts/benchmark-avatar.py --smoke --backends gpu geometry
# 静止、サイズ、DPIの組合せ
direnv exec . python3 scripts/benchmark-avatar.py --static --fps 60 --sizes 50 100 200 --scales 1 2
# 非表示、再表示、停止、再生、サイズ、出力、モデル選択失敗からの復帰
direnv exec . python3 scripts/benchmark-avatar.py --backends gpu --exercise --duration 30 --repeats 1 --skip-verify
```

今回の組合せ検査を再現するには、各コマンドに`--backends gpu geometry --warmup 1 --duration 3 --repeats 1 --verify-warmup 1 --verify-duration 4`を指定する。静止姿勢には`--static`（その実行だけモーションを外す）、各モーションには`--motion "$HOME/.cache/ene/motions/<ファイル名>.vrma"`を使う。4本は`three-vrm-sample`、`quaternius-dance`、`josie-walk`、`josie-slow-run`。検査用モデルには`--static --model /tmp/ene-coverage.vrm`を指定する。測定ディレクトリは実行ごとに変える。

モデル切替を含む操作検査では`--exercise --exercise-model /tmp/ene-coverage.vrm --exercise-output HDMI-A-1`を指定する。出力名は実機に合わせる。`scripts/check-avatar-pointer.py --output-dir /tmp/ene-avatar-pointer`では、プロファイルと検査画像の読み戻しを無効にして、実ポインターのクリック・ドラッグ・透明部分からのフォーカス退出を確認する。書込み可能な`/dev/uinput`と、出力名・仮想デスクトップ寸法の指定が必要。

`ENE_AVATAR_PROFILE=1`でCPU段階時間、GPU timestamp、Wayland presentation feedbackを標準エラーへJSON行として出力する。プロファイル実行中の表示surfaceにだけidle inhibitorを作り、省電力で表示が停止した時間を性能値に混ぜない。通常実行と非表示中は抑止しない。`ENE_AVATAR_VERIFY=1`でのみ、提示した枠の画像を検査用に読み戻し、CPUのalpha判定と比較する。通常実行はtimestampの読み戻しも検査画像の読み戻しも行わない。

FPSはwp_presentationのpresentedイベントを測定時間で割る。送出数とは区別し、discardedと表示時刻間隔も記録する。CPU使用率は1コアを100%とし、DRM fdinfoのプロセス別GPUエンジン使用率と、デスクトップ全体のGPU使用率を別に記録する。利用できない項目はゼロとせず未取得とする。表示遅延はCLOCK_MONOTONICの対応を測定し、同じ時計の実表示時刻とenqueue時刻の差を誤差範囲付きで記録する。コールバック受信までの遅延は別項目とする。GPU描画は同じRenderGraph Submitバッチ内の前後timestamp、入力領域はcompute pass、色変換はrender pass内のtimestampで測る。CPUの`coverage_ns`は投影／座標デコード、GPU側処理のCPU準備、Wayland領域登録の合計で、表示用drawやswapchain取得待ちを含めない。転送量はフレームごとに合算し、画像・座標とtimestampを分ける。実行ごとのJSON、CSV、標準入出力ログ、環境・設定のメタデータを残す。

採用条件は標準設定で平均59fps以上、入力領域のCPU処理とGPU処理がそれぞれp95で2ms以下、検査対象の誤判定ゼロ。複数の候補が合格した場合は処理p95、差が10%未満ならCPU使用率、さらに同程度ならGPU方式の順で選ぶ。

## 検査用モデル

`python3 scripts/make-avatar-test-model.py /tmp/ene-coverage.vrm`で外部素材を使わない検査用モデルを生成する。OPAQUE／MASK／BLEND、透明テクスチャ、alpha 0／8／9／128／255、左右の腕と胴体の隙間、0.75のモーフ重み、MToonアウトラインを含む。骨のアニメーションはSeed-sanの4モーションとスキニング単体検査で確認する。

## 実測結果

実測結果と検証範囲は[性能比較](avatar-performance-results.md)に記録する。

表示数・欠落・時刻の定義は[presentation-time仕様](https://wayland.app/protocols/presentation-time)に従う。
