# ene-ipc

desktopとavatar描画プロセスのRemoc IPC APIを提供します。`renderer` featureで利用します。

`Renderer` traitからRemocが `RendererClient` と `RendererServer` を生成します。
avatarはtraitを実装し、生成されたサーバーの `serve()` で操作を順番に処理します。
要求enum、要求ID、返信チャネル、要求を振り分けるrunnerは自前で実装しません。

接続時に `RendererSession` を標準入出力のパイプ上で `provide`／`consume` します。
avatar側のパイプI/Oは `interprocess`、標準入出力のFD／HANDLEの借用は
`io-lifetimes` に委譲し、Linux／Windowsで同じ接続コードを使用します。
Windowsのパイプ読み取りはブロッキング処理を使うため、ランタイム終了の
タイムアウトは維持します。
セッションは次の3経路を持ちます。

- `renderer`: 初期化、アバター追加・削除、設定変更、中央配置、終了のRTCクライアント。
- `messages`: `ShowChat`、`MotionFinished`、非同期のエラー通知を受信するremote MPSC。
- `states`: 最新の `RendererState` を受信するremote watch。中間更新は集約されます。

```rust,ignore
use ene_ipc::renderer::{Renderer, RendererConfig, RendererSession};

async fn control(mut session: RendererSession) -> Result<(), ene_ipc::renderer::RendererError> {
    session.renderer.initialize(RendererConfig::default(), vec![]).await?;
    session.renderer.set_render_config(RendererConfig { fps: 60 }).await?;
    session.renderer.shutdown().await?;
    Ok(())
}
```

操作の検証・重複ID・不明ID・個別ウィンドウへの投入失敗は、その呼び出しの
`RendererError::Rejected` として返します。通信障害は `RendererError::Call` です。
追加の成功はID登録と読み込み開始、個別設定・中央配置の成功はウィンドウへの要求投入を
意味します。モデル読み込みや実際の位置・再生状態はwatchで確認します。
全体FPS変更は管理設定の更新を返し、各ウィンドウへの投入失敗は非同期エラーで報告します。
削除はウィンドウと資源の解放後に返るため、削除完了の別通知はありません。

`shutdown(self)` はクライアントを消費して生成サーバーを終了し、全ウィンドウを解放します。
Remocが返信の送信完了を待ち、desktopは子プロセスの `wait()` で終了を確認します。
クライアントの破棄や接続切断でもサーバーが終了し、ウィンドウを解放します。
ウィンドウの所有フィールドは、要求チャネル、`jod-thread` の終了待ちハンドル、
`scopeguard` の状態登録ガードの順に破棄します。これにより、要求チャネルの
切断→描画スレッドの終了待ち→watchからの削除を、手書きの `Drop` なしで行います。
描画のpanicは引き続きワーカースレッド内で捕捉し、アバターのエラー状態に反映します。
イベント送信はFuturesのローカルキューをRemocの `SenderSink` へ `forward` し、
描画スレッドでは通信を待ちません。手書きのイベント転送ループは持ちません。
キューは非有界で、接続済みの相手がイベントを長時間読まなければメモリが増えます。

`AvatarId` はdesktopが発行する `u64` の専用型です。同じ描画セッション内では単調増加させ、
削除後も再利用しません。別経路間の到着順には依存せず、状態の正本はwatchのスナップショットです。
描画全体の設定もwatchだけで保持し、サービス側に同じ設定のコピーを持ちません。

```sh
cargo check --locked -p ene-ipc --all-targets --all-features
cargo clippy --locked -p ene-ipc --all-targets --all-features -- -D warnings
```
