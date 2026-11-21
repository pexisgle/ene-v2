# ene-v2 プロダクション品質引き上げ・修正計画書

対象スナップショット：`ene-v2`（2026-11-21 時点、`src/` 全 169 ファイル / 92,247 行）

## 全体方針

`crates/` 配下（`ene-vrm`, `ene-plugin`, `ene-plugin-runtime`, `ene-rpc`, `ene-avatar-ipc`）は責務もテスト比率も健全である一方、全コードの **84.3%** が `apps/ene-host`（47,501 行）と `apps/ene-desktop`（30,290 行）に集中している。

過剰な抽象化やラッパーの追加は避け、以下の 4 フェーズで **「コンパイラが境界を強制する構造」「UI と状態機械の分離」「機械的な Lint / CI ガードレール」** を導入する。

| フェーズ | 目的 | 主な対象 | 完了条件 |
| :--- | :--- | :--- | :--- |
| Phase 0 | 新規劣化の止血（ガードレール導入） | `Cargo.toml`, `clippy.toml`, `rustfmt.toml`, `deny.toml`, `AGENTS.md`, `.github/workflows/ci.yml` | `just verify` + `cargo deny check` が通過し、新規の巨大関数・`unwrap` が CI で弾かれる |
| Phase 1 | `ene-host` の循環依存解消と境界固定 | `apps/ene-host/src/host/`、`crates/ene-policy`、`crates/ene-catalog` | `ene-host` 内の双方向結合 5 組が 0 件、`permission/mod.rs` 分割完了 |
| Phase 2 | `ene-desktop` の頭脳と UI の分離 | `ui/mod.rs`, `session.rs`, `state.rs`、`crates/ene-client-core` | `ui/mod.rs` が 500 行以下、`ene-client-core` が `gpui` 非依存でテスト比率 30% 以上 |
| Phase 3 | 巨大描画関数の解体と型安全化 | `ui/connections.rs`, `ui/device.rs`, `ui/pod.rs`, `ui/companions.rs`、全 `Result<_, String>` | 1,000 行超の `.rs` ファイル 0 件、`#[allow(clippy::too_many_lines)]` 0 件、`Result<_, String>` 0 件 |

---

## Phase 0：機械的ガードレールの先行導入（止血）

リファクタリング前に防波堤を敷く。既存違反は一時的に関数単位の `#[allow(...)]` で隔離し、新規コードおよびエージェント生成コードの肥大化を即座に止める。

### 0-1. `Cargo.toml`（ワークスペースルート）

全クレートがすでに `[lints] workspace = true` を継承しているため、ルートの `[workspace.lints]` に Clippy ルールを追加する。

```toml
[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_debug_implementations = "warn"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
dbg_macro = "deny"
todo = "warn"
unimplemented = "deny"
large_futures = "warn"
too_many_lines = "warn"
cognitive_complexity = "warn"
rest_pat_in_fully_bound_structs = "warn"
```

### 0-2. `clippy.toml`（新規作成）

テストコードの実用性を損なわず、プロダクションコードだけを締める。

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
too-many-lines-threshold = 120
cognitive-complexity-threshold = 20
too-many-arguments-threshold = 6
```

### 0-3. `rustfmt.toml`（新規作成）

import の並び順ノイズと差分膨張を防ぐ。

```toml
edition = "2024"
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

### 0-4. `deny.toml`（新規作成）と `.github/workflows/ci.yml` 更新

- `deny.toml` を追加し、CI の `Linux checks` ジョブに `cargo deny check` と `cargo machete` を組み込む。
- 脆弱性（RustSec）、ライセンス不整合、同一クレートの複数バージョン混在、未使用依存を自動検出する。

### 0-5. `AGENTS.md` の構造規約化

現状 1 行のみの `AGENTS.md` に、以下の不変条件を明文化する。

- 1 ファイル 500 行以内、1 関数 120 行以内を目安とし、超える場合は責務単位で分割する。
- `apps/ene-desktop/src/ui/` に Protobuf 変換（`w::*` の直接書き換え）、ベンダー分岐、RPC 調停を書かない。
- 新規コードで `Result<_, String>` を使用しない（`thiserror` でドメインエラーを定義する）。
- `#[allow(clippy::...)]` を追加する場合は、直前コメントに理由と削除条件を明記する。
- クレート間の依存方向は「`apps/*` → Layer 2 → Layer 1 → Layer 0」の一方向のみとする。

---

## Phase 1：`apps/ene-host` の循環依存解消とドメイン境界の確定

既存テスト比率が 38.2% と高いため、最初に `ene-host` の構造を整えて安全な足場を作る。

### 1-1. 双方向モジュール結合（5 組）の解消

| 循環ペア | 現在の逆流箇所（ファイル・行番号） | 具体的な修正内容 |
| :--- | :--- | :--- |
| `db ⇄ work` | `apps/ene-host/src/host/work/hand.rs:652` が `crate::host::db::truncate` を参照 | `truncate` / `digest` / `epoch` などの純粋ユーティリティを `db` から独立モジュール（`host::util` または共通基盤）へ移動。これだけで `db ⇄ work` は完全解消する。 |
| `db ⇄ mind` | `mind/consolidate.rs:425`, `mind/embedding/backfill.rs:65,159,181`, `mind/embedding/mod.rs:67`, `mind/memory.rs:6` が `db` を参照。逆に `db/companions.rs:3,88,167`, `db/memories.rs:6-8` が `mind` を参照 | 1) 上記ユーティリティ（`truncate`, `digest`, `epoch`）を `db` 外へ分離。<br>2) `Companion`, `Profile`, `Fact`, `PendingEmbedding` 等の永続化ドメイン型を `crates/ene-mind`（下位層）へ置き、`db` は `ene-mind` に依存する一方向にする。<br>3) `backfill.rs` の DB 読み書き呼び出しは `Store` 経由の呼び出し側（オーケストレーション層）へ移す。 |
| `client ⇄ mcp` | `client/dynamic.rs:391,408` が `mcp::tool_names` を参照。逆に `mcp/projection.rs:148` が `client::dynamic::sync_tools` を呼び出し | ツール投影の同期責務（`sync_tools`）を `client` から独立した `host::tools::sync`（または ECS の `ToolsDirty` イベント／フラグ）へ抽出する。`mcp` と `client` は互いに直接呼ばず、イベント／ファサードのみを参照する。 |
| `client ⇄ providers` | `client/dynamic.rs:392,409` が `providers::extensions::tool_names`、`client/mod.rs:730` が `trigger_character_event` を参照。逆に `providers/extensions.rs:392` が `client::dynamic::sync_tools` を呼び出し | 上記と同じ `ToolsDirty` / `host::tools::sync` へ統合し、`providers -> client` の逆参照を削除する。 |
| `providers ⇄ voice` | `providers/extensions.rs:169-172` が `voice::{speech::for_speech, on_stream_text, on_turn_end}` を呼び出し。逆に `voice/plugin.rs:27` が `Arc<providers::Running>` を保持 | プラグインランタイムの実行ハンドル（`Running`）を `providers` 固有の内部型から切り離し、`voice/plugin.rs` が `providers::extensions` に依存しない構成にする（あるいは `extensions` からの発話通知を ECS イベント経由に統一する）。 |

### 1-2. `apps/ene-host/src/host/permission/mod.rs`（3,127 行）の解体

1 ファイルに 4 つの責務と 3 箇所の散在する `#[cfg(test)]`（L654, L2142, L2156）が同居している。以下のように分離する。

| 現在の行範囲 | 主なシンボル | 移動先・責務 |
| :--- | :--- | :--- |
| L17–L653 | `PolicyEffect`, `PolicyRule`, `ActionTarget`, `extract_target`, `hard_denied_action`, `canonicalise`, `decide_action`, `decide_capability` | **`crates/ene-policy`**（新設、Bevy ECS / SQLite 非依存の純粋クレート）へ抽出。L654–L881 の単体テストも同梱する。 |
| L882–L1464 | `GatePlugin`, `gate`, `issue_hold`, `notify_approval_card`, `resolve_capability`, `within_tool_limits` | `apps/ene-host/src/host/permission/gate.rs`（ECS ゲート処理） |
| L1465–L1809 | `Decisions`, `DecisionInputs`, `decision_prompt`, `run_decision`, `start_decision`, `collect_decisions` | `apps/ene-host/src/host/permission/decision.rs`（Decision LLM 補助判定） |
| L1810–L2141 | `submit`, `submit_with_outcome`, `resolve_approval`, `expire_orphaned_approvals` | `apps/ene-host/src/host/permission/approval.rs`（承認状態遷移と永続化） |
| L2142–L3127 | `tool_limit_tests`, `decision_tests` | 各分割ファイルの末尾、または `permission/tests.rs` へ集約 |

### 1-3. `crates/ene-catalog`（新設）への共通知識の集約

現在 `apps/ene-host` と `apps/ene-desktop/src/ui/mod.rs` の双方に散在しているプロバイダ・ベンダー固有知識を 1 クレートに集約する。

- 移設対象の例（現状 `apps/ene-desktop/src/ui/mod.rs` L3713–L4265 に混在）：
  - `is_sakura_endpoint`, `is_sakura_conn`, `is_local_voicevox_conn`
  - `default_embedding_dimensions`, `default_models_for_adapter`
  - `parse_voice_resource`, `search_adapter_of`, `search_adapter_requires_key`
  - `apply_tts_voice_to_config`, `apply_stt_model_to_config`, `build_voice_assignment_inputs`
- これにより、ホストとデスクトップ間で既定値やアダプタ判定がズレる不具合を構造的に防ぐ。

### 1-4. `ene-host` の `pub(crate)` 露出削減（819 件の整理）

- `apps/ene-host/src/host/mod.rs` の 26 個の `pub(crate) mod` について、サブモジュール内部の関数・構造体は原則プライベートまたは `pub(super)` に変更する。
- 他モジュールから参照される API のみを各サブモジュールの `mod.rs` で明示的に `pub(crate) use` する（ファサード化）。

---

## Phase 2：`apps/ene-desktop` の頭脳（State / Session）と UI（GPUI）の分離

`apps/ene-desktop` はプロダクション 27,680 行に対してテスト 2,610 行（8.6%）と最も脆い。UI フレームワーク（`gpui`）に依存しない `crates/ene-client-core` を切り出すことで、高速なヘッドレス単体テストを可能にする。

### 2-1. `apps/ene-desktop/src/ui/mod.rs`（4,662 行 / `impl Hub` 3,491 行・119 メソッド）の解体

| 現在の責務（`ui/mod.rs` 内） | 代表メソッド・関数 | 移動先 |
| :--- | :--- | :--- |
| ウィンドウ・トレイ制御 | `register_pod_window`, `open_pod`, `hide_pod`, `sync_window_closed`, `sync_tray_state`, `handle_tray_action` | `apps/ene-desktop/src/ui/window_manager.rs`（または薄い `ui/mod.rs` に残す） |
| 状態遷移・RPC 調停・履歴管理 | `apply`, `request`, `request_with`, `refresh_active_data`, `resnapshot`, `select_companion`, `load_history`, `send`, `send_keyed`, `retry_unsent`, `stop` | **`crates/ene-client-core`**（`Controller` / `Command` / `Reducer`） |
| 音声・知覚トグル制御 | `toggle_mic_mute`, `toggle_capture_mode`, `ptt_toggle`, `ptt_cancel`, `set_eyes_enabled`, `set_computer_use_enabled`, `toggle_output_mute` | `crates/ene-client-core` のコマンド処理へ移設し、OS デバイス操作はトレイト境界またはチャネルで `apps/ene-desktop` へ委譲 |
| モデル・接続・シークレット選択肢計算 | `maybe_auto_assign_default_model`, `register_secret_for_connections`, `tts_voice_choices`, `stt_model_choices_for_connection`, `available_models_for_connection`, `collect_connection_choices`, `all_llm_models` 等（L3290–L3713） | `crates/ene-catalog`（既定値・アダプタ規則）＋ `crates/ene-client-core`（ViewModel 生成） |
| ベンダー判定・Config 変換ヘルパー | L3713–L4275 のフリー関数群（`is_sakura_*`, `default_embedding_dimensions`, `llm_provider_input`, `search_connection_input` 等） | `crates/ene-catalog` へ完全移設 |

最終目標：`apps/ene-desktop/src/ui/mod.rs` は **500 行以下** に収め、GPUI の Entity 購読とビューの組み立てのみを担当する。

### 2-2. God Struct の分割（`state.rs`・`session.rs`・`ui/studio.rs`）

| 対象構造体 | 現状フィールド数 | 分割後の構造 |
| :--- | :---: | :--- |
| `apps/ene-desktop/src/state.rs` `struct Model` | 32 | `ConnectionViewState`, `CompanionState`, `AudioState`, `PerceptionState`, `ApprovalState` の 5 サブ構造体へネスト整理し、`fn apply`（292 行）をサブステート別の更新関数へ委譲する。 |
| `apps/ene-desktop/src/session.rs` `struct Session` | 32 | `serve`（248 行）と `command`（259 行）を、RPC ストリーム管理（`stream.rs`）・コマンドハンドラ（`commands.rs`）・アバター／音声ブリッジ（`bridge.rs`）へ分割する。 |
| `apps/ene-desktop/src/ui/studio.rs` `struct SettingsStudio` | 34 | 各設定タブ（接続・コンパニオン・音声・デバイス・MCP）固有の一時フォーム状態を各タブ専用の `FormState` 構造体へ移し、`SettingsStudio` は選択中セクションと各タブ状態への参照のみを持つ。 |

### 2-3. `ene-client-core` のテスト拡充

`gpui` に依存しない状態で、以下をユニットテスト化する（テスト比率 8.6% → 30% 以上を目標）：

- 接続追加・編集・モデル自動割り当て（`maybe_auto_assign_default_model`）の状態遷移
- 音声停止・再生世代切り替え・PTT キャンセルの状態整合性
- コンパニオン切り替え時の履歴ロード・未送信リトライ・承認カード応答

---

## Phase 3：巨大描画関数の解体とエラー型安全化

### 3-1. 500 行超の巨大 UI 関数の分割対象一覧

| ファイル | 対象関数 | 現状行数 | 分割方針 |
| :--- | :--- | :---: | :--- |
| `apps/ene-desktop/src/ui/device.rs` | `render_device`（L46） | 1,642 | セクション別モジュール（`device/permissions.rs`, `device/perception.rs`, `device/host_access.rs`, `device/plugins.rs` 等）へ分割し、各描画関数を 100 行以下にする。 |
| `apps/ene-desktop/src/ui/connections.rs` | `render_connection_editor`（L1280） | 1,328 | 編集フォーム状態を `ConnectionFormState` にまとめ、基本設定・認証・モデル選択・テスト接続・保存アクションのサブコンポーネントへ分割する。 |
| `apps/ene-desktop/src/ui/connections.rs` | `render_advanced_roles_panel`（L2786） | 614 | ロール別（Chat / Decision / Embedding / Compaction 等）の共通行コンポーネントを抽出し、重複するドロップダウン描画コードを削減する。 |
| `apps/ene-desktop/src/ui/connections.rs` | `render_add_connection_panel`（L585）<br>`render_web_search_card`（L199） | 487<br>385 | `connections/` ディレクトリ化し、`list.rs`, `editor.rs`, `add_panel.rs`, `roles.rs`, `web_search.rs` に分離する。 |
| `apps/ene-desktop/src/ui/companions.rs` | `render_companion_editor`（L719）<br>`render_companions`（L17） | 637<br>361 | `companions/` ディレクトリ化し、`list.rs`, `create.rs`, `editor.rs`, `voice_body.rs` に分離する。 |
| `apps/ene-desktop/src/ui/pod.rs` | `render_onboarding_body`（L688）<br>`render_bubble_stage`（L291）<br>`render_expanded_timeline`（L1439） | 597<br>229<br>213 | `pod/` ディレクトリ化し、`onboarding.rs`, `bubble.rs`, `timeline.rs`, `composer.rs` に分離する。 |
| `apps/ene-desktop/src/ui/voice.rs` | `render_voice`（L72） | 532 | 入力（マイク / VAD / PTT）・出力（TTS / スピーカー）・Voicevox 管理の 3 セクション関数へ分割する。 |
| `apps/ene-desktop/src/ui/mcp_add.rs` | `render_mcp_add`（L17） | 370 | トランスポート選択（stdio / HTTP）・環境変数編集・プレビューの 3 関数へ分割する。 |

### 3-2. `Result<_, String>`（`ene-desktop` 63 箇所 / `ene-host` 19 箇所）の撲滅

`apps/ene-desktop/src/error.rs`（または `crates/ene-client-core/src/error.rs`）に `thiserror` ベースの構造化エラー型を定義し、以下の箇所を順次置換する。

| 対象ファイル | `Result<_, String>` 件数 | 置換先のエラー型設計 |
| :--- | :---: | :--- |
| `apps/ene-desktop/src/avatar.rs` | 18 | `AvatarError`（IPC 通信失敗、プロセス起動失敗、プロトコル不正） |
| `apps/ene-desktop/src/session.rs` | 11 | `SessionError`（ConnectRPC 障害、入力パース失敗、未接続状態） |
| `apps/ene-desktop/src/work_tools.rs` | 10 | `ToolExecError`（引数不正、パス検証失敗、コマンド実行失敗） |
| `apps/ene-desktop/src/audio.rs` | 8 | `AudioError`（デバイス取得失敗、リサンプラ初期化失敗、ストリーム停止） |
| `apps/ene-desktop/src/computer.rs` | 7 | `ComputerUseError`（権限不足、OS 操作失敗、引数不正） |
| その他（`main.rs`, `ui/workflows.rs`, `perception.rs`, `plugins.rs`, `state.rs`, `ui/companions.rs`, `ui/mod.rs`） | 9 | 上記エラー型または `ClientError` へ統合 |

### 3-3. 最終クリーンアップ（Phase 0 の一時 `#[allow]` 全廃）

- Phase 0 で既存コードに付与した `#[allow(clippy::too_many_lines)]` や `#[allow(clippy::too_many_arguments)]` をすべて削除する。
- 最終検証コマンドを実行し、全項目グリーンを確認する。

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny check
cargo machete
```
