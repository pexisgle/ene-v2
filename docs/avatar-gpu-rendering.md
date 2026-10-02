# VRMのGPU表示と入力領域

通常経路はBevyと同じWGPU 29.0.4デバイスでWayland surfaceへ描画する。RGBA画像とalpha画像をCPUへ戻さず、GPUが最終画像から生成した水平区間の座標だけを読み戻す。公開IPCと保存設定の形式は変更しない。

## フレームと入力領域

BevyのRGBA8 SRGB描画先を3枠の再利用可能なGPU画像へコピーする。各枠にはフレーム番号、世代、物理寸法、整数DPI、座標バッファ、非同期読み戻し状態を保持する。GPUは論理画素に対応する物理画素のいずれかのalphaが8/255を超えた場合に、その論理画素を入力領域へ含める。従来の`alpha_runs`と同じ条件である。

論理画素の判定を256スレッドのworkgroupへ分散し、区間の先頭だけが終点を調べる。透明な背景を行ごとに直列走査しない。座標の出力順序はGPU上では不定でも、CPUで行・始点順に並べることで同じ領域を得られる。

座標列の容量が足りない場合は容量を拡張し、その枠の同じGPU画像から再生成する。切れた座標列や処理途中の結果は提示しない。CPUは完了した座標を順序づけ、隣接行の同じ区間を矩形にまとめてWaylandへ登録する。隙間を埋める矩形近似は行わない。表示用シェーダーは従来の色変換・premultiplied alphaに合わせる。対応しない透明合成形式では明示的に失敗する。

入力領域とbuffer scaleを設定し、同じ枠の画像の`SurfaceTexture::present()`でattachとcommitを行う。途中で入力領域だけをcommitしない。枠の参照が残る間は再利用しない。モデル、モーション状態、サイズ、DPI、出力、非表示からの復帰で世代を更新し、前世代の結果を破棄する。リサイズ中は、次の対応する画像が用意できるまで、以前の表示画像とその入力領域を保持する。

Waylandの入力領域はcommitで画像とともに反映される。[Wayland仕様](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_surface)。WGPUのWayland上のpresentはbufferのattachとcommitを行う。[WGPU仕様](https://docs.rs/wgpu/29.0.3/wgpu/struct.SurfaceTexture.html#method.present)。実装に使う29.0.4のローカルソースと、実機のWaylandプロトコルログでもこの順序を確認した。

## 開発時の起動

`just run-avatar`と`just run-desktop`は、描画プロセスを依存ライブラリも含めreleaseビルドする。通常のworkspace検査と会話UIの開発ビルドは従来どおり。
