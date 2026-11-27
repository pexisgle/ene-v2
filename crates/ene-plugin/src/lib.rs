//! Plugin manifests, typed extension APIs and the Extism guest SDK.
#![warn(missing_docs)]
// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
//! Native execution and application authorization live outside this crate.
//!
//! プラグイン作者向けの型定義とRust SDKをまとめています。manifest、拡張API、ホストサービスの契約に加え、WasmビルドではExtism PDKと `emit` / `service` / HTTPヘルパーを提供します。
//! 旧 `ene-plugin-sdk` はこのパッケージへ統合しました。`use ene_plugin::*;` で利用できます。
//! モデル情報・設定項目は `ene-rpc` の生成型を共有しますが、プラグイン通信にConnect RPCを使う意味ではありません。
//!
//! ## 現在の実装範囲
//!
//! 実行と検証は `ene-plugin-runtime`、アプリ固有の認可は各アプリが担当します。
//! `ene-host` はExtismをプロセス内に組み込んで実行します。デスクトップクライアントは未実装で、
//! OSイベント取得、購読の認可と配送、アバターへの動作反映は未実装です。
//! `ene-avatar` は `apps/ene-avatar/src/control.rs` のJSON行通信を持ちますが、
//! プラグインイベントからその通信への変換はまだありません。
//!
//! Extism の組み込み `extism:host/env`（alloc/store/load、config、log）は
//! PDK の都合で残ります。実行基盤は `max_var_bytes = 0` にして var 書き込みを
//! 無効化しますが、`log_*` は manifest の `log` サービスとは別に host の stderr へ
//! 出る点に注意してください。
//!
//! ## 共通契約
//!
//! | 定義 | 用途 |
//! | --- | --- |
//! | `Manifest` / `EntryPoint` | 実行入口、提供する拡張、利用するサービス |
//! | `Operation` | ホストからプラグインへの依頼 |
//! | `Event` | プラグインからの結果・途中経過 |
//! | `ServiceRequest` | プラグインからホストへの機能利用依頼 |
//! | `Failure` | ホストサービス呼び出しの失敗 |
//!
//! 実行方式はExtism固定で、manifestに `runtime` は指定しません。Luauスクリプトは受け付けません。WasmのABIはExtismが担当します。
//! 共通型はクレート直下から利用できます。内部ファイルは `manifest.rs`、`api/`（`wire.rs` の共通enumと、`llm` / `character` / `harness` / `http` の型）、`failure.rs`、`sdk.rs` です。
//! `provider` は認証方式、ルーティング設定、チャットメッセージを提供します。
//! HTTPは `HttpOpen` / `HttpRead` / `HttpClose` で依頼し、認証情報と通信の認可はホスト側で管理する設計です。
//! プラグイン provider の通信は同一 origin に限定し、Generate / ConnectionCheck は
//! `POST <endpoint>/chat/completions` または `POST <endpoint>/responses`、
//! ListModels は `GET <endpoint>/models` だけを許可します。公開カタログの取得は
//! ホスト専用の catalog transport が行います。
//! `service::<T>` は応答を直接 `T` に復号します。戻り値が不要な操作は `service::<()>`、未保存ならnullを返す状態読取は `service::<Option<T>>` を使います。`http_close` の戻り値は `()` です。
//! SDKのヘルパーは `Result<T, extism_pdk::Error>` を返します。サービスの `Failure` はPDKエラーの原因として保持され、`error.downcast_ref::<Failure>()` でコードを取得できます。`FnResult` は `#[plugin_fn]` のエントリポイントで使い、ヘルパーのエラーは `?` で変換します。
//! Providerは生成ごとに新しいInstanceで実行します。Instanceを再利用する場合は、再利用前に `ene_initialize` を再実行します。`ene_initialize` は再入可能にし、guest globalへ前回リクエスト固有の会話・認証情報を残さないでください。
//! ハーネス入力とイベントの詳細は[タスクハーネス](#タスクハーネス)を参照してください。
//! `Log` サービスは認可後、plugin/provider IDを添えてホストの標準エラー出力へ記録します。メッセージは4096バイトまでで、改行・制御文字はエスケープします。
//! manifestの参照ファイル検証と入出力のサイズ制限も実行基盤の責務です。
//! 子プロセスの起動やworker用IPCはありません。
//! 現在のWasmサンプルはExtismの関数呼び出しと `ene_service` / `ene_emit` を使います。
//!
//! ## タスクハーネス
//!
//! ホスト向け Wasm パッケージは `provides` に `harness` を宣言し、
//! `Operation::Harness` に応答します。入出力には ACP v1 の型を使います。
//! 一つのセッションで複数のプロンプトターンを扱います。
//!
//! | 入力 | 受け取る時点 |
//! | --- | --- |
//! | `HarnessInput::Start { prompt, cwd }` | セッションを開始し、最初のプロンプトを渡す |
//! | `HarnessInput::Prompt { prompt }` | 生存中のセッションに次のプロンプトを渡す |
//! | `HarnessInput::Respond { request_id, outcome }` | 保留した権限要求に対するホストの決定を渡す |
//! | `HarnessInput::Cancel` | 保留中のターンを中止する |
//!
//! 実行中の呼び出しはホストが中断できます。権限要求は `RequestPermission` を
//! 呼び出しの最後に送信し、その後に受け取る `Respond` でターンを継続します。
//! 選択肢の `option_id` は識別子です。ホストは `PermissionOptionKind` によって
//! 今回の承認または拒否を選び、該当する種類がなければ `cancelled` を返します。
//!
//! 進捗は `Update` のテキスト、ツール状態、または計画で送信します。
//! ターンは `Stop` で終了します。ホストは `end_turn` を完了、`cancelled` を
//! 中断として記録し、拒否と上限到達を失敗として記録します。
//! 権限選択肢は最大8件、選択肢と要求のIDは最大128文字、タイトルは最大256文字、
//! ツールのパスは最大4096文字、計画は最大512件です。
//!
//! ハーネス呼び出しではホストサービスが拒否されます。ゲストは `cwd` を
//! セッションの文脈として使えます。外部 ACP への stdio 接続と ACP の
//! ファイル・ターミナル機能は現行の実行環境にありません。
//!
//! ## 動作の対象と認可
//!
//! `CharacterTarget { device_id, character_id }` で動作対象を明示します。
//! `Operation::Character`、`Event::Character`、`ServiceRequest::Character`
//! が同じ対象型を使います。拡張の出力は `emit(Event::Character { target, action })`、
//! ホストへ依頼して結果・失敗を受け取る場合は `service(ServiceRequest::Character { target, action })` を使います。
//! 通知・要求応答・端末サービスの契約は異なります。実際の動作の実装まで三重に持つ意味ではありません。
//! 表情・モーション・発話の定義があっても、レンダラーがすべてを実行できるという意味ではありません。
//!
//! 実行基盤は次を検証する必要があります。このクレートやSDKが自動で強制するものではありません。
//!
//! - manifestの宣言は要求であり許可ではない。端末とバインディングに対する許可を別途検証する。
//! - `target` は許可されたバインディングから決める。返された対象も照合し、別端末・別キャラクターへの操作を拒否する。
//! - 世代・呼び出しの有効性を確認し、終了済み・取り消し済みの呼び出しからの動作を実行しない。
//! - モーションのassetは検証済みmanifest内の資産へ解決し、任意ホストパスとして開かない。
//! - 移動等の大量イベントに対するキュー上限・集約は実行基盤で設ける。
//!
//! ホストサービス経由では `character_actions` の許可も確認します。
//! キャラクター拡張の結果としての動作は、許可されたバインディングの対象に限ります。
//!
//! ## バージョンと移行
//!
//! `CONTRACT_VERSION` は **8** です。実行基盤は読み込み時に
//! `contract_version` の一致を検証します。パッケージ自身の版は
//! `Manifest.version` で管理します。移行時は現行の SDK で Wasm を再ビルドし、
//! 下記のサンプルと同じ manifest 形式で配布してください。
//!
//! 更新後はパッケージ revision が変わるため、新しい版の認可を確認してください。
//!
//! モデル情報は引き続きprotobuf JSON形式です。メタデータのフィールド名はcamelCase、
//! 64ビット整数は文字列、任意のリストは `{"values": [...]}` です。
//!
//! ## サンプルと検証
//!
//! [OpenRouterサンプル](../../../examples/plugins/openrouter-wasm/) はHTTP応答のSSEを解析する例です。現行のHTTP brokerは上限付きで本文全体を受信した後、ABIサイズ内のチャンクに分割します。ネットワークからのライブストリーミングではありません。
//!
//! ```sh
//! cargo check --locked -p ene-plugin --all-targets --all-features
//! cargo clippy --locked -p ene-plugin -p ene-openrouter-wasm -p ene-reference-wasm --target wasm32-unknown-unknown -- -D warnings
//! ```
mod api;
mod failure;
mod manifest;
pub mod provider;

pub use api::{
    AudioFormat, CharacterAction, CharacterEvent, CharacterTarget, Content, ContentBlock,
    ContentChunk, EmbeddingCapabilities, EmbeddingRequest, EmbeddingResult, Event, HarnessEvent,
    HarnessInput, HookDecision, HookEvent, HttpChunk, HttpRequest, LlmCapabilities,
    MAX_HTTP_HEADER_BYTES, MAX_HTTP_HEADER_FIELDS, MessageId, Operation, PermissionOption,
    PermissionOptionId, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    PluginToolDefinition, RequestId, RequestPermissionOutcome, SelectedPermissionOutcome,
    ServiceRequest, SessionUpdate, SpeechCapabilities, StopReason, SynthesisRequest,
    SynthesisResult, TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolInvokeRequest, ToolInvokeResult,
    ToolKind, TranscriptionRequest, TranscriptionResult, text_block,
};
pub use failure::Failure;
pub use manifest::{
    CONTRACT_VERSION, EntryPoint, Extension, Manifest, ProviderConfiguration, Service, Target,
};
pub use provider::{FieldSource, LlmProviderCapabilities, Model, ModelMetadata, SettingField};
#[cfg(target_family = "wasm")]
mod sdk;
#[cfg(target_family = "wasm")]
pub use sdk::*;
