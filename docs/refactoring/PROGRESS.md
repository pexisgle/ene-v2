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
- [ ] PR-2.2 `host/work/*` の World 参照解体（171→125）
- [ ] PR-2.3 `host/mcp/supervisor.rs` 統合（→95）
- [ ] PR-2.4 `host/client/*` + `eyes` の Gateway 化（→55）
- [ ] PR-2.5 `host/permission/*` の PermissionService 化（→25）
- [ ] PR-2.6 残存 World 参照の完全撤去（→0）

### Phase 3: RPC 薄層化・二重管理解消
- [ ] PR-3.1 `crates/ene-host-core` 新設・`request!` マクロ撤廃
- [ ] PR-3.2 `settings/mod.rs`（1,005 行）の 5 分割
- [ ] PR-3.3 `rpc/voice.rs`（853 行）の分離
- [ ] PR-3.4 純粋ロジックの `crates/*` 昇格（mind/work/policy 二重管理）

### Phase 4: Desktop 整理
- [ ] PR-4.1 `crates/ene-platform-os` 新設・`tray.rs` 移管
- [ ] PR-4.2 `window` / `capture` / `input` 移管
- [ ] PR-4.3 `#![forbid(unsafe_code)]` 有効化（Rule 8: →0）
- [ ] PR-4.4 巨大 GPUI ビュー分割（bubble / row_editor / policy / editor / onboarding）
- [ ] PR-4.5 `widgets/` プリミティブ分解
- [ ] PR-4.6 `session/` 状態機械化・TaskTracker 統一

### Phase 5: 運用・テスト完成
- [ ] PR-5.1 `let _ = ...` の系統的撲滅
- [ ] PR-5.2 観測性標準化・`ErrorCode`・otel feature
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

## 5. 次セッションへの引き継ぎ（PR-2.2 の設計メモ）

**現状**: Phase 0・Phase 1 完了、PR-2.1 完了。`host_world_refs = 171`（未着手）。

**PR-2.2 で判明している構造的事実**（再調査不要）:

- `host/work/` の `&mut World` は **2種類が混在**している。
  1. **ただの状態コンテナ**（`World` から引く必要がない）:
     `Hands`（`hand.rs:55`、`HashMap<String, Hand>`）、`Runs`（`mod.rs:34`、
     `ene_work::RunTracker` の newtype）、`Receipts`（`mod.rs:53`、
     `ene_work::WorkReceipts` の newtype）。この3つは引数注入
     （`&mut Hands`, `&mut Runs`, `&mut Receipts`）へ置換可能。
  2. **本物の ECS エンティティ操作**（ECS に残すべき）:
     `World::query::<(Entity, &WorkAgent)>`（`mod.rs:71,112,129`）、
     `world.get_mut::<WorkAgent>(entity)`（`mod.rs:94,455`）、
     `world.get::<Conversation>(entity)`（`mod.rs:421`）、
     `world.entity_mut(call).despawn()`（`hand.rs:307`）。
- 外部呼び出し元は `server/rpc/work.rs` の `request!` マクロ
  （`work::start` / `work::send` / `work::stop` / `work::close`）と
  `host/agent/mod.rs:722` の `work::settle_run` の **5箇所のみ**。

**PR-2.2 の推奨アプローチ**:
`WorkOrchestrator` を新設し、`Hands`/`Runs`/`Receipts` を所有させる。
`World` を取る公開関数は「orchestrator + `&mut World`（ECS操作のみ）」を
受け取る形へ変える。`request!` マクロ側は PR-3.1 で
`ene-host-core` を入れるときにまとめて変わるため、PR-2.2 では
マクロの呼び出しシグネチャを壊さない範囲で内部実装を差し替えるのが安全。

**注意**: `World` を完全に 0 にする（PR-2.6 の目標）には、`WorkAgent` /
`Conversation` を ECS から追い出す設計判断が要る。設計書 §5.2 は
「`Connections`, `WorkReports` は ECS に残留」としており、
`WorkAgent`/`Conversation` の扱いは設計書に明記が無い。
PR-2.6 着手時にこの点を判断し、ここへ記録すること。

