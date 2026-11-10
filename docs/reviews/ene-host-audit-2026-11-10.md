# ene-host 実装監査レポート（2026-11-10）

対象は `apps/ene-host`（`apps/ene-host-test` を土台に2026-11-09に書き直した実装）です。このレポートは、問題の所在・根拠・修正方法・受け入れテストを、そのまま修正作業に着手できる粒度で記録します。設計の正本は [アーキテクチャ](../architecture.md)、[心](../design/mind.md)、[作業相手](../design/work.md)、[権限](../design/permissions.md)、[ホストAPI](../api.md)、実装の現況は [ene-host README](../../apps/ene-host/README.md) です。

## 1. 監査の範囲と方法

- 読んだもの：`apps/ene-host` の全ソース、`docs/features` 全体、上記の設計文書、`crates/ene-rpc/proto/host.proto`。
- 照合した一次資料（`~/.cargo/registry` 内のソースと公式ガイド）：
  - rig-ecs 0.46.0（`lib.rs`、`agent.rs`、`prompt.rs`、`tasks.rs`、`session_log.rs`）と、Rig の plugin guide（`rig-harness/PLUGINS.md`）。
  - rig-core 0.46.0（`providers/openai`・`openrouter`、`CompletionRequest`）、rig-memory 0.46.0（`recent`、`compaction`、`DirStore`）、rig-http / rig-reqwest の `runtime`。
  - connectrpc 0.9.1、private-fs 0.2.1、keyring 4.2.0、bevy_app 0.20.0。
- 検証：`cargo test -p ene-host --locked`（18件 + process 1件、全て合格）と `cargo clippy -p ene-host --all-targets --all-features --locked -- -D warnings`（警告なし）を確認しました。
- 実験：`apps/ene-host/src/host/tests.rs` に一時テスト（付録A）を追加して実行し、削除済みです。再現した項目には「**再現済み**」と書きます。コード上の確認だけで再現していない項目は「**未再現**」と書きます。

## 2. 総評

構成は公式の拡張点に忠実です。Rig の `AgentPlugin` / `SessionLogPlugin` を中核に会話の正本を置き、`add_open_tool` + `Decision` + `ToolAccess::Only` でツール境界を作り、`Last` の `.after(WriteLogs)` で通知し、`Time<Virtual>::set_max_delta(60s)` で疎なフレームに備えています。プロバイダー接続も rig-core のベンダー用マクロが展開するのと同じ `OpenAIConfig::with_key(&DIALECT, key).connect(transport)` です。

一方で、**ホスト全体を停止させるデッドロックが1件**（A1）あり、既存テストはそれを検出できていません。さらに、チャット表示と自動記憶に私的な文脈が混入する問題（A2・A3）と、将来の機能追加時に利用者データを守れない設計（A4・A5）があります。まずセクション3のA1〜A5を直すことを推奨します。

| ID | 重大度 | 概要 | 状態 |
| --- | --- | --- | --- |
| A1 | 致命的 | `ControlMemory` が DB の `Mutex` を再ロックしホスト全体が停止 | 修正済み（`try_lock`＋`Tx` 経由の読み取り、回帰テスト） |
| A2 | 高 | 自動整理のモデル出力が `MessageDelta` としてチャットに流れる | 修正済み（`With<TurnCall>` 限定、注入テスト） |
| A3 | 高 | 注入した文脈が履歴APIで「user」発言になり、自動整理にも再投入される | 修正済み（`role_of`、`include_context`、著者フィルタ、`acquisition` 許可リスト） |
| A4 | 高 | SQLite にマイグレーション機構がない | 修正済み（`user_version`＋連番migration、旧DB引き継ぎ、バックアップ） |
| A5 | 高 | コンテキスト窓の管理（Fit・圧縮）がない | 修正済み（段階1＝`Fit` の窓・`max_tokens`・`Notice`、段階2＝落ちた範囲の要約。[設計メモ](../design/compaction.md)） |
| B1〜B12 | 中 | キュー共有、読み取りのtick、テスト用継ぎ目、プロンプト構成など | 修正済み（B9の `WorkView.requests` は予約せず） |
| C1〜C14 | 低 | 小さな不整合・衛生 | 修正済み（C13は現状維持と README に明記、C12は方針のみ） |

## 3. 必須の修正（A）

### A1. `ControlMemory` がホスト全体をデッドロックさせる【致命的・再現済み】

**場所**：`apps/ene-host/src/host/mind/memory.rs:73`（`control_memory`）、`db/mod.rs:189`（`Database::transaction`）。

**現象**：`ControlMemory` RPC を呼ぶと Rig スレッドが永久に戻りません。以後のRPCはキューに積まれ、256件で `resource_exhausted` になります。SIGTERM でも `begin_shutdown` が `Runtime::call` を待つため終了できず、SIGKILL が必要になります。

**原因**：`Database::transaction` は `self.0.lock()`（`std::sync::Mutex`）を保持したままクロージャを呼びます。`control_memory` のクロージャ内で `db.get_memory(..)` と `companion(db, ..)` を呼ぶと、同じスレッドが同じ `Mutex` を再ロックします。`std::sync::Mutex` は再入不可なのでデッドロックします。他の `transaction` 呼び出し（`settings.rs:132,230`、`consolidate.rs:318`、`tools.rs:247`）はクロージャ内で `tx` しか使っておらず、`memory.rs` だけが違反しています。既存テストに `control_memory` を呼ぶものがありません。

**再現**：付録A-1のテストで、`control_memory` が3秒以内に戻らないことを確認しました。

**修正手順**：

1. `db/mod.rs` の `impl Tx<'_>` に読み取りを追加します（`rusqlite::Transaction` は `Connection` に `Deref` するので既存の `get_doc` が使えます）。
   ```rust
   impl Tx<'_> {
       pub(crate) fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
           get_doc(&self.0, key)
       }
   }
   ```
2. `db/fts.rs` で `get_memory` の本体を接続を受け取る関数に切り出し、`Database` と `Tx` の両方から使います。
   ```rust
   fn read_memory(db: &Connection, id: &str) -> Result<Option<MemoryRecord>> { /* 既存の query_row + from_str */ }
   impl Database { pub(crate) fn get_memory(&self, id: &str) -> Result<Option<MemoryRecord>> {
       read_memory(&self.0.lock().map_err(storage)?, id) } }
   impl Tx<'_>   { pub(crate) fn get_memory(&self, id: &str) -> Result<Option<MemoryRecord>> {
       read_memory(&self.0, id) } }
   ```
3. `control_memory` のクロージャを `tx` のみ使う形に直します。
   ```rust
   let memory = db.transaction(|tx| {
       let mut memory = tx.get_memory(&request.memory_id)?
           .filter(|m| m.owner == request.companion_id)
           .ok_or_else(|| not_found("memory not found"))?;
       // ...audience / preference の決定は現状どおり...
       if changed {
           let key = format!("companion:{}", request.companion_id);
           let mut record: CompanionRecord = tx.get(&key)?
               .ok_or_else(|| not_found("companion not found"))?;
           record.memory_revision = next_revision(record.memory_revision)?;
           record.companion.revision = next_revision(record.companion.revision)?;
           record.companion.updated_at = now();
           tx.put_memory(&memory)?;
           tx.put(&key, &record)?;
       }
       Ok(memory)
   })?;
   ```
4. 再発防止として、`Database` の全メソッドが使う `lock()` ヘルパーを作り、`Mutex::try_lock` が `WouldBlock` を返したら `ConnectError::internal("database re-entered")`（`is_storage` にならないエラー）を返します。デッドロックが即時エラーに変わります。`Database` を使うのは Rig スレッド（と起動時の `Credentials::load`）だけなので、競合による誤検知は起きません。

**受け入れテスト**（`host/tests.rs`）：付録A-1を、`timeout` + `mem::forget` を外した形で追加します。
- 記憶を `forget` し、`recall_preference == "forgotten"`、`memory_revision` が1増えることを確認します。
- 同じ操作を2回呼んで2回目は revision が増えないことを確認します。
- `share` → `unshare` で `audience` が `shared` ↔ `private` になることを確認します。
- 別コンパニオンの記憶IDを指定すると `not_found` になることを確認します。

---

### A2. 自動整理のモデル出力が会話ストリームに混入する【高・未再現】

**場所**：`apps/ene-host/src/host/kernel.rs` の `streams`（約334行）、`mind/consolidate.rs` の `start_consolidation`。

**根拠**：rig-ecs のガイドは "Every model call streams, a turn's and a plugin's own alike" と明記しています。`StreamDelta.agent` は side call でも `Some(agent)` です。`streams` は `delta.effect` が `TurnCall` かを確認せず、`(&Owner, &ActiveTurn)` を持つエージェントなら何でも `MessageDelta` を出します。自動整理の `ModelRequest` は `ForAgent(parent)` で `TurnCall` を持ちません。ターン N が確定した直後に整理が始まり、整理の応答中にユーザーがターン N+1 を始めると、整理用JSON（私的記憶の抽出結果）がターン N+1 の `stream_id` で `MessageDelta` として配信されます。

**修正手順**：

1. `streams` の系で効果のクエリを `TurnCall` + `CallOf` に限定します。
   ```rust
   fn streams(
       mut deltas: MessageReader<StreamDelta>,
       calls: Query<(&Effect, &CallOf), With<TurnCall>>,
       turns: Query<&Turn>,
       owners: Query<&Owner>,
       mut offsets: Local<HashMap<Entity, u64>>,
       mut host: ResMut<Host>,
   ) {
       offsets.retain(|call, _| calls.contains(*call));
       for delta in deltas.read() {
           let StreamEvent::Text { text, .. } = &delta.event else { continue };
           let Ok((effect, &CallOf(turn_entity))) = calls.get(delta.effect) else { continue };
           let Ok(turn) = turns.get(turn_entity) else { continue };
           let Some(agent) = delta.agent else { continue };
           let Ok(owner) = owners.get(agent) else { continue };
           // ...以降は既存どおり（stream_id = conversation_id:turn.start、offset、call_id）...
       }
   }
   ```
   `ActiveTurn` に依存しないので、ターン終了直後のフレームに残る delta も正しいターンに紐づきます。
2. 整理の `ModelRequest` はそのまま `ForAgent(parent)` で構いません。ただし将来の `SideCall` が増えても混入しないよう、`streams` は常に `With<TurnCall>` を要求する、とコメントで残します。

**受け入れテスト**：遅延するモックが無いため、ECS 単体で行います。`TurnCall` を持たない `ModelRequest` に対する `StreamDelta` を発行できるテスト用 App を作り、`MessageDelta` が通知されないことを確認します。あるいは `Host.events` を購読した状態でターンと整理を重ねて走らせ、整理のJSON文字列が `MessageDelta.delta` に現れないことを確認します。

---

### A3. 注入した文脈が履歴APIと自動整理に漏れる【高・再現済み】

**場所**：`host/chat.rs:410`（`to_message`）、`host/mind/consolidate.rs:122`（`collect_batch`）、`host/mind/memory.rs:110`（`inject_recall`）、`host/mind/mod.rs:334`（`deliver_state`）。

**現象**：

1. `inject_recall`（「関連する記憶: …」）と `deliver_state`（「現在の心（revision …）」）は、Rig のガイドどおり `DeliveryMode::Inform` + `Author::plugin("mind")` で会話に入ります。ここまでは正しい設計です。
2. しかし `to_message` は `AuthorKind::Agent` だけを `"agent"` にし、それ以外（`Plugin`・`Rig` 含む）をすべて `role: "user"` で返します。クライアントには私的な想起内容とJSON化した心の状態が「ユーザーの発言」として表示されます。既存テスト `a_mind_update_is_stored_and_the_next_turn_recalls_it` は、この表示を仕様として断言しています。
3. `collect_batch` は `chat::entry_text` が空でない全エントリを対象にします。著者の種別を見ません。実験（付録A-2）で、2回目の整理プロンプトに「関連する記憶」が含まれることを確認しました。想起された記憶が再び記憶として抽出され、`acquisition: "direct_experience"` 固定で保存されます。子の報告（`AuthorKind::Agent`）も同じ扱いです。これは [心の設計](../design/mind.md) の出所の分離、および [Alpha計画](../alpha.md) の「由来と隔離」に反します。

**修正手順**：

1. `chat.rs` に役割の決定関数を作り、`to_message` と整理の両方から使います。
   ```rust
   pub(crate) fn role_of(entry: &EntryRef<'_>) -> Option<&'static str> {
       match entry.message {
           message::Message::System { .. } => None,
           message::Message::Assistant(_) => Some("assistant"),
           message::Message::User { .. } => match entry.author.map(|a| &a.kind) {
               None | Some(AuthorKind::User) => Some("user"),
               Some(AuthorKind::Agent) => Some("agent"),
               Some(AuthorKind::Plugin(_)) => Some("context"),
               Some(AuthorKind::Rig) => None, // 停止マーカー等は履歴に出さない
           },
       }
   }
   ```
   `to_message` は `role_of` が `None` のエントリを返さず、`"context"` は `author` ラベル付きで返します（`w::Message.author` は既にあります）。`host.proto` の `Message.role` のコメントに取り得る値 `user | assistant | agent | context` を書きます。`context` を表示するかはクライアント側の判断です。
2. `mind` の想起（`関連する記憶`）をクライアントに見せたくない場合は、`context` 全体を `GetHistory` から除外するオプション（`HistoryRequest.include_context`、既定 `false`）を追加します。proto は既存フィールドを再利用せず、新しいフィールドとして足します。
3. `collect_batch` のエントリ選択に著者条件を追加します。
   ```rust
   let eligible = entry.is_users_own()
       || (matches!(entry.message, message::Message::Assistant(_)) && entry.author.is_none())
       || entry.author.is_some_and(|a| a.kind == AuthorKind::Agent); // 子の報告は別扱い
   if !eligible { continue; }
   ```
   `Plugin`・`Rig` の著者は対象外にします。`is_users_own()` は `rig_core::conversation::EntryRef` の公式メソッドです。
4. `AuthorKind::Agent` のエントリは整理の行頭に `[作業報告]` を付け、プロンプトで「作業報告は `acquisition: "report"`、ユーザー発言は `user_statement`、アシスタント自身の体験は `direct_experience` とする」と指示します。`apply_batch` は `acquisition` を許可リスト（`direct_experience | user_statement | report | shared | inference`、`tools.rs` の `validate_mind_update` と同じ）で検証し、範囲外は `direct_experience` ではなく破棄します。`"direct_experience"` の固定はやめます。
5. 既存テストの断言（「recall note must be in the streamed context」）を、`GetHistory` には `role == "context"` として返る、に直します。

**受け入れテスト**：

- 記憶を事前投入し、想起が起きる2ターンを実行した後、整理プロンプト（`MockCompletionModel::requests()` の `Debug` 出力）の「会話:」以降に「関連する記憶」「現在の心（revision」が含まれない。
- `GetHistory` で想起ノートの `role` が `"context"` である。
- 子の報告を含む会話の整理結果の `acquisition` が `"report"` になる。

---

### A4. SQLite にマイグレーション機構がない【高・拡張性】

**場所**：`host/db/mod.rs:16-40,270-319`。

**現象**：`BASELINE = "2026-11-rewrite-1"` の完全一致だけで起動可否を決めています。`PRAGMA user_version` を使っていません。スキーマ変更時に取れる手段は、(a) `CREATE TABLE IF NOT EXISTS` を足す（列変更・索引変更はできない）か、(b) `BASELINE` を変えて既存DBを拒否する（利用者の記憶と設定の消失）かのどちらかです。[Alpha計画](../alpha.md)の「不意のデータ消去をしない」と衝突します。意味検索（`mind_vectors` / `mind_index_pending`）、スケジュール、キャラクターパッケージなどを入れる前に解決が必要です。

**修正手順**：

1. 接続ごとの `PRAGMA`（`journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`）を `configure(conn)` に分けます。`journal_mode` はトランザクション内で変えられないため、マイグレーションの外に置く必要があります。
2. マイグレーションを連番の配列にします。
   ```rust
   const MIGRATIONS: &[&str] = &[
       include_str!("migrations/001_baseline.sql"), // meta, documents, memories, memories_fts
   ];
   ```
   `001` は現行の `SCHEMA`（PRAGMA を除く）の内容です。
3. `initialize` を次の手順にします。
   - `user_version` を読む（`db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))`）。
   - `user_version == 0` かつ `meta.schema_baseline == "2026-11-rewrite-1"` なら、既存の開発DBとして `user_version = 1` を設定して引き継ぐ。
   - `user_version == 0` かつ他のテーブルがあるなら、現行どおり拒否する。
   - `user_version > MIGRATIONS.len()` なら、より新しいホストのDBとして拒否する（ダウングレードで壊さない）。
   - `user_version` 以降を、**1件ごとに `transaction` + `pragma_update(None, "user_version", n + 1)` + `commit`** で適用する。
   - 2件目以降を適用する前に、`VACUUM INTO` で `host.sqlite3.bak-v{user_version}` を作る（失敗したらそこで起動を中止する）。
4. `host_id` / `environment_id` の生成は `001` の後のコードで、`meta` に存在しなければ入れる処理として残します。
5. `rusqlite_migration`（Cargo のレジストリに存在）を使う選択肢もありますが、上の方式は依存を増やさずに済みます。

**受け入れテスト**：

- 旧ベースライン形式のDB（`meta.schema_baseline` あり、`user_version = 0`）を開くと `user_version = 1` に引き継がれ、`host_id` が保たれる。
- `MIGRATIONS` に2件目（テスト用に `ALTER TABLE` 等）を足して、1→2 の移行が既存データを保つ。
- 途中で失敗する2件目では `user_version` が1のまま、データが変わらない。

---

### A5. コンテキスト窓の管理がない【高・拡張性】

**場所**：`kernel.rs`（`PrepareSystems::Fit` を使うシステムがない）、`chat.rs`、`mind/consolidate.rs`。

**現象**：`features/chat/basic.md` は「会話履歴をホストに永続し、過去のログが破棄された後も長期記憶で継続する」を前提にします。現状は履歴全体が毎回送られるため、長期利用では必ずモデルの文脈長を超え、ターンが `Overflow` で失敗します。[アーキテクチャ](../architecture.md)4節の「固定のトークン数で黙って丸めない」にも未対応です。

**方針**：Rig は `ModelRequest.history` を「保存されない、そのリクエスト専用の会話ビュー」として公開しています。ここを `PrepareSystems::Fit` で刈り込めば、**セッションログ（正本）・`GetHistory`・整理カーソルを一切変えずに**窓を実現できます。段階を2つに分けます。

*段階1：窓（要約なし）*

1. `kernel.rs` に次のシステムを追加します。
   ```rust
   app.add_systems(AgentPass, fit.in_set(PrepareSystems::Fit));

   fn fit(mut calls: Query<&mut ModelRequest, Added<Admitted>>, connections: Query<&Connection>, /* ... */) {
       for mut call in &mut calls {
           let Some(history) = call.history.as_mut() else { continue };
           let spec = /* call.connection or agent の Connection から取る */;
           let Some(context) = spec.context_window else { continue };   // 不明なら窓をかけず、診断を通知
           let budget = context.saturating_sub(spec.max_output_tokens.unwrap_or(8_192)) as usize
                        - prompt_tokens_estimate;
           // rig_memory::recent::{TokenBudget, view} と同じ規則で先頭を決め、
           // history.splice(0..start, [], Author::plugin("ene.window")) で落とす。
       }
   }
   ```
   `rig_memory::recent::view` は「ツール呼び出しと結果の対を壊さない」`safe_start` を内蔵しています。同じ規則で `start` を求めてから `history.splice` を使ってください。API の細部は `rig-ecs/src/turn.rs` の `ModelRequest::history` の説明と `rig-memory/src/recent.rs` を読んで合わせます。
2. `CompletionRequest::max_tokens` を設定します（B6と合わせて、全ての `ModelRequest` で `spec.max_output_tokens` から決める）。現状は出力上限が事後チェック（128,000文字）だけです。
3. 窓で落ちた分が「記憶に整理済みか」を確認できるようにします。整理カーソル（`memory-cursor:{id}.last_sequence`）より新しいエントリが窓の外に落ちる場合は、`Notice` で警告し、整理を優先して実行します。

*段階2：圧縮（要約）*

- `rig_memory::compaction::CompactionPolicy` と `summary_author()` を使い、`ReviewSystems` と `Fit` の組み合わせで要約を作ります（Rig の `rig_basics::compaction` の方式）。
- 事前に次を決めてから実装します。
  - `collect_batch` は `summary_author()` の著者のエントリを対象外にする（A3のフィルタが `Plugin` / `Rig` を落とすなら `summary_author` の `AuthorKind` を確認して同様に除外する）。
  - `GetHistory` は要約エントリを `role == "context"` として返すか、隠すか。
  - 整理カーソルは `MessageId` を使うため、`splice` 後も同じ `id` が保たれることを `rig-memory` のテストで確認する。

**受け入れテスト**：大きな履歴（モック）を作り、(1) モデルに渡るメッセージ数・トークンが上限以下、(2) `GetHistory` は全件を返す、(3) 呼び出しと結果の対が壊れない、(4) 文脈長が不明なモデルでは窓をかけず明示的に通知する、を確認します。

## 4. 重要な修正（B）

### B1. 起床信号とRPCが同じ有界キューを共有している

**場所**：`host/mod.rs:202,225,287`。

**現象**：`Wake` が `sync_channel(256)` に `Signal::Wake` を `try_send` し、`Runtime::call` も同じ送信側に `try_send` します。rig-ecs は wake 1回につき最大4フレーム分の追加 wake を要求します。Rig スレッドが SQLite の fsync（`synchronous=FULL`）などで詰まると、wake がキューを埋め、**`Stop` を含む全RPCが `resource_exhausted` で拒否**されえます。設計は「停止はモデルを待たない制御経路」（[API](../api.md)の5節）を求めています。

**修正**：

1. `crossbeam-channel`（rig-ecs の依存で `Cargo.lock` に入っています）を明示的に依存へ追加します。
2. 3本に分けます：コマンド `bounded::<Job>(256)`、wake `bounded::<()>(1)`（`try_send` の `Full` は合流として無視）、シャットダウン `bounded::<()>(1)`。
3. ループは `crossbeam_channel::select!` で3本を待ち、`recv(shutdown)` を最優先にします（`select_biased!`）。1秒の `recv_timeout` は `default(Duration::from_secs(1))` に置き換えます。
4. `Runtime::call` は従来どおりコマンドキューだけに `try_send` します。wake はコマンドキューを消費しません。

**テスト**：コマンドキューを満杯にしても `Stop` 用の経路（B2の `read`/制御）が通ることは求めません。代わりに、`Wake` を10万回呼んでもコマンドキューの空き容量が変わらないことを確認します。

### B2. 読み取りRPCでも `tick(app)`（`app.update()`）を実行する

**場所**：`host/mod.rs:296`（`Runtime::call`）。

**現象**：`get_snapshot` / `list_*` / `get_history` などの読み取りでも毎回フレームが進み、ターンのロジックと Bevy の `Time` が動きます。

**修正**：`Runtime::read` を追加し、`ready()` 判定と `operation(app)` だけを行い `tick` を呼びません（シグネチャは現行の `FnOnce(&mut App)` を維持して差分を小さくします）。`rpc.rs` の読み取り系（`list_*`、`get_*`、`get_snapshot`、`get_history`、`get_message`、`get_availability`、`get_mind`）を `read` へ切り替えます。書き込み系だけが `call` を使います。

### B3. 読み取り・停止系が未知のコンパニオンのエージェントを生成する【再現済み】

**場所**：`host/chat.rs:42` の `target_entity`（`stop`、`message`、`accept` が使う）。

**現象**：`stop(ghost)` が `Ok` を返し、`parent-ghost` エージェントが生成されます（付録A-3）。`accept` も `bind` が失敗する前にエージェントを作ります。さらにテストでは `factory` が `connect_for` の `companion()` 存在確認を迂回するため、存在しない ID の送信が通ってしまい、検出できません。

**修正**：

1. `target_entity` の先頭で、親なら `mind::companion(&host.db, &target.companion_id)?`（`not_found`）を検証します。作業なら現状の検証を維持します。
2. `stop` と `message` は `find_agent` で見つからなければ生成せず、`stop` は空操作で `Ok`、`message` は `not_found` にします。エージェントの生成は `accept` と `deliver_to_parent`（親は存在が確定している）だけにします。
3. B4でテスト用の `factory` を外すと、存在確認が本番と同じ経路を通ります。

### B4. テスト用の継ぎ目が本番コードに入っている

**場所**：`host/mod.rs`（`Host.factory`、`consolidation_enabled`）、`chat.rs:89-95`（`bind` の `&& host.factory.is_none()`）、`model/mod.rs:263`（`connect_for`）、`tests.rs`（`host.consolidation_enabled = true`）。

README のテスト方針（「テストの配置のために実装を公開したり…しない」）に反し、`Host` が本番では使わない `Option<ModelFactory>` と可変フラグを持っています。

**修正**：

1. モデル供給を小さな trait にします。
   ```rust
   pub(crate) trait ModelSource: Send + Sync {
       fn connect(&self, host: &Host, companion: &str, purpose: Purpose)
           -> Result<(rig_ecs::model::Connection, Binding)>;
   }
   struct Providers;                      // 現行の model::connect
   ```
   `Host.models: Arc<dyn ModelSource>`、`Runtime::start(state)` は `Providers` を使い、`Runtime::start_with(state, Options { models, consolidate })` を `#[cfg(test)]` に限らず `pub(crate)` で用意します（テストモジュールからの利用のみ）。`Scripted`（モック）は `tests.rs` の中だけに置きます。
2. `Options.consolidate: bool` で自動整理の有効・無効を渡し、`Host.consolidation_enabled` の可変代入と `factory.is_none()` による導出を廃止します。
3. `bind` の早期 return 条件から `factory` を外します。`Scripted` が毎回違う `Binding.revision`（`AtomicU64` のカウンタ）を返すようにすれば、早期 return が働かず、本番のコードを変える必要がありません。
4. `Scripted::connect` は companion の存在確認を `Providers` と同様に行います（B3）。

### B5. システムプロンプトの構成が Rig の流儀から外れている

**場所**：`kernel.rs:252`（`prepare`）、`chat.rs:61`（`SystemPrompt(String::new())`）、`work.rs`。

**現象**：`prepare` が**モデル呼び出しのたびに** SQLite からコンパニオンを読み、`prompt.role` を上書きし、`Section` を直接 push します。rig-ecs の `SystemPrompt` は「エージェント自身の部分」、`PromptSection` / `SectionOf` は「エージェント固有のセクション」として用意されています（`prompt.rs`）。ツールループの各反復で DB 読み込みとJSON解析が走ります。

**修正**：

1. `attach_agent`（親のみ）で `SystemPrompt(record.companion.mind)` を設定します。`SystemPrompt` はセッションに保存され、復元時に古い値が戻るので、`attach_agent` は**常に** DB の値で上書きします。
2. `facets` / `examples` / `background` / `expression` は `PromptSection::new(PromptSection::ORDER_ROLE + i, tag, text)` + `SectionOf(agent)` のエンティティにします（変わりにくい順に `order` を昇順にします）。復元されたエージェントは新しいエンティティなので、`restore` と `attach_agent` で生成し直します。
3. `update_companion` / `update_mind_profile` は、変更後に該当エンティティの `text` を `set_if_neq` で更新し、`SystemPrompt` を `insert` し直します（`world` を渡すので `World::get_mut` で行えます）。
4. `kernel::prepare` からプロンプト関連部分（`prompt.role` と `sections` の編集）を削除します。`store:false` の付与だけが残ります。
5. 作業エージェントの `SystemPrompt(WORK_PROMPT + prompt)` は現行のままで構いません。

**テスト**：2回目以降のモデル呼び出しで DB が読まれないこと（`Database` の読み取り回数をテスト用に数えるか、`prepare` を削除した上で送信されたリクエストに作者設定が含まれること）を確認します。

### B6. 自動整理が自由文のJSONを厳密に解析している

**場所**：`consolidate.rs:175-186,244-251`。

**現象**：プロンプトで「JSONだけ」と頼み、`serde_json::from_str(text.trim())` で解析します。コードフェンスや前置きが付くだけで失敗し、`state: "error"` になります。

**修正**：

1. 型を定義します。
   ```rust
   #[derive(Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   struct Extraction { reason: String, memories: Vec<Extracted> }
   #[derive(Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   struct Extracted { content: String, #[serde(default)] description: String,
       #[serde(default)] appraisal: String, acquisition: String }
   ```
2. `CompletionRequest::new(prompt).output_schema(schemars::schema_for!(Extraction)).max_tokens(spec.max_output_tokens)` を使い、モデルの構造化出力に任せます（`CompletionRequest::output_schema` は rig-core 0.46 の公式API、`schemars` は依存済み）。プロンプトからJSON形式の説明を削除します。
3. 解析は `Extraction` への `from_str` にします。保険として、先頭と末尾のコードフェンスを取り除く関数を1つだけ用意します。
4. 整理の失敗は `record_failure` だけでなく `Notice` を出し、カーソルは進めません（現行どおり）。

### B7. `start_consolidation` が作業エージェントを先に拾うことがある

**場所**：`consolidate.rs:92-99`。

**現象**：`agents.iter().find(|(_, owner, _)| owner.companion() == companion_id)` が作業エージェントに当たると `if owner.is_work() || ... { continue }` でキューを捨てます。そのコンパニオンの整理が再起動まで走らなくなります。

**修正**：`find` の条件に `!owner.is_work()` を含めます。`pending.contains(&agent)` で見送る場合は、`consolidation_queue` に戻します。

**テスト**：作業セッションを開始した後でも、親のターン確定で整理が走ることを確認します。

### B8. 接続・許可がLLM専用にハードコードされている

**場所**：`settings.rs:117-126`（`kind: "llm"`、`adapter` が空なら `kind`）、`model/mod.rs`（`Purpose` の閉じたenum、`builtin()`、静的 `PROVIDERS`）、`settings.rs:44`（`data_kinds.any(|v| v == purpose)`）。

**現象**：埋め込み、TTS/STT、検索サービスなどの送信先に Grant をかけるには、`Purpose` と `builtin()` を書き換える必要があります。`data_kinds` は常に `[purpose]` と同値なので、`permissions.md` の「送信するデータの種類・用途・送信先」を区別する意味を持ちません。

**修正**（順序どおりに）：

1. `Provider` に `kind: ProviderKind`（`Llm | Embedding | Transcription | Speech | Search`）を追加し、`descriptors()` と `validate_connection` が `kind` を使うようにします。`ConnectionInput.kind` を無視しないようにし、`adapter` の空文字を `kind` で補う暗黙処理（`settings.rs:88`）を廃止します（`invalid("adapter is required")`）。
2. `Purpose` から用途ごとの必須 `data_kinds` を引く表に変えます。
   ```rust
   const PURPOSES: &[(&str, &[&str])] = &[
       ("conversation", &["conversation"]), ("task", &["task"]), ("memory", &["memory"]),
   ];
   ```
   `GrantRecord::permits` は `grant.data_kinds` が要求される `data_kinds` を**包含する**ことを確認します。`create_grant` / `update_grant` の `data_kinds.iter().any(|k| k != purpose)` 検証は、表の許容値かで判定します。
3. `gate`（`kernel.rs:293`）は現在、モデル・ツールのどの呼び出しも `Binding` を要求します。ツールが外部送信する場合は、ツールごとの `Grant` 要件を `ToolDef` と対応づけて `gate` で検査する方針を `docs/design/permissions.md` に追記してから実装します（Web検索ツール自体は対象外です）。

### B9. 作業バックエンドの境界がない

**場所**：`work.rs` 全体、`WorkRecord.session.backend`（保存のみで分岐に使われない）。

**現象**：`view()` / `report_work()` / `stop()` は内蔵 Rig 子エージェントの `Conversation::turns()` を直接読みます。ACP / MCP / Wasm を足すには全関数の書き換えが必要です。ツールハンドラも `(world, agent: Entity, …)` を取るため、外部の子には代理エージェントが必要です。

**修正**：[作業相手API](../design/work.md) の最小操作（`open / send / cancel / close / get`）に合わせ、**enum 分岐**で境界を切ります（同設計の「型を名前だけ変えた委譲ラッパーを作らない」に従い trait は後回しにします）。

1. `work::Backend { InternalRig }` を追加し、`WorkRecord.session.backend` を解釈します（未知の値は `unavailable("backend not supported")`）。
2. `open`・`send`・`cancel`・`state(&WorkRecord) -> WorkState`（status・runs・last_report）の4関数に切り出し、`start_tool` / `send` / `stop` / `view` / `report_work` はこの関数を呼ぶだけにします。Rig 固有の `Conversation` 参照は `InternalRig` の実装の中だけに閉じます。
3. `WorkState` に `requests: Vec<Request>`（相談）を予約しておき、現状は空にします。proto には後で `WorkView.requests` を新しいフィールドとして足します。

### B10. 作業の完了報告が親を起こさない

**場所**：`work.rs:428`（`deliver_to_parent` の `DeliveryMode::Inform`）。

**現象**：`Inform` は「入力だけでは推論を開始しない」ため、完了報告は次のユーザー発言まで親の推論に入りません。`features/task/request.md` の「完了するとコンパニオンが対話の中で報告します」と、自律実行（`autonomy/*`）の「ホストが起点でターンを始める」経路が成立しません。

**修正**：

1. `deliver_to_parent` を `DeliveryMode::Queue` に変更します（親がアイドルならターンが始まり、実行中なら現在のターン後に入ります）。著者は現状の `Author::agent(work, request_id)` を維持します。
2. 「発言しないことも選べる」（`hosting/README.md`）ために、親のプロンプトに「作業報告に対して、ユーザーに伝える必要がなければ空の返答にしてよい」を追記します。`to_message` は空テキストを返さないので、クライアントには何も表示されません。
3. 自動報告を抑制したい場合のため、`Companion` に `proactive` 設定を持たせるかは別途決めます（まず常時有効で構いません）。

**テスト**：`a_work_session_runs_while_the_parent_keeps_talking_and_reports_once` を拡張し、ユーザー発言なしで親の追加ターン（報告に対する返答）が起きること、二重報告が起きないこと（`reported_run`）を確認します。

### B11. `check_connection` / `list_models` の応答サイズが無制限

**場所**：`model/mod.rs:352`（`probe`）。

**現象**：`.bytes().await` が全量を読みます。`Authorized` の 8 MiB 制限を通らないため、README の「HTTP/ストリームサイズを制限します」が成り立ちません。

**修正**：

```rust
let mut response = client.get(url).bearer_auth(secret).send().await.map_err(..)?;
if !response.status().is_success() { return Err(unavailable("provider is unavailable")); }
if response.content_length().is_some_and(|n| n > MAX_PROBE_BYTES) { return Err(..); }
let mut body = Vec::new();
while let Some(chunk) = response.chunk().await.map_err(..)? {
    if body.len() + chunk.len() > MAX_PROBE_BYTES { return Err(unavailable("provider reply is too large")); }
    body.extend_from_slice(&chunk);
}
```
`MAX_PROBE_BYTES = 4 * 1024 * 1024`。成功ステータスの確認も追加します（現状は 401 本文を JSON として解析して「invalid」になります）。

### B12. `mind_update` の `expected_revision` をモデルが知る手段がない

**場所**：`mind/mod.rs:334`（`deliver_state` が `state.is_empty()` で何も送らない）、`mind/tools.rs:162`。

**現象**：新規コンパニオン（state が空、revision 1）では、モデルは現在のリビジョンを知らされず、推測して呼ぶ必要があります。不一致時のエラー（`mind revision changed`）にも現在値がありません。

**修正**：

1. 不一致時のエラーを `ConnectError::aborted(format!("mind revision changed; current revision is {}", record.state_revision))` にします（`db::aborted` は `&'static str` のため、ここだけ `ConnectError` を直接使います）。
2. `deliver_state` の `is_empty` による早期 return をやめ、空でも `現在の心（revision N）: （まだ何もありません）` を Inform します（`has_request` により revision が変わらない限り再送されません）。

## 5. 軽微な修正（C）

| ID | 場所 | 内容 | 修正 |
| --- | --- | --- | --- |
| C1 | `rpc.rs:82`、`mod.rs:110` | `Notification.companion_id` / `client_id` が設定されるだけで、`Subscribe` はフィルタしない。全クライアントに全コンパニオンのストリームが流れる | `SubscribeRequest` の `client_id`・`companion_id` に基づいてフィルタする。`client_id` 付き通知は該当クライアントだけに送る。不要ならフィールドを削除する |
| C2 | `mod.rs:443` | `max_companions: 16` を広告するが強制していない | `create_companion` で上限を検査するか、広告から削除する |
| C3 | `chat.rs:183` | 未知のエージェントを `world.despawn` する。ログは残り、再起動で復活して再び消される | `world.trigger(Retire { entity })` にする |
| C4 | `chat.rs:236`、README | Rig の追記は `fsync` しない（`rig-memory/src/writer.rs:562`）。SQLite は `synchronous=FULL` | 電源断で受付レシート（Accepted）だけ残る可能性を README に明記する。`Accepted` を返す前に `has_request` を再確認する現状の検査は維持する |
| C5 | `settings.rs:132` | `update_settings` が接続を全置換し、消えた接続のkeyringエントリを削除しない | 消えた接続の `credential_ref` を集め、トランザクション後に `credentials.forget` を呼ぶ。関連する Grant も失効させる |
| C6 | `model/credentials.rs:133` | `OPENAI_API_KEY` が任意エンドポイントの `openai` 接続にも使われる | 環境変数の代替は、接続の `endpoint` が組み込みの既定値と一致するときだけ使う |
| C7 | `client/device.rs:305-312` | 送信してから `DeviceRecord` を保存する。保存失敗で配送済みが未記録になる | `pending` で先に保存し、送信成功後に `dispatched` に更新する |
| C8 | `client/device.rs:273` | モデルが `client_id` を指定でき、アクティブ端末の排他を迂回できる | 引数から `client_id` を削除し、コンパニオンのアクティブ端末だけを使う |
| C9 | `rpc.rs:43` | axum の `DefaultBodyLimit(8 MiB)` は connectrpc の本文読み取りに効かない（connectrpc の既定 `Limits` は 4 MiB） | 層を削除する。必要なら `into_axum_service()` の `ConnectRpcService::with_limits` で上限を設定する |
| C10 | `mod.rs:223` | Bevy の `TaskPoolPlugin` がコア数分のスレッドを起動する。さらに rig-http の1スレッドのフォールバックTokioが自動起動し、ホスト自身のTokioとは別になる | `TaskPoolPlugin { task_pool_options }` でスレッド数を絞る。待機時負荷を測定し、結果を[Alpha計画](../alpha.md)に記録する |
| C11 | `architecture.md` §6 | `models.rs` / `clients.rs` と書かれているが、実体は `model/` / `client/` | ドキュメントを実体に合わせる |
| C12 | `Cargo.toml` | `keyring 4` を `v1` 互換ファサードで使用。公式は `keyring-core` と個別ストアを推奨 | 現状で動作するため、変更は任意。方針を決めて README に書く |
| C13 | `access.rs:50` | トークンが `Uuid::new_v4` 2個の連結 | `getrandom` で32バイトを生成し、base64url にする（現状でも実害は小さい） |
| C14 | `kernel.rs` 全ツール | `ToolOptions::default()`（直列実行、`Writes(Anything)`） | 現状で安全側。読み取り専用ツール（`list_work`、`mind_recall`、`mind_read`）には `footprint: Some(Touches::Reads(Scope::Anything))` を指定する |

## 6. 将来機能の追加余地

`docs/features` の各領域について、設計変更なしで追加できるか、事前に直すべき前提があるかを整理します。ツールシステムへの追加で済むもの（Web検索など）は対象外です。

| 機能領域 | 現状の境界 | 設計変更なしで足せる範囲 | 先に直す前提 |
| --- | --- | --- | --- |
| 基本チャット・長期継続（chat） | 会話はRigのログが正本、`GetHistory` はカーソル付き | 履歴の取得、複数端末、入力の冪等受付 | **A5**（窓・圧縮）、A3（履歴の `role`） |
| キャラクター（character） | `Companion.body/voice`、`CompanionRecord.profile/state` が分離 | パッケージ出力は `companion.mind` + `profile`、状態は含めない設計をそのまま使える | VRM などの大きな資産は `documents`（JSONのみ）に入れられない。ファイル保管（private-fs 配下のBLOB領域）と配信経路（axum の通常ルート）の設計が必要 |
| 記憶・自動整理（memory） | `memories` + `memories_fts`、`audience`、`recall_preference` | 共有記憶プール、忘却、訂正（`supersedes`）、スキル種別（`kind`） | **A1**（`ControlMemory`）、**A3**（由来）、**A4**（意味検索・索引待ちのテーブル追加）、B6、B7 |
| 意味検索・埋め込み | FTS5 trigram のみ。`MemoryStatus.semantic = false` | RRF統合、再検証（`audience` / `recall_preference`） | **A4**、B8（送信先Grantの一般化。`Purpose` に埋め込みを追加） |
| 設定・権限（settings） | `Grant`（purpose / destinations / data_kinds / fingerprint）、`gate`、`Binding.permit` | 接続・送信許可の追加、取り消し（`revoke_and_stop`） | B8（`data_kinds` を `purpose` と独立にする）、Hold/Ask の永続化（下段） |
| 承認・保留・質問（Hold / Ask） | `gate` が1か所にあり、`Decision` を使う | Rig の `Ask` / `HoldOf` を `gate` から起こせる | 保留要求（request id・対象操作・版）をSQLiteに保存し、再起動後に復元する。回答用のRPC（`RespondRequest`）を新しいサービスとして追加する。`ApprovalPlugin`（rig-basics）は回答者不在で拒否するため、そのまま使わない |
| 作業の依頼・調査（task） | 内蔵Rig子エージェント、`WorkSession.backend` | 並列作業、個別停止、結果の保持 | **B9**（バックエンド境界）、**B10**（親の自動報告） |
| 外部ハーネス（ACP / MCP / Wasm） | `Backend` 未実装、ツールは `(World, Entity)` ハンドラ | ACP接続は enum の新しい分岐で追加できる | B9。MCP公開はツールハンドラが `Entity` を取るため、外部セッション用の代理エージェントを作るか、ハンドラを `Owner` 基準にする |
| スケジュール・自発実行（autonomy） | `Deliver` + `Inform` / `Queue`、1秒の起床 | 期限の保存（SQLite）と、期限到来時の `Deliver`（`Queue`）で親のターンを始める | **B10**（`Queue` による起点）、B1（wake の分離）。期限は `Time` ではなく壁時計で比較する。スリープ後の期限超過は一度だけ評価する |
| 自発的な見守り（watch-over） | 端末の能力（`granted_capabilities`）と `device_request` | 端末が画面メタデータを `device_request` の結果として返す | 送信先・データ種別のGrant（B8）。画面キャプチャのクラウド送信は設計課題として継続 |
| 音声（voice） | 未実装 | 新しいサービス（双方向ストリーム）を追加できる（connectrpc は streaming に対応） | B8（TTS/STT の送信先Grant）、`ProviderKind` の追加 |
| ホスト・クライアント（hosting） | Owner Bearer、`client_id`、アクティブ端末、端末操作の `uncertain` | 複数端末、端末オフライン時の保留 | C1（通知のフィルタ）。外部アクセス制御（Tailscale Serve / Cloudflare Access）への移行は `authenticate` を差し替える形で可能。`host-access.md` の検証を完了するまでOwner Bearerは外さない |
| 拡張（extensions） | `Connection.adapter`、`WorkSession.backend` | 新アダプターの追加 | B8、B9 |

## 7. 修正の実施順序

作業は次の順にPRを分けることを推奨します。各PRで `cargo fmt --all -- --check`、`cargo clippy -p ene-host --all-targets --all-features --locked -- -D warnings`、`cargo test -p ene-host --locked`、`cargo test -p ene-host --test process --locked` を通します。

1. **PR1（A1）**：デッドロックの修正、`Tx::get` / `Tx::get_memory` 追加、`lock()` ヘルパー（`try_lock`）、回帰テスト。
2. **PR2（A2・A3）**：`streams` の `TurnCall` 限定、`role_of`、`collect_batch` の著者フィルタ、`acquisition` の許可リスト、`host.proto` のコメントと `HistoryRequest.include_context`、テストの更新。
3. **PR3（A4）**：マイグレーション機構、旧ベースラインの引き継ぎ、バックアップ、テスト。
4. **PR4（B1・B2・B3・B4・B7）**：実行基盤の整理（wake 分離、`Runtime::read`、存在確認、`ModelSource` trait、整理の `find`）。テストの継ぎ目は同じPRで置き換えます。
5. **PR5（A5 段階1・B6）**：`Fit` による窓、整理の `output_schema`、全 `ModelRequest` への `max_tokens`。
6. **PR6（B5・B10・B12）**：プロンプトの `SystemPrompt` / `PromptSection` 化、親の自動報告、`mind_update` の改善。
7. **PR7（B8・B9・B11）**：接続・許可の一般化、作業バックエンドの enum 境界、`probe` の上限。
8. **PR8（C）**：C1〜C14。A5 段階2（圧縮）は別の設計メモ（ADR）を `docs/design` に追加してから実施します。

各PRの完了時に、[Alpha計画](../alpha.md)の実装記録と [ene-host README](../../apps/ene-host/README.md) の「既知の問題」を更新します。

## 付録A：実験用テスト（監査で使用、削除済み）

`apps/ene-host/src/host/tests.rs` の末尾に追加して実行し、監査後に元へ戻しました。修正後は回帰テストとして書き直して構いません。

### A-1（A1：デッドロック）

```rust
#[tokio::test]
async fn control_memory_must_not_deadlock() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), Some(factory("x"))).await.unwrap();
    let companion = default_companion(&runtime).await;
    let c = companion.clone();
    runtime.call(move |app| {
        app.world().resource::<Host>().db.transaction(|tx| tx.put_memory(&memory(&c, "りんご")))
    }).await.unwrap();
    let c = companion.clone();
    let result = tokio::time::timeout(Duration::from_secs(3), runtime.call(move |app| {
        mind::control_memory(app.world_mut(), w::MemoryControlRequest {
            companion_id: c, memory_id: "m1".into(), action: "forget".into(), ..Default::default()
        })
    })).await;
    assert!(result.is_ok(), "control_memory must return"); // 修正前は失敗する
}
```
`memory(owner, content)` は `MemoryRecord`（`id: "m1"`、`audience: "private"`、`recall_preference: "normal"` など）を作る補助関数です。

### A-2（A3：整理に想起が混入）

モデルを `[turn1 応答, 整理JSON(空), turn2 応答, 整理JSON(空)]` の順に設定し、`host.consolidation_enabled = true` にします。事前にDBへ「りんごが好き」の記憶を入れ、2回目の送信で想起が起きるようにします。最後に `model.requests()` を `Debug` で文字列化し、整理プロンプト（「以下は保存済みの会話」を含む）の「会話:」以降に「関連する記憶」が含まれるかを調べます。監査時は2回目の整理（request#3）で `true` でした。

### A-3（B3：未知IDのエージェント生成）

`chat::stop(app, Target { companion_id: "ghost" })` が `Ok` を返し、`Query<(&AgentId, Option<&Owner>)>` に `("parent-ghost", true)` が現れることを確認しました。`chat::accept` でも `ghost2` の `parent-ghost2` が生成されました。
