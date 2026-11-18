# ライブラリ委譲の再監査（PR #10 head 6e76e01）

[前回の監査](library-delegation-audit.md)の後、ACPバックエンドとプラグイン実経路が追加された。本稿はその追加分と、前回の判定を現在のコードで再確認した結果を残す。行数ではなく、eneが保守しなくてよくなった責務で記述する。

## 対象

- head `6e76e0102f3c03c316437e7457e4140bd1065f68`（`feat/conversation-core`）、base `main`。
- 開始時の作業ツリーはクリーン。未コミット変更の上書き・破棄はしていない。
- 固定版の一次資料: `agent-client-protocol` 5.2.0、`extism` 1.41.0、`extism-manifest` 1.41.0、`wasmtime` 49、`process-wrap` 10.0.1、`rig-*` 0.46.0、`audioadapter-*` 5.2.0。すべて `Cargo.lock` の解決結果と、`~/.cargo/registry/src/` の実ソースで確認した。

## 適用した変更

### 1. ACP子プロセスの停止をライブラリのシグナル送出へ委譲

- 変更: `apps/ene-host/src/host/work/acp.rs` の `terminate`。
- 委譲先: `process-wrap` 10.0.1 `ChildWrapper::signal`（`src/tokio/core.rs:228`、`ProcessGroupChild` 実装は `src/tokio/process_group.rs:230` → `killpg`、`src/tokio/process_group.rs:113`）。
- eneが持たなくなった責務: プロセスグループへのシグナル送出そのもの。`command_wrap` が `ProcessGroup::leader()` を付けるため、`signal` は子だけでなくグループ全体に届く。
- 併せて直した挙動: 従来は `start_kill()`（`SIGKILL` のみ、`src/tokio/process_group.rs:174`）だったため、子は終了の機会を得られなかった。`SIGTERM` → `TERM_GRACE`（2秒）待機 → `SIGKILL` の順に変更した。ACP SDK 5.2.0 の停止手順（`src/acp_agent.rs:169-188`、定数は `:26` と `:36`）と同じ形。
- 不変条件の更新: `work/mod.rs` の `FORCE_GRACE`（10秒）は `TERM_GRACE` + `KILL_TIMEOUT` を上回る必要がある。コメントを実態に合わせた。値の変更は不要（2 + 5 < 10）。
- 検証: 新規 `terminating_an_agent_asks_before_it_kills` は、`SIGTERM` を処理する子が実際に graceful 終了することを確認する。このテストは変更前の実装では失敗する（kill のみのため `graceful` が空）。既存の `terminating_an_agent_stops_its_process_group` と `an_exited_agent_cannot_leave_a_running_descendant` も通過し、子孫の retire は維持されている。

### 2. プラグイン実行の重複したタスク管理を削除

- 変更: `crates/ene-plugin-runtime/src/instance/supervisor.rs` を削除、`instance/mod.rs` の `mod`/`pub use` を削除。
- eneが持たなくなった責務: インスタンス同時数の管理表。実際の上限は `instance/invocation.rs:34-35` の `SLOTS`（`Semaphore(MAX_INSTANCES)`）が担っており、`Supervisor` は本番経路から参照されていなかった（`grep` で唯一の利用は `tests/embedded.rs`）。
- 削除した型: `Supervisor`、`Tasks`、`GRACE` 定数。
- 検証: `SLOTS` の境界は `instance/lifecycle_tests.rs:294` と `:416` が既に検証している。テスト側の `Supervisor` 依存も削除した。

### 2b. 削除で露呈したテスト基盤の欠陥を修正

`Supervisor` のテストを消した直後、`tests/embedded.rs` の2件が `Unavailable` で断続的に失敗した。原因は今回の変更ではなく、既存の潜在的な欠陥だった。

- 機構: `Instance::load` はプロセス全体で共有する `SLOTS`（`Semaphore(MAX_INSTANCES)` = 16）から permit を取る。テストバイナリはテストを並列実行し、`embedded.rs` は合計19回 `Instance::load` を呼ぶ。16並列で走ると上限を超え、本番経路では起きない理由で `Unavailable` になる。
- なぜ今まで表面化しなかったか: 削除した `Supervisor` のテストが `GRACE`（4秒）待つため、スイート全体を実質的に直列化していた。テスト実行時間が 4.00s から 0.33s に短縮されたのはこのため。
- 反証による確認: ベースライン（変更前）でも `--skip supervisor_shutdown` を付けると同じ2件が失敗した。つまり変更前から壊れており、遅いテストが偶然隠していた。ガードを入れた版は6回連続で通過、外すと4回中1回失敗することを確認した。
- 修正: `tests/embedded.rs` に `ONE_INSTANCE_TEST` ミューテックスを置き、`Instance::load` を含むテストを直列化した。本番の上限は変更せず、テスト側がその上限を尊重する。アサーションは弱めていない。
- 残る制約: このガードはテスト専用で、本番の同時実行上限には影響しない。

### 3. 未使用の上限定数を削除

- 変更: `crates/ene-plugin-runtime/src/limits.rs` から `MAX_EVENTS` と `MAX_SESSIONS`。
- 根拠: ワークスペース全体で参照が定義箇所のみ。`MAX_SESSIONS` は `MAX_INSTANCES / 2` という、どの経路も使わない派生値だった。

### 4. ExtismのマニフェストAPIを直接使う

- 変更: `crates/ene-plugin-runtime/src/instance/mod.rs` の `build`。
- 委譲先: `extism-manifest` 1.41.0 `Manifest::disallow_all_hosts`（`src/lib.rs:304`）。従来の `with_allowed_hosts(std::iter::empty())` を置換。
- 変えなかったもの: `max_var_bytes = Some(0)` の直接代入は残した。`MemoryOptions` は `extism` 1.41.0 から再エクスポートされておらず（`extism-1.41.0/src/lib.rs:40` は `Manifest`/`Wasm`/`WasmMetadata` のみ）、`extism-manifest` を第二のピンとして追加するコストに見合わない。
- 取り下げた変更: `PoolingAllocationConfig::max_memory_size` の削除は行わない。これはマニフェストのページ上限の重複ではなく、pool の1メモリあたり仮想予約量（既定4 GiB、`wasmtime-49.0.5/src/config.rs:4023`）を決める設定で、消すと予約が64 MiBから4 GiBへ増える。Extismの `MemoryLimiter`（`extism-1.41.0/src/current_plugin.rs:26-71`）は実行時の成長だけを制限し、この予約量は制限しない。

### 5. HTTP応答のサイズ制限を `http-body-util` へ委譲

- 変更: `crates/ene-plugin-runtime/src/service/http.rs` の `bounded`。
- 委譲先: `http-body-util` 0.1 `Limited` と `LengthLimitError`。同じ形は既に `apps/ene-host/src/host/net.rs:54` で使われている。
- eneが持たなくなった責務: チャンクを回して累積バイト数を数える自前のループ。`http_body_too_large` と `http_failed` の区別は維持している。
- 依存: `http-body-util = "0.1"` を `crates/ene-plugin-runtime/Cargo.toml` に追加。`Cargo.lock` には既に推移的依存として存在しており、ロックの差分は依存辺1行のみ。新しいクレートは増えていない。
- 検証: `a_reply_larger_than_the_cap_is_refused` と `unknown_length_bodies_are_also_capped` が通過。ブローカーのテスト11件すべて通過。

## 検討したが適用しなかったもの

| 対象 | 判断 |
| --- | --- |
| `AcpAgent`（`process` feature）でプロセス起動ごと委譲 | 5.2.0 に `current_dir` がない（`src/acp_agent.rs:82-88` は `command`/`args`/`env` のみ、`spawn_process` は `:295-297` で `current_dir` を呼ばない）。`env_clear` もない（`:297` は `envs` のみ）ため、隔離方針が壊れ、ホストの資格情報が子へ漏れる。加えて公開された PID 取得口（`:803-809`、`:852` は private）と停止確認の戻り値（`:929-932` は `Result<(), Error>` のみ）がない。ene の `unverified_pids` と `CancellationUnconfirmed` の判定が成立しない。 |
| ACP セッションビルダー（`build_session`/`start_session`）で初期化とセッション生成を置換 | API は存在する（`src/session.rs:165`、`:990`）が、ene は初期化応答を待つ間に到着したプロンプトを保持する必要があり、`start_session` は `session/new` の応答まで呼び出し側を待たせる。現在の mpsc による先取り保持を保ったまま置換すると、接続確立の失敗経路と run の結び付けを作り直すことになり、得るものが差分の削減だけになる。公開契約に触れずに済む範囲で効果が確定しないため見送った。 |
| `acp_bridge` のHTTPプロキシをライブラリへ | 固定依存にプロキシ**サーバー**を提供するものがない。`hyper-util` 0.1.21 はクライアント側のプロキシ支援のみ（`src/client/proxy/`）。汎用部分は約40行で、残りは送信先許可・アドレス固定・ヘッダー除去という ene 固有の方針。新しい依存（TLS/WebSocket スタックを含む）に見合わない。 |
| `wire.rs` のフィールド写しを serde へ | buffa 生成型は proto-JSON 名と snake_case 別名を持つが、ene のドメイン型とは名称も構造も一致しない箇所がある（`Companion.profile`/`state` は proto では `MindProfile`/`MindState` に分かれる）。`..Default::default()` を外す利点はあるが、変換層の置換ではなく移動にしかならず、今回の委譲の目的に合わない。 |
| `db/clients.rs`・`db/settings.rs`・`db/memories.rs` の CAS 事前チェック削除 | 削除不可。SQL 側の CAS は `current` と比較しており、`expected_revision` との比較ではない。事前チェックを外すと、古い `expected_revision` を持つ書き込みが成功し得る。また「存在しない」と「古い」を区別するエラー意味もここで決まっている。 |
| `LastProjectedEntry` を `ConversationChanged` へ | 復元経路が根拠。復元時は変更が通知されない（`rig-ecs-0.46.0/src/session_log.rs:191-193`）ため、`Added<LastProjectedEntry>` による全件再投影が必要。置換すると復元後の通知が消える。 |
| `LastUsage::context()` に `ContextStale` の確認を追加 | 追加しない。rig-ecs 0.46.0 は `ContextStale` を挿入する箇所を持たず（`src/turn.rs:1497` の削除のみ）、ene の会話は追記専用で要約はログを書き換えないため、アンカーが古くならない。存在しない不具合を作って正当化しない。 |
| `cosine_similarity` を `VectorDistance` へ | 置換先は `Vec<f64>` を要求し、ene は索引のメモリを半分にするため `Vec<f32>` を保存している。ビット一致は ene 側テストで既に固定されており、削除できる行数に対して実行時コストが増える。 |
| 音声の i16↔f32 変換を `audioadapter` へ | 変換は1行の式で、置換にはアダプター構築・trait import・失敗可能なサンプル読み出しが要る。行数も複雑さも増えるため、委譲の目的に反する。 |
| `Supervisor` 相当の同時数管理を `Pool` へ | `Pool` は1プラグイン内のインスタンス数を制限するが、ene が制限したいのはアダプターをまたぐプロセス全体の同時実行数（`invocation.rs:33` のコメントどおり、キャンセルされたコンパイルもネイティブコンパイラが返るまでスロットを保持する）。範囲が異なる。 |
| `rusqlite_migration` の導入 | `Cargo.lock` になく、固定依存を増やす判断は本作業の範囲外。現行の `VERSION = 1` はアルファ時点の意図的な選択でもある。 |

## 残る独自処理

以下は ene 固有で、委譲先が固定依存に存在しない。箇所ごとの理由を記す。

- **隔離方針**: `Isolation` の段階と通知（`acp.rs`）、bwrap のコマンドライン生成、継承環境の許可リスト、子が受け取る PATH でのコマンド解決。bwrap は外部プログラムで、ライブラリではない。
- **プロキシの認可**: 送信先の許可リスト、DNS スナップショットの固定、CONNECT と absolute-form の規則、hop-by-hop ヘッダー除去、接続数・ヘッダー数の上限。汎用の HTTP 機構ではなく ene の権限方針。
- **`session/cancel` の送出**: ACP SDK 5.2.0 にヘルパーがない（`src/session.rs:1068-1073`）。`SentRequest::cancel()` は `$/cancel_request` という別の機構。
- **権限要求の拒否**: 承認UIを持たない Alpha では `RequestPermissionOutcome::Cancelled` を返す。ene の製品判断。
- **プラグインABI**: `ene_service`/`ene_emit` のホスト関数、`HttpChunk` のハンドル方式、イベント文法の検証。公開プラグイン契約。
- **実行予算**: `MAX_MESSAGE_BYTES` などの上限値。Extism に対応する設定がないもの、または ene が決める値。
- **VRMの実行時処理**: スプリングボーン、look-at、ノード制約、MToon マテリアル、VRMA リターゲット。`vrm-spec` 0.1.1 はスキーマのみで、Bevy 0.20 対応の VRM ランタイムクレートは固定依存にない（`bevy_vrm1` は Bevy 0.19 向けで、追加してもビルドできない）。
- **記憶の採点式と RRF**: `docs/design/mind.md` §4.1 が定める式。`rig-*` に対応する関数はない。

## 検証

すべて head `6e76e0102f3c03c316437e7457e4140bd1065f68` の作業ツリー上で実行した。

- `cargo fmt --all -- --check`: 通過。
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: 通過。
- `cargo test --workspace --all-features --locked`: 通過。ホスト221件、プロセス2件、Avatar21件、ランタイム47件と結合16件、RPC1件、VRM23件（3件は GPU 必須のため未実行）。失敗0件、終了コード0。
- `cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown`: 通過。
- `python3 scripts/verify-library-delegation.py --self-test` と同スクリプト本体: 通過（63項目、verified 33、rejected 30。前回の台帳は変更していない）。
- 新しい停止経路の検証: `terminating_an_agent_asks_before_it_kills`（SIGTERM を処理する子が graceful 終了する）、`terminating_an_agent_that_ignores_the_ask_still_kills_it`（SIGTERM を無視する子が `TERM_GRACE` 後に kill される。所要 2.01 秒で、待機経路が実際に通っていることを示す）、既存の `terminating_an_agent_stops_its_process_group` と `an_exited_agent_cannot_leave_a_running_descendant`（子孫の retire 維持）。
- 実行していない検証: Windows 実機、GPU 必須のソフトウェア描画テスト、実クラウド接続、`ENE_REFERENCE_WASM`/`ENE_OPENROUTER_WASM` を要する `#[ignore]` 付き結合テスト、実 ACP エージェント（子は報告用フィクスチャであり、外部の実装ではない）。

## 今回のループを終えた理由

ACP、プラグイン実行、音声、VRM、ホスト中核、記憶と圧縮の各機能群について、責務の分類と委譲先の有無を確認した。残った候補は、公開契約の変更を伴うか、固定依存に委譲先がないか、置換によって行数・複雑さ・実行時コストが増えるかのいずれかで、現在の制約内では有効な改善にならない。全面委譲は達成していない。上記「残る独自処理」がその事実を示す。

本稿の主張は「ene固有でない独自処理が残っていない」ではない。残っているものは箇所ごとに理由を記した。数学的に最小であることや、世界中のライブラリを調査済みであることも主張しない。
