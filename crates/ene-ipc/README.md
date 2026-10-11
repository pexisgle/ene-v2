# ene-ipc

desktopクライアントとavatar描画プロセスの間のIPC契約を提供します。

`renderer` featureで描画の要求・通知・状態と共有クライアント型を利用できます。
このクレートは通信ライブラリに依存しません。

`RendererClient<RequestSender, MessageReceiver, StateReceiver>` は要求送信、
通知受信、状態受信のフィールド定義を共有します。avatar・desktop側で具体的なチャネル型を
指定し、Remocへの直接依存、接続・codec選択・送受信・終了処理を持ちます。
両側で同じ具体型を使い、要求は `RendererRequest`、通知は `RendererMessage`、
最新状態は `RendererState` を扱います。

`AvatarId` は `u64` の専用型で、desktopが発行した番号を `AvatarId(number)` として渡します。
desktopは同じ描画セッション内で単調増加の連番を発行し、アバター削除後もIDを再利用しません。
IDの発行・管理はdesktop側が担当し、このクレートは型の定義だけを提供します。

avatar・desktop側での具体型の指定例:

```rust
use ene_ipc::renderer::{RendererClient, RendererMessage, RendererRequest, RendererState};

type AvatarClient = RendererClient<
    remoc::rch::mpsc::Sender<RendererRequest>,
    remoc::rch::mpsc::Receiver<RendererMessage>,
    remoc::rch::watch::Receiver<RendererState>,
>;
```

Remocを使う実装では、要求とイベント・エラーをremote MPSC、最新状態をremote watchで渡し、
`provide`／`consume` で具体型を指定した `RendererClient` を一度受け渡す構成にします。
別チャネル間の到着順には依存せず、状態の正本はwatchのスナップショットです。
`Shutdown`、切断、停止・再起動時の実際の振る舞いはavatar・desktop側で実装・検証します。

```sh
cargo check --locked -p ene-ipc --all-targets --all-features
cargo clippy --locked -p ene-ipc --all-targets --all-features -- -D warnings
```
