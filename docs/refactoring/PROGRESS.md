# ene-v2 リファクタリング進捗台帳

設計書: [`ene_v2_refactoring_plan.md`](ene_v2_refactoring_plan.md)
最終更新: 2026-11-23

このファイルはセッション中断時の再開点でもある。**各 PR のコミット直前に必ず更新する。**

---

## 0. 環境前提（このセッションで確定した事実）

| 項目 | 状態 | 備考 |
| :--- | :--- | :--- |
| `cargo` / `rustc` | 1.100.0 (stable) | `rust-toolchain.toml` と一致 |
| `python3` | **無し** | パス上の `python`/`python3` は Microsoft Store のスタブ (exit 9009)。DSH 同梱の Python 3.12 を使用する。 |
| `buf` | 1.73.0 を `C:\tools\buf\buf.exe` に導入 | README の固定バージョン。`crates/ene-rpc/build.rs` が `use_buf()` で必須。 |
| `sccache` | 0.19.0 を復旧 | `~/.cargo/config.toml` の `rustc-wrapper` が壊れた 0 バイト shim を指していた。`~/.cargo/bin/sccache.exe` と WinGet Links 双方へ実体を配置。 |
| `cargo-deny` | 0.22.0 | 導入済み |
| `cargo-machete` | 0.10.1 | 導入済み |
| `cargo-nextest` | **未導入**（0 バイト shim のみ） | 設計書 3.1 の `--self-test` 等は `cargo test` で代替。nextest 必須要件は無い。 |
| `sccache` | **無効化** | 0.19.0 に Rust コンパイルで `cache errors` を起こす不具合があり、`bevy_internal` のビルドが `sccache: encountered fatal error` で落ちる。リポジトリの `.cargo/config.toml` では無効のまま、`RUSTC_WRAPPER=""` で実行する。 |

### 段階導入中の lint（PR-0.2）

`[workspace.lints.clippy]` に 3 ルールを追加した。いずれも `warn` で全ビルドが報告するが、
`-D warnings` との衝突を避けるため検証ゲートでは明示的に `-A` で除外している。
**各 PR が自分の分の `-A` を外し、同時に `deny` へ引き上げる。**

| lint | 残存箇所 | 解消する PR | 内容 |
| :--- | ---: | :--- | :--- |
| `undocumented_unsafe_blocks` | 63 | PR-4.3 | `// SAFETY:` コメント必須化 |
| `multiple_unsafe_ops_per_block` | 20 | PR-4.3 | 1 ブロック 1 操作 |
| `let_underscore_must_use` | 230 | PR-5.1 | `#[must_use]` の破棄検出 |

> 63 + 20 の `unsafe` は全て `apps/ene-desktop`（`tray.rs` 53 件、`ui/pod/win32.rs` 9 件、
> `ui/pod/mod.rs` 1 件）に集中しており、PR-4.1/PR-4.3 で `crates/ene-platform-os` へ移管する
> 対象そのもの。先に移管してから `SAFETY:` を付けた方が二度手間にならないため、
> Phase 0 では `warn` に留めた。
> `crates/ene-plugin/src/sdk.rs` の 2 件は wasm ゲートが `-A` 対象外だったため PR-0.2 で修正済み。

### 既知の環境制約（コードの問題ではない）

**`host::tests::f10_remote_host_target_enforces_https_and_roundtrips_config` は本ワークスペース配下では失敗する。**

- 原因: `ene_rpc::connect_remote` → `rustls_native_certs::load_native_certs()` が Windows の
  CurrentUser\ROOT 証明書ストアを開く際、**ワークスペースディレクトリ配下から起動したプロセスでは
  `os error 5 (アクセスが拒否されました)`** になる。DSH のファイルサンドボックスがディレクトリ単位の
  ACL を適用しており、その配下のプロセスから証明書ストアへのアクセスが拒否されるため。
- 実証: **同一のテストバイナリ**を `%TEMP%` へコピーして実行すると `ok`（`certs=38` が読める）。
  ワークスペース配下では `certs=0` + permission denied。`.NET` の `X509Store` では両ストアとも
  52 件読めるため、コード側の欠陥ではなくプロセス起動位置に依存する環境要因。
- 対応: 本セッションではこのテストのみ `--skip` して残り全テストの green を確認する。**CI（GitHub
  Actions）では影響しない**ため、CI で green になることをもって完了とする。ローカル再現時は
  `%TEMP%` 配下でテストバイナリを実行すれば通る。


### 検証ゲートの実行方法（Windows / このセッション）

```powershell
$env:PATH = "C:\tools\buf;" + $env:PATH
$py = "C:\Users\pexisgle\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe"
& $py scripts/check-architecture.py --self-test
& $py scripts/check-architecture.py
```

`scripts/verify-all.ps1` にゲート一式をまとめてある。

---

## 1. KPI 推移

`domain_no_rpc_dep` = `ene-rpc` に依存するドメイン系クレート数（Rule 7、目標 0）
`host_world_refs` = `apps/ene-host` 内の `&World`/`&mut World` 本番参照数（Rule 6、目標 0）
`app_unsafe_count` = `apps/ene-desktop` 本番 `unsafe` 数（Rule 8、目標 0）
`max_prod_file_lines` = 本番 500 行超ファイル数（Rule 9、目標 5 以下）
`rpc_no_ecs_resource` = `apps/ene-host/src/server/rpc/` の `#[derive(Resource)]` 数（Rule 10、目標 0）

| 時点 | domain_no_rpc_dep | host_world_refs | app_unsafe_count | max_prod_file_lines | rpc_no_ecs_resource |
| :--- | ---: | ---: | ---: | ---: | ---: |
| **ベースライン (2026-11-23, commit c9f0758)** | 4 | 171 | 84 | 47 | 1 |
| PR-0.1 後 | 4 | 171 | 84 | 47 | 0 |
| PR-1.1 後 | 3 | 171 | 84 | 47 | 0 |
| PR-1.2 後 | 2 | 171 | 84 | 47 | 0 |
| PR-1.3 後 | 1 | 171 | 84 | 47 | 0 |
| PR-1.4 後 | 0 | 171 | 84 | 47 | 0 |
| PR-1.5 後 | 0 | 171 | 84 | 47 | 0 |
| PR-2.1 後 | 0 | 171 | 84 | 47 | 0 |
| PR-2.2 後 | 0 | 168 | 84 | 47 | 0 |
| PR-2.3 後 | 0 | 167 | 84 | 47 | 0 |
| PR-2.4 後 | 0 | 164 | 84 | 47 | 0 |
| PR-2.5 後 | 0 | 161 | 84 | 47 | 0 |
| PR-2.6 後 | 0 | 160 | 84 | 47 | 0 |
| PR-3.2 後 | 0 | 160 | 84 | 47 | 0 |
| PR-4.1 後 | 0 | 160 | 29 | 47 | 0 |
| PR-4.2 (part 1) 後 | 0 | 160 | 17 | 47 | 0 |
| PR-4.2 完了 | 0 | 160 | 6 | 46 | 0 |
| PR-4.3 完了 | 0 | 160 | 0 | 46 | 0 |
| PR-4.4 (part 2) 後 | 0 | 160 | 0 | 45 | 0 |

> **設計書との差分（重要）**: 設計書 §3 は「本番 500 行超ファイル数 = 43」「`Resource` = 2」としているが、
> 実測は **47** と **0** だった。
> - 47 は `scripts/check-architecture.py` と同じ `strip_cfg_test` を通した本番行数での実測値。設計書の 43 は
>   テストコード込みの別集計と推測される。**実測 47 をラチェット初期値とした。**
> - `Resource` = 0 は、設計書が挙げる `Observed` (`server/rpc/chat.rs`) と `Frames` (`server/rpc/voice.rs`) が
>   いずれも `#[cfg(test)]` モジュール内のテスト専用フィクスチャであり、**本番コードには存在しない**ため。
>   Rule 10 は本番コードのみを対象とするため初期値 0 が正しい（達成済み）。テストフィクスチャの
>   `bevy_ecs` 依存を消す作業は PR-2.1 で別途扱う。

---

## 2. PR チェックリスト

### Phase 0: ガードレール
- [x] PR-0.1 `check-architecture.py` に Rule 7〜10 を追加
- [x] PR-0.2 CI nextest / Clippy lint 厳格化 / AGENTS.md 追記

### Phase 1: 依存方向の逆転解消
- [x] PR-1.1 `ene-policy` から `ene-rpc` 削除（Rule 7: 4→3）
- [x] PR-1.2 `ene-plugin` から `ene-rpc` 削除（Rule 7: 3→2）
- [x] PR-1.3 `ene-catalog` の接続モデル純粋化（Rule 7: 2→1）
- [x] PR-1.4 `ene-voice` から `ene-rpc` 削除（Rule 7: 1→0）
- [x] PR-1.5 `ene-client-core` のワイヤー分離検証

**Phase 1 完了 (DoD 達成)**: `Rule 7 (domain_no_rpc_dep) == 0`。
`ene-domain` / `ene-policy` / `ene-catalog` / `ene-plugin` / `ene-voice` の
いずれも `Cargo.toml` に `ene-rpc` を持たない。

### Phase 2: `World` 脱却
- [x] PR-2.1 `server/rpc/{chat,voice}.rs` の Resource 撤去（Rule 10: 2→0）
- [x] PR-2.2 `host/work/*` の World 参照解体（171→168）
- [x] PR-2.3 `host/mcp/supervisor.rs` 統合（171→167、検出 5→1 Resource）
- [x] PR-2.4 `host/client/*` + `eyes` の Gateway 化（171→164）
- [x] PR-2.5 `host/permission/*` の PermissionService 化（171→161）
- [x] PR-2.6 残存 World 参照の部分撤去（171→160、→0 は Phase 3 後に再評価）

### Phase 3: RPC 薄層化・二重管理解消
- [ ] PR-3.1 `crates/ene-host-core` 新設・`request!` マクロ撤廃
- [x] PR-3.2 `settings/mod.rs`（988 行）の 5 分割（→549 行 + 5 サブモジュール）
- [x] PR-3.3 `rpc/voice.rs` は本番 498 行で既に閾値内（853 行はテスト込みの計測）→ 対応不要と判断
- [ ] PR-3.4 純粋ロジックの `crates/*` 昇格（mind/work/policy 二重管理）

### Phase 4: Desktop 整理
- [x] PR-4.1 `crates/ene-platform-os` 新設・`tray.rs` 移管（unsafe 84→29）
- [x] PR-4.2 `window` / `capture` / `input` 移管（unsafe 29→6、長大ファイル 47→46）
- [x] PR-4.3 `#![forbid(unsafe_code)]` 有効化（Rule 8: 84→0 達成）
- [ ] PR-4.4 巨大 GPUI ビュー分割（bubble 931→526+423、row_editor 874→376+351+182。policy / editor / onboarding は未着手）
- [ ] PR-4.5 `widgets/` プリミティブ分解
- [ ] PR-4.6 `session/` 状態機械化・TaskTracker 統一

### Phase 5: 運用・テスト完成
- [ ] PR-5.1 `let _ = ...` の系統的撲滅（platform crate 49→0、apps/* 208→151。client/mod.rs は複雑度上限のため未着手）
- [x] PR-5.2 観測性標準化・`ErrorCode` 単一化（`is_retryable` は消費者不在のため削除、otel は未導入）
- [ ] PR-5.3 proptest / insta / E2E 拡充

---

## 3. 判断ログ（設計書と実コードの差分）
（各 PR 実施時に追記）

| 日付 | PR | 判断内容 |
| :--- | :--- | :--- |
| 2026-11-23 | — | `check-architecture.py` は既存 6 ルール。設計書の Rule 7〜10 は未実装。ベースライン値（4/171/84/43/2）は実測と一致した。 |
| 2026-11-23 | PR-0.1 | Rule 7〜10 を実装。初期ラチェットを実測値で設定: `domain_no_rpc_dep=4`, `app_unsafe_count=84`, `max_prod_file_lines=47`, `rpc_no_ecs_resource=0`。Rule 10 のみ設計書の 2 ではなく 0 を採用（`Observed`/`Frames` はテスト専用のため本番 0 が正）。`--self-test` に 8 個のフィクスチャを追加し、各新ルールの検出とテストコード除外の両方を検証。 |
| 2026-11-23 | PR-0.1 | `cargo deny` が既存の `RUSTSEC-2026-0592`（`url` 2.5.8 の `Url::origin()` スタックオーバーフロー）で失敗していたため、`url` を 2.5.9 へ更新。 |
| 2026-11-23 | PR-0.1 | `cargo machete` の 4 件を精査: `serde_json_canonicalizer` は `ene-host` の未使用 dev-dependency なので削除。`buffa-build`/`connectrpc-build` は `build.rs` 専用で machete が走査しないため ignore に追加。`libc` は gpui の推移依存バージョン固定で直接参照が無いため ignore に追加（いずれも理由コメント付き）。 |
| 2026-11-23 | PR-0.2 | `check-architecture.py` を再確認したところ `strip_cfg_test` は正常（`chat.rs`/`voice.rs` の `Observed`/`Frames` は本当に `#[cfg(test)]` 内）。Rule 10 の初期値 0 はこれで裏付け済み。 |
| 2026-11-23 | PR-0.2 | 新 lint 3 つは `warn` で導入。`-D warnings` は `warn` を `deny` に昇格させるため、ゲートは `-A` を後置して除外する（clippy は後勝ち）。計画の「警告→段階的に deny」を満たしつつ、未処理 313 箇所で Phase 0 が止まらないようにした。各 PR が自分の `-A` を外す運用。 |
| 2026-11-23 | PR-0.2 | CI を `cargo test` → `cargo nextest run --no-fail-fast` に切替（failed 時の全件報告とフレーク検出）。ローカルは nextest 未導入のため `cargo test` のまま。`scripts/verify-all.ps1` にゲート一式を集約。 |

## 4. 再開手順

1. `docs/refactoring/PROGRESS.md` のチェックリストで未完了の先頭 PR を確認する。
2. `git log --oneline -n 10` で直近コミットが台帳と一致することを確認する。
3. `pwsh -NoProfile -File scripts/verify-all.ps1` で現状の green を確認する。
4. その PR を実装 → ゲート通過 → `PROGRESS.md` 更新 → コミット。

## 5. 次セッションへの引き継ぎ（PR-2.5 以降の設計メモ）

### PR-4.2 の実装メモ（次セッションはここから）

**現状**: `app_unsafe_count = 29`（84 から削減済み、PR-4.1）。残りは 4 ファイル:

| ファイル | unsafe 数 | 移管先（設計書 §7.1） |
| :--- | ---: | :--- |
| `apps/ene-desktop/src/ui/pod/win32.rs` | 11 | `ene-platform-os::window` |
| `apps/ene-desktop/src/perception/screen.rs` | 8 | `ene-platform-os::capture` |
| `apps/ene-desktop/src/computer.rs` | 6 | `ene-platform-os::input` |
| `apps/ene-desktop/src/perception/platform_windows.rs` | 3 | `ene-platform-os::capture` |
| `apps/ene-desktop/src/ui/pod/mod.rs` | 1 | `ene-platform-os::window` |

**PR-4.1 で確立したパターン**（そのまま踏襲すればよい）:

1. `crates/ene-platform-os/src/<module>.rs` へ**ファイルごと移す**（中身は変えない）。
2. UI 型に依存していたら、プラットフォーム側に**ミラー enum** を定義し、
   UI 側で変換関数を書く。依存の向きは必ず「app → platform」の一方向に保つ。
   実例: `TrayMode` / `TraySection` と `ui/hub/windows.rs` の
   `tray_mode()` / `settings_section()`。
3. `ene-desktop` 側は `use ene_platform_os::...` へ差し替えるだけ。
4. `apps/ene-desktop/Cargo.toml` の Windows 依存から不要になった
   `windows` / `image` を外す（crate 側へ移す）。
5. ラチェット `MAX_DESKTOP_UNSAFE` を**同じコミットで**下げる。

**PR-4.3 の前提**: 4 ファイルすべてを移管し終えるまで
`#![forbid(unsafe_code)]` は有効化できない（部分移管では残りが落ちる）。

**注意**: `computer.rs` の `Desktop` 構造体は Win32 ハンドル
（`window: isize`、`RECT`、`HWND`）を保持する不透明型なので `input` ごと移すのが自然。
`Frame`（JPEG と寸法）は `capture` 側の型なので、`Desktop` と `Frame` の分割に注意。

### PR-3.2 の実装メモ（完了済み / 制約の記録）

`apps/ene-host/src/server/rpc/settings/mod.rs` は 988 行のまま。分割は試作して
**破棄した**（未コミット、作業ツリーは clean・ゲート green）。

**試作で判明した制約（再試行時に必ず読むこと）**:

1. **Rust は同一型への同一トレイトの `impl` を複数書けない。**
   設計書の「5 ファイルに分割」を素直に行うと
   `E0119: conflicting implementations of trait SettingsService` で落ちる。
   正しい形は「**1 つの impl は維持**し、各メソッド本体をサブモジュールの
   自由関数へ移して impl から委譲する」。
   ```rust
   impl w::SettingsService for Services {
       async fn get_settings(&self, ctx: RequestContext, request: ...) -> ... {
           general::get_settings(self, ctx, request).await
       }
   }
   ```
2. メソッド境界は「`#[tracing::instrument` 行 〜 次の `#[tracing::instrument`
   の直前」。実測値（元ファイルの 1 始まり行番号、`impl` は 20〜780 行）:

   | メソッド | instrument | async fn | | メソッド | instrument | async fn |
   | :--- | ---: | ---: | :--- | :--- | ---: | ---: |
   | get_settings | 21 | 30 | | list_providers | 395 | 404 |
   | update_settings | 43 | 52 | | list_models | 415 | 425 |
   | list_pending_approvals | 132 | 141 | | register_credentials | 450 | 460 |
   | delete_connection | 161 | 171 | | revoke_credentials | 490 | 500 |
   | submit_approval | 217 | 226 | | check_connection | 523 | 533 |
   | list_policy_rules | 253 | 262 | | list_plugins | 593 | 602 |
   | update_policy_rules | 282 | 291 | | install_plugin | 662 | 671 |
   | list_policy_texts | 324 | 333 | | set_plugin_enabled | 706 | 716 |
   | update_policy_texts | 353 | 362 | | remove_plugin | 752 | 762 |

3. ヘルパ（`notify_settings` / `is_builtin_voicevox` / `rule_from` /
   `model_item` / `modality_names` / `run_connection_probe`）は 782 行目以降にあり、
   サブモジュールから使うため `pub(super)` が必要。
   `async` は `pub(super) async fn`（`async pub(super)` は構文エラー）。
4. サブモジュール側は `use super::{...}` でヘルパを取り込む。

### 残り `&mut World` 関数の実測分類（PR-2.4 完了時点）

`apps/ene-host/src/host/` のうち `World` を引数に取る本番関数 154 個を、
関数本体が実際に何をしているかで分類した:

| 分類 | 個数 | 意味 |
| :--- | ---: | :--- |
| **resource-only** | **58** | リソース参照のみ。引数注入で `World` を外せる |
| entity-only | 25 | `spawn`/`trigger`/`query`/`despawn` のみ。ECS に残す |
| both | 23 | 両方。分割が要る |
| neither | 48 | `World` をそのまま別関数へ渡しているだけ。呼び出し側を直せば連鎖的に消える |

**この 58 + 48 = 106 個が PR-2.5 / PR-2.6 の実対象。**
`Rule 6` は「`&mut World` を引数に取る関数」を数えるので、`neither` の
連鎖を上から切るのが最も効率的（1 箇所直すと複数が消える）。

優先順位（呼び出しグラフの上流から）:
1. `neither` 48 個のうち、外部公開 API（`server/rpc/` から呼ばれるもの）を先に直す。
2. 次に `resource-only` 58 個を、使用しているリソースだけを受け取る形へ。
3. `both` 23 個は最後（本体を「リソース部分」と「エンティティ部分」に割る）。

### 計画の数値目標に到達できない理由（重要）

設計書は PR-2.2→125、PR-2.3→95、PR-2.4→55、PR-2.6→0 としているが、
実測は 171→168→167→164 と、各 PR で 1〜3 しか減っていない。

理由は **`Rule 6` の定義**にある。このルールは
`&\s*(?:mut\s+)?World\b` を数える、つまり「`&mut World` を**引数に取る**関数」を
数える。したがって:
- 関数の中身をどれだけ整理しても、シグネチャが `&mut World` のままなら減らない。
- リソースを1つに統合しても（PR-2.3）、`sync(world: &mut World)` は残るので減らない。

**減らすには「`World` を取らない関数へ変える」以外に方法がない。**
PR-2.6 の目標 0 は、`bevy_ecs` を `agent/runtime.rs` の内部に完全に隠蔽し、
ホストの全サービスが `&mut World` を受け取らない設計にした場合のみ達成できる。
これは Phase 3 の `ene-host-core`（PR-3.1）と不可分であり、
**PR-2.6 を単独で 0 にするのは非現実的**。実務的には Phase 3 完了後に再評価する。



### PR-2.2 の結果（実測 171 → 168）

計画は 171→125 を目標にしていたが、**実測は 168**。理由を記録する。

Rule 6 は「`&mut World` / `&World` を**引数に取る**関数」を数える。したがって
`World` を取る関数の引数を増やしても減らない。減らすには
「`World` を取る関数を廃止する」しかない。PR-2.2 で実際に廃止できたのは:

| 関数 | 旧 | 新 |
| :--- | :--- | :--- |
| `hand::stop` | `(world, work_id)` | `(hands, calls, runtime, wake, work_id)` |
| `hand::release` | `(world, work_id)` | `(calls, hands, work_id)` |
| `hand::drop_pending` | `(world, hands, adapter)` | `(calls, hands, adapter)` |

これらは `World` を **リソース参照のためだけ**に取っていた。`CallDespawner`
（`hand.rs`）が唯一の真の ECS 操作（保留 call エンティティの despawn）を隔離する。

**167→125 に進めない理由**（重要 / 次セッションは再調査不要）:
残る `host/work/` の `World` 取得関数は、いずれも**本物の ECS 操作**を行う。

- `agent_of` / `retire_entity` / `retire` / `restore` — `query::<(Entity, &WorkAgent)>`
- `start` / `send` / `stop_with` / `close` / `finish_run` / `detect_stalls` / `inform`
  — `world.spawn` / `world.trigger` / `world.get_mut::<WorkAgent>`、加えて
  `super::fail` と `client::computer::invalidate_run` が `&mut World` を要求する
- `tools.rs` の 4 関数 — `world.get::<CompanionAgent>` / `Conversation` の読み出し

つまり **`WorkAgent` / `Conversation` を ECS から追い出さない限り、125 には到達しない**。
これは設計書 §5.2 が「ECS 残留」と明記していない設計判断であり、
PR-2.6（World = 0）の前提でもある。**PR-2.3 に着手する前に、この判断をここへ記録すること。**

### 検証ゲートの既知の罠（解決済み・再発防止）

`scripts/verify-all.ps1` の cert-store ゲートが断続的に落ちていた原因は
**スクリプト側のバグ**だった（環境要因ではなかった）。

- 症状: 2 つの TLS テストが `os error 5` で落ちる。単体実行では 10/10 通る。
- 原因: 呼び出し元が `$env:TEMP` をリポジトリ配下（`.cargo-tmp`）に設定して
  `verify-all.ps1` を起動すると、スクリプトはそれを `$stagingRoot` として
  引き継ぎ、**子プロセスの TMP/TEMP をワークスペース配下に戻していた**。
  サンドボックスはその配下のプロセスに証明書ストアを開かせない。
- 修正: `$env:TEMP` が `$repo` 配下なら `%LOCALAPPDATA%\Temp` にフォールバックする。
- 検証: フルゲートを2回連続実行し、いずれも `ALL GATES PASS`。




### PR-4.4 の残り（row_editor / policy / editor / onboarding）

ubble.rs は 931→526+423 に分割済み。残り 4 ファイルは **未着手**。


ow_editor.rs（874 行）は同じ手法で 3 分割まで到達したが、配線で破綻して revert した:

- iews.rs が orm.rs の関数と EditorDraftSet のフィールドを読むため、
  関数とフィールドの両方を pub(super) にする必要がある。
- それを正規表現で一括でやろうとすると、構造体リテラルのフィールド初期化子や
  関数の引数まで巻き込んで壊れる（実際に壊れた）。

**次回の手順**: 可視性は型定義を直接編集して広げること。
「フィールド宣言」と「フィールド初期化子」を区別できない置換は 874 行では必ず事故る。
