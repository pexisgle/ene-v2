//! Connect RPC messages and services generated from `proto/host.proto`.
//! Consumers use these types directly; transport limits belong to Connect RPC.
//! Enable `client` for loopback HTTP/Bearer transport setup with `connect`.
//!
//! `proto/host.proto` を正本とするConnect RPCの契約です。ビルド時にbuffa/Connectの型とサービスを生成し、ホスト、SDK、テストが生成型を直接使います。
//!
//! ## サービス
//!
//! | サービス | 用途 |
//! | --- | --- |
//! | `HostService` | スナップショット、購読、コンパニオンCRUD、心・記憶一覧 |
//! | `ChatService` | 受理、停止、履歴、メッセージ |
//! | `SettingsService` | 設定、承認、ポリシー、プロバイダー／モデル、資格情報、接続確認 |
//! | `WorkService` | 作業セッションの一覧・取得・開始・追加指示・停止・クローズ |
//! | `ClientService` | 端末登録、アクティブ端末、能力、端末操作 |
//! | `VoiceService` | マイク入力、合成音声の購読、合成設定の確認 |
//!
//! ## 境界
//!
//! - 認証は接続経路のOwner Bearerだけです。`client_id` は接続端末の区別であり、認証ではありません。
//! - 同じ `request_key` の再送は元の受理結果を返し、内容が違えば競合です。受理は実行完了ではありません。結果は購読・履歴・状態取得で確認します。
//! - 通知には起動epochと単調なsequenceを付けます。`Subscribe` は `Notification` のストリームを返し、開始時に `initial_state`（Snapshot）、取りこぼし時に `resync_required` を送ります。現在状態は `GetSnapshot` でも取得できます。
//! - `Snapshot` はコンパニオン・端末・設定・作業セッション・承認・ポリシー・healthを持ち、会話履歴は含めません。履歴は `GetHistory` でページングします。
//! - 任意JSONはラッパー型ではなくJSON textの `string` で運びます。ホストは境界で1回だけparseし、保存するのはドメイン型です。
//! - `UpdateCompanionRequest` は `companion` / `task` / `perception` / `compaction` / `consolidation` / `decision` / `embedding` の7 assignment patchと `persona` / `voice_identifier` を持ちます。`optional string` の未指定は変更なし、空文字は `{}` へのクリアです。
//! - `MemoryControlResult` は `importance`（0〜10）と `scope`（`'shared'` またはコンパニオンID）を返します。
//! - 旧 `JsonDocument` / `JsonDocumentPatch`、`Grant` / `Consent` / `Availability`、`Target` などは削除済みです。旧クライアントとは互換性がありません。
//!
//! ネイティブのクライアントは `connect(endpoint, token)` でループバックHTTP/2のBearer設定を作り、生成クライアントへ渡します。Wasmではメッセージ型だけを生成し、Connectのブラウザーruntimeを含めません。

// The generated module keeps its own `#[allow]`; this covers the hand-written code.
/// Generated v1 contract, available in every build.
// Suppresses style and size lints inside generated `_connectrpc.rs`; remove when codegen complies.
// `redundant_closure_for_method_calls` and `needless_pass_by_value` are
// pedantic, so `clippy::all` does not cover them and the generator trips both.
#[allow(
    missing_debug_implementations,
    clippy::all,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::large_futures,
    clippy::redundant_closure_for_method_calls,
    clippy::needless_pass_by_value,
    clippy::cloned_instead_of_copied,
    clippy::implicit_clone,
    clippy::inefficient_to_string,
    clippy::manual_let_else
)]
pub mod rpc {
    include!(concat!(env!("OUT_DIR"), "/_connectrpc.rs"));
    pub use ene::host::v1::*;
}

/// Built-in tools owned by `ene-host`. Client capabilities and plugins may not shadow them.
pub use ene_policy::BUILTIN_TOOLS;
/// Desktop GUI capability executed by a work agent on its bound client.
pub use ene_policy::COMPUTER_USE;
/// How much damage an approval's action could do. The desktop renders it on
/// the approval card and the host decides it, so it travels with the contract.
pub use ene_policy::RiskLevel;
pub use rpc::*;

#[cfg(not(target_family = "wasm"))]
pub mod client_model;
#[cfg(not(target_family = "wasm"))]
mod domain;
/// Maximum width or height of a reported JPEG frame.
pub const MAX_PERCEPTION_FRAME_EDGE: u32 = 1280;
/// Maximum encoded JPEG size before Connect serialization.
pub const MAX_PERCEPTION_JPEG_BYTES: usize = 512 * 1024;

#[cfg(all(feature = "client", not(target_family = "wasm")))]
mod client;
#[cfg(all(feature = "client", not(target_family = "wasm")))]
pub use client::connect;
#[cfg(all(feature = "client-tls", not(target_family = "wasm")))]
pub use client::{connect_remote, validate_remote_endpoint};
