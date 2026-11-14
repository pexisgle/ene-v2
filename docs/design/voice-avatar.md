# 知覚（目）・身体表現・音声パイプライン共通仕様

状態：Alphaの採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[技術スタック](tech-stack.md)、[作業相手API](work.md)、[音声機能仕様](../features/voice/README.md)、[デスクトップクライアント実装](desktop-client.md)を参照する。

この文書は、ホストとすべてのクライアントに共通する「目」「体」「口と耳」の契約を定義する。デスクトップ固有のプロセス構成、OSフック、Bevy/VRM描画、音声デバイス実装は[デスクトップクライアント設計](desktop-client.md)に分離する。

---

## 1. 目的と責務境界

デスクトップ常駐AIコンパニオンにおいて、「沈黙の壁」（話し終えてから数秒間固まり、長文を一気に喋る従来のターン制カスケード）は実在感を大きく損なう。
本設計は、環境を常時センシングする**「目」（知覚 / Perception）**、発話と独立していつでも動ける**「体」（リアルタイムモーション / Body）**、そして自然なテンポ・先行合成・割り込み停止を可能にする**「口と耳」（プラガブル音声パイプライン：参考 Shizuku-Duplex）**の協調仕様を定義する。

本設計は、クライアントが宣言する能力と、ホストが配送する操作の契約として記述する。ホストは特定の描画エンジン、OS、音声デバイスを前提にしない。

```text
[クライアント]
  表現エンジン / 知覚センサー / 音声入出力
  capability_specs の宣言
        │  Connect-RPC（能力宣言・DeviceOperation・音声ストリーム）
        ▼
[ホスト（頭）]
  会話・記憶・作業のオーケストレーション
  能力に応じたツール公開 / DeviceOperation の配送 / 知覚要約
```

### 1.1 クライアント能力と操作の契約
- **能力の宣言**: クライアントは`ClientService.RegisterClient` / `SetCapabilities`で、`capabilities`（能力名）と`capability_specs`（説明、入力JSON Schema、プリセット、ツール名）を宣言する。
- **能力の公開**: ホストは宣言された能力を許可に照らして検証し、`tool_name`を持ち、かつ許可された能力だけを親エージェント向けの専用ツールとして公開する。能力の宣言は存在を示すだけで、実行の可否は許可で別に判定する。
- **操作単位の承認**: 能力の許可は包括承認ではない。`ClientCapabilitySpec.approval`が`CONFIRM`の能力は、操作ごとにホストの`Stage::Gate`と`ApprovalRequest`を通す。`AUTO`でもハード禁止や明示的毎回確認ポリシーを迂回できない。
- **操作の配送**: ホストは具体的な実装を持たず、`ClientService.DeviceStream`上の`DeviceOperation`として能力名と入力を配送する。クライアントは自分の表現エンジン・センサー・デバイスで実行する。
- **対象端末の束縛**: モデルは対象端末を指定しない。ホストが受付時に`client_id`と`activation_revision`を束縛し、受付後にアクティブ端末が切り替わったら`client_moved`として失効させる。別端末へ自動追従しない。
- **音声出力の所有**: 音声出力を担当するアクティブクライアントが、再生デバイスと単一の再生ストリームを所有する。複数クライアントでの同時再生を前提にしない。
- **デスクトップ実装**: デスクトップクライアントと`apps/ene-avatar`の構成は[デスクトップクライアント設計](desktop-client.md)を参照する。

能力宣言と`DeviceOperation`の詳細は[ホストAPI](../api.md#クライアント能力とデバイス操作)を参照する。

---

## 2. 「目」（知覚 / Perception）

### 2.1 ハイブリッド知覚アーキテクチャ
リソース消費を最小限に抑えつつ作業変化を捉えるため、クライアントの知覚センサーはイベント駆動の検出と定期サンプリング（15〜30秒）を併用できる。採用する検出技術はクライアントのプラットフォームに依存する。

- クライアントは`ClientService.ReportPerception`で知覚イベント（`PerceptionEvent`）をホストへ送信する。
- イベントには、アクティブウィンドウの変化（`ActiveWindowChanged`）、ユーザーの活動状態（`UserActivityState`）、クライアントが生成した要約（`PerceptionSummary`）がある。
- ホストは受信したイベントを簡潔な知覚要約（Perception Summary）に変換し、「頭」の動的文脈枠へ注入する。
- OS別の検出技術の例（LinuxのX11/Wayland、Windowsの`SetWinEventHook`、macOSのAccessibility API等）は[デスクトップクライアント設計](desktop-client.md)を参照する。

> [!NOTE]
> **知覚情報の責務分担**:
> OSウィンドウ監視で取得できるのは「どのアプリが最前面で、どんなタイトルか」という表層情報に限られる。
> エディタで開いている具体的なファイルパスやプロジェクト情報、ブラウザのURL等は、VS Codeやブラウザの専用連携プラグインから直接取得し、作業エージェントの活動状態はWorkSessionから直接受け取る。これにより、ウィンドウタイトルの不確実な解析に過度に依存しない。これらの専用連携もクライアント側の知覚センサーとして実装する。

### 2.2 セマンティック知覚要約と推測の分離
- **客観的事実と解釈の区分**:
  - ウィンドウタイトルやプロセス名は「観測された事実」として扱う。
  - 「VS Codeでコーディング中」などの文脈推測は、ホスト側で簡潔な要約（Perception Summary）として整形し、取得できなかった項目は不明とする。
- **配置位置**: プロンプトのPrefix Cacheを保護するため、最新ターンの直前（動的文脈枠）へ注入する。

### 2.3 動的Watcher（`set_eye_watch`）と優先割り込み
親（頭）は対話やタスクの文脈に応じて、特定の監視条件を「目」に登録できる。動的Watcherは能力ベースで提供する。

1. クライアントは`eye.watch` capabilityを`ClientCapabilitySpec`で宣言する。仕様には監視ルールの入力スキーマを含める。
2. ホストは、`eye.watch`を宣言し許可されたアクティブクライアントがある間だけ、親エージェントへ`set_eye_watch`ツールを公開する。
3. 親が`set_eye_watch`を実行すると、ホストは`DeviceOperation { capability: "eye.watch", input: <ルールJSON> }`を`ClientService.DeviceStream`で配送する。
4. クライアントは自身のセンサー実装（OSネイティブフック、アプリ連携等）のフィルタ条件を更新し、条件合致時は定期サンプリングを待たずに**即時優先割り込みイベント（Priority Interrupt）**を`ReportPerception`で返す。これにより親のリアクション推論が起床する。
5. `eye.watch`を宣言しない、または許可されないクライアントでは、ホストは`set_eye_watch`を公開せず、受動的な知覚イベントのみを利用する。

デスクトップクライアントが宣言するスキーマの例は[ホストAPI](../api.md#3-公開状態)を参照する。ホストはルールの意味を解釈せず、能力名とスキーマに従って配送する。

---

## 3. アバター描画・身体表現・リップシンク（Body連携）

アバターの描画と身体表現はクライアントの表現エンジンが担当する。ホストは能力名・入力スキーマ・プリセットだけを保持し、描画や再生の実装を持たない。

### 3.1 発話と独立した単独モーション直接制御
親（頭）は発話音声（ストリーミング出力）の有無にかかわらず、**`body_motion`ツールをいつでも単独で呼び出すことができる**。ただし、このツールは`body.motion` capabilityを宣言し許可されたアクティブクライアントがある場合に限り公開される。

- クライアントは利用可能なモーション・表情のプリセット（例: 傾聴中の頷き、作業画面注視時の首傾げ、アイドル時の伸び）を`ClientCapabilitySpec.presets`に含めて宣言する。
- 親が`body_motion`を呼ぶと、ホストは`DeviceOperation { capability: "body.motion", input: <モーション・表情JSON> }`を送出する。
- モーション名・表情名の解釈、クロスフェード、物理演算はクライアントが行う。ホストはBevy、VRM/VRMA、ウィンドウ制御などの実装を知らない。
- 再生できないモーションはクライアントが無視または近似してよく、音声パイプラインや会話を止めてはならない。

### 3.2 クライアント側RMS音量解析によるリップシンク
- サーバー側での音素（Viseme）解析やネットワーク同期を排し、**クライアントが受信した音声PCMデータのRMS（実効値）振幅をローカルで直接計測**してブレンドシェイプ（`aa` 等）を変位させる。
- サーバー往復遅延がなく、音声再生と同一フレームで口パクが連動する。
- RMS計測の実装はクライアントの音声バックエンドに依存する。デスクトップクライアントでの実装例は[デスクトップクライアント設計](desktop-client.md)を参照する。

### 3.3 表現の実行責任
- クライアントは、受信した`DeviceOperation`を自身の表現エンジンに適合する形で実行する。
- ホストは特定のモデル形式、描画ライブラリ、ウィンドウシステムを要求しない。
- 表現能力を持たないクライアントは、`body.motion`を宣言しなければよい。宣言しても許可されなければ、ホストはそのクライアントに対して身体ツールを公開しない。

---

## 4. 音声対話パイプライン設計（プラガブルエンジン適応）

### 4.1 LLMテキストストリーム直接接続による先行合成
- 発話全文の生成完了を待つのではなく、**LLMの出力トークンストリームから文末記号（`。`, `！`, `？`, 改行）を検出した順に即座にTTSエンジンへ投入**する。
- **フィルタリング原則**: `Stage::Review`（AgentPass 内）で `MessageReader<StreamDelta>` を購読し、`Query<(&Effect, &CallOf), With<TurnCall>>` により通常の会話ターン呼び出し（`TurnCall` マーカー付き）であることをフィルタし、かつ `matches!(delta.event, StreamEvent::Text { .. })` で思考トークン（Reasoning）を除外した上で、親コンパニオンの発話テキストのみをTTSへ流す（Compaction要約、記憶整理、子エージェントログ等は完全に遮断）。
- **未完了文の終端フラッシュ**: LLMの生成が文末記号なしで完了（`end_turn`）した場合、文蓄積バッファに残っているテキストを即座に最終文としてフラッシュし、TTSへ投入する。
- **並行合成バッファ上限**: 先行合成キューの滞留を防ぐため、再生待機バッファの最大文数は **3文** までとし、再生速度に応じたバックプレッシャーをかける。
- **合成失敗時の明示的スキップ通知（SentenceSkipped）**:
  - 特定の文（第 $N$ 文）の合成が失敗・タイムアウトした場合、ホストはクライアントへ即座に `SentenceSkipped { utterance_id, generation_id, sequence_no, reason }` を送信する。
  - クライアントは文番号順の待機キューにおいて第 $N$ 文の欠番確定を認識し、待機を即座に解除して第 $N+1$ 文の再生へ安全に進める（クライアント側のデッドロック防止）。
  - クライアントは再生完了報告（`ReportPlaybackStatus`）において、実際に再生された文番号一覧（`played_sequence_nos`）とスキップされた文番号一覧（`skipped_sequence_nos`）を明示報告し、ホスト側でどの文が伝達されたかを完全に復元可能とする。
- **発話送信完了（UtteranceFinished）と再生完了報告（ReportPlaybackStatus）の厳格な分離**:
  - LLMの応答テキスト生成およびTTSパイプラインの全入力処理（PCMチャンク送信、または `SentenceSkipped` 送信）が完了した際、ホストはクライアントへ発話送信完了イベント `UtteranceFinished { utterance_id, generation_id, total_sentences }` を送信する。
  - **二段階の完了確定契約**:
    | 通知イベント | 送信元 ➔ 送信先 | 確定すること |
    | :--- | :--- | :--- |
    | **`UtteranceFinished`** | ホスト ➔ クライアント | **ホスト側の送信完了**: ホストは、この発話に対する追加の音声チャンクやスキップ通知をもう送らない |
    | **`ReportPlaybackStatus(COMPLETED)`** | クライアント ➔ ホスト | **クライアント側の再生完了**: クライアントは、この発話のすべての音声再生処理を終えた |
  - **クライアントの通常再生完了報告条件**:
    1. `UtteranceFinished` を受信済みであること
    2. `0 .. total_sentences` のすべての文が、再生完了またはスキップ確定（`SentenceSkipped` 受信）となっていること
    3. 当該発話の再生中音声および再生待ちバッファが完全に空（消化完了）になっていること
  - クライアントは上記条件をすべて満たした時点で初めて `ReportPlaybackStatus(PLAYBACK_COMPLETED)` をホストへ報告する。これにより、送信完了が再生速度より早く届いた場合でも、再生中の音声が途中で打ち切られる事故を防止する。
  - **例外（即時終端）**: 全文がスキップされた場合や `total_sentences == 0` など、再生対象の音声データが存在しない場合に限り、クライアントは `UtteranceFinished` 受信直後に終端（`ReportPlaybackStatus` 返答）して待機状態へ遷移できる（未到着チャンクを待ち続けるハングの防止）。
- 第1文の音声合成と再生が開始されている間に第2文以降を並行合成することで、初声レイテンシ（Time to First Audio）を劇的に短縮する。
- 身振り・表情は`body_motion`ツール（`body.motion`能力）を通じて独立して非同期に送出される。

### 4.2 TTSエンジンの特性に応じた実装パターン

```rust
pub struct TtsCapabilities {
    pub chunk_mode: TtsChunkMode, // Sentence(文単位), Phrase(文節), FullText(全文)
    pub supports_streaming_audio: bool,
    pub supports_cancellation: bool,
}
```

1. **パターンA: 文単位 HTTP API エンジン（VOICEVOX / Style-Bert-VITS2 等、Alpha標準）**
   - ストリームから切り出した文ごとに非同期でHTTPリクエスト。
   - 完成したWAV/PCMをクライアント再生キューへ順次投入。
2. **パターンB: ネイティブ・ストリーミングエンジン（sherpa-onnx / ElevenLabs 等、拡張）**
   - 読点や文節ごとに逐次ストリーム投入し、合成完了したPCMチャンクを順次配信。
3. **パターンC: 双方向リアルタイム Live API（OpenAI Realtime / Gemini Live、拡張）**
   - 単なるTTSエンジンではなく双方向会話バックエンドとしてアダプターを設計。

### 4.3 世代管理（Generation ID）と再生順序・音声フォーマット制御

レビューで指摘された「Flush後の遅延到着再生」「文完成順の逆転」「音声フォーマット不整合」を防ぐため、以下の順序・世代管理を厳格に実装する。

- **標準音声フォーマット（Wire Standard）とクライアント側変換**:
  - 全エンジンからの音声ストリームは **24,000 Hz / 16-bit signed Little-Endian PCM（`pcm_s16le`）/ 1 チャンネル（モノラル）** に統一して配信する。
  - クライアントは自身の音声デバイスに合わせてリサンプリング・チャンネル変換を行う。デスクトップクライアントでの`cpal` / `rubato`を用いた実装例は[デスクトップクライアント設計](desktop-client.md)を参照する。

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioChunkEnvelope {
    pub utterance_id: String,
    pub generation_id: u64,
    pub sequence_no: u32,
    pub pcm_data: Vec<u8>, // pcm_s16le mono 24kHz
    pub sample_rate: u32,  // 24000
    pub is_final_chunk: bool,
}
```

- **割り込み（Barge-in）時の旧世代破棄**:
  1. ユーザーの割り込み発話を検知した際、ホストは進行中のLLM生成およびTTSタスクをキャンセル。
  2. クライアントへ `FlushBuffer { utterance_id, current_generation_id }` を送信。
  3. クライアントは再生中バッファを瞬時に破棄し、**`generation_id <= flush_generation_id` の遅れて届いた音声チャンクをすべて無視・破棄**する。
- **文単位並行合成の順序保証**:
  - 各文には文頭からの連番（`sequence_no: 0, 1, 2...`）が付与される。
  - 短い第2文が長い第1文より先に合成完了した場合でも、クライアントは `sequence_no` 順に整列して待機させ、順序逆転再生を防止する。
- **音響エコー（Acoustic Echo）対策と発話割り込み（Barge-in）の境界**:
  - **VADとエコーキャンセレーション（AEC）の責務分離**:
    - VAD（Voice Activity Detection）は「音声信号が存在するか」を判定するものであり、「その声が人間のユーザーか、スピーカーから再生されたコンパニオン自身の声か」を識別することはできない。
    - したがって、スピーカー再生中に単純なVADのみで割り込みを判定すると、自身のTTS音声をマイクが拾って即座に自発話を誤中断（自己割り込みループ）させる。
  - **環境に応じた割り込み方式の段階的制御**:
    1. **参照信号AEC（Acoustic Echo Cancellation）利用環境**:
       - OS・ハードウェアのAEC、または再生PCMを参照信号として減算するWebRTC APM等のエコーキャンセラが有効な環境では、スピーカー音声を打ち消した残差信号から人間の割り込み音声を検出し、自動Barge-in（`FlushControl`）を発動する。
    2. **AEC未検証・スピーカー環境（フォールバック）**:
       - ソフトウェア単体で再生参照信号との同期エコー抑制が保証できない環境では、**マイク入力による自動Barge-inを無効化**し、自発話による誤中断を防止する。
       - この場合の割り込みは、GUI上の停止ボタン、キーボードショートカット（Esc等）、プッシュ・トゥ・トーク、またはヘッドフォン／イヤホン利用時（マイクへの音響回り込みがない環境）に限定する。
  - **音声承認の厳格な発話束縛契約**:
    - クライアントはマイク音声送信時（`AudioInputChunk`）、再生中であるかを示すフラグ `is_companion_speaking` を付与する。
    - ただし、`is_companion_speaking = false` はクライアント側の再生停止状態を示すソフトウェアフラグであり、マイク入力の音響的静寂を保証するものではない。
    - したがって、スピーカーからの回り込みによる自己承認事故を防止するため、**「コンパニオン再生中（`is_companion_speaking == true`）に収録・バッファリングされた音声フレームは、権限承認の判定対象から完全に破棄・除外」**する。
    - 高権限な音声承認（「いいよ」「実行して」等）は、**再生が完全に停止した後に新しく開始（発声検知）されたユーザー発話にのみ厳格に束縛**して受け付ける。再生中の残響や遅延フレームから承認を確定することは禁止する。

---

## 5. 複数コンパニオンの調停（Multi-Companion Arbitration）

複数体のコンパニオンが同時に常駐している場合の調停ルール。

```text
                     [ ユーザーの音声発話 ]
                               │
                               ▼
                        [ STT 音声認識 ]
                               │
                ┌──────────────┴──────────────┐
                ▼ (名前でフィルタせず同報配信)  ▼
        [ コンパニオン A ]              [ コンパニオン B ]
        自身の人格・文脈照合            自身の人格・文脈照合
       「私宛てだ！話そう」            「私宛てではないな」
                │                               │
                ▼                               ▼
       テキスト生成・音声開始                   noop()
       （「了解だよ！」）
                │
                ├───────────────────────────────┐
                ▼ work_started 通知             ▼ (受信)
         [ 同一クライアントの             「Aが作業を開始した。
           他コンパニオンへ同報 ]          重複作業は控えて待機」
                │
                ▼ start_work 実行開始
         [ 音声再生開始 ]
                │
                ▼ クライアントから ReportPlaybackStatus(COMPLETED)
         [ speech_completed イベント同報 ]
                │
                └───────────────────────────────┐
                                                ▼
                                        [ コンパニオン B ]
                                        「Aが話し終えた。補足しよう」
                                                │ (上限5往復まで)
                                                ▼
                                        テキスト生成・音声開始
                                        （「私も手伝うね！」）
```

### 5.1 STT同報配信と自律的該当性判断
- 音声認識テキストは、アクティブな全コンパニオンへ等しく同報配信される。
- 各コンパニオンは自身のシステムプロンプトと文脈から「自分宛てか」を自律判断し、無関係なら `noop()` を返して静観する。

### 5.2 会話上の自律性と不可逆操作権限の分離（原子的な受付登録の獲得）
- **会話上の自律性と副作用実行権の明確な分離**:
  - コンパニオン同士の日常雑談、相談、譲り合いはLLMが自律的に行う。
  - しかし、**メール送信・ファイル確定・外部API呼び出し等の取り消せない外部副作用を伴う同一タスクの着手**については、「後発側が気づいて自律キャンセルする」という緩い前提に依存せず、ホストが入力要求由来のID（`transcription_id`）に基づく**原子的受付登録（Atomic Claim）**により一意に担当を確定する。
- **タスク着手時の原子的受付登録（`try_claim_delegation`）とキー自動束縛**:
  - `start_work` 呼び出しにおける `delegation_key` は、LLMに引数として生成させない（異なる文字列が生成されて排他が効かなくなるのを防ぐ）。
  - **ホスト側で「同一の音声認識イベント（`transcription_id`）に紐づくターンでの `start_work` 呼び出しは、ホストが `transcription_id` を自動で `delegation_key` に束縛」**する。
  - 実務指示を開始する際、コンパニオンはホストに対して委託受付を登録する：
    ```rust
    // ホスト内の同期・原子的判定 (transcription_id を delegation_key として自動利用)
    if host.try_claim_delegation(&delegation_key, companion_id) {
        // 先着で権利を獲得：作業エージェントの起動と不可逆操作へ進む
        notify_others_work_started(companion_id, delegation_key);
        start_work(...);
    } else {
        // 既に他コンパニオンが着手済み：作業を控え、サポートまたは会話待機へ移行
        yield_to_peer();
    }
    ```
  - これにより、複数コンパニオンが同時に同一指示を受け取った場合でも、外部副作用（メール2重送信など）が先行して実行される競合リスクを決定論的に排除する。
  - ユーザーが明示的に「2人とも調べて」と複数並行委託を指示した場合は、指示ごとに個別の `delegation_key` が発行され、安全に並行実行される。
- **発話権（Floor）の先着基準と負けた側のターン出力破棄**:
  - 作業担当の受付登録とは独立して、**ホスト上で最初にユーザー向け応答テキストの生成（最初の文ストリーム）を開始・受付させたコンパニオンが発話権（Floor）を獲得**し、クライアントへの音声配信ストリーミングを行う。
  - **負けた側のターン出力破棄規約**: 先着でフロアを獲得できなかった側のコンパニオンについては、ホストが `world.trigger(StopTurn::new(peer_entity))` により推論を即座に中断・破棄する。これにより、「言いかけて言わなかった応答」が未発話のまま `SessionLog` に残って会話記憶が汚染される事故を防止する。
  - 音声フロアの解放はクライアントの再生完了報告（`ReportPlaybackStatus`）またはタイムアウトにより行われる。

### 5.3 会話リレー（`speech_completed`）と再生制御・伝達進捗
- **クライアント側での単一再生物理保証と旧発話の完全失効**:
  - 音声再生を担当するクライアントは「同時に再生できる音声ストリームは常に1件のみ」という不変条件を保持する（再生デバイスの排他所有）。
  - 新しい発話（新しい `utterance_id`）の音声チャンク、または割り込み（`FlushControl`）を受信した場合、クライアントは現在再生中の旧音声を即座に停止・フェードアウトし、音声の重複再生を物理的に防ぐ。
  - **旧発話の失効（逆戻り防止）**: 新しい発話へ切り替えた時点で以前の `utterance_id` はすべてクライアント側で失効扱いとなる。遅れて届いた旧発話のチャンクや旧発話宛てのFlushControlはすべて破棄され、再生中の新発話を停止させたり旧発話へ巻き戻したりしない。
- **再生進捗の報告と未再生部分の伝達整合性**:
  - クライアントは、`UtteranceFinished` 受信（送信完了）後にすべての文の再生完了またはスキップが確定し、再生バッファが空になった時点で `ReportPlaybackStatus(PLAYBACK_COMPLETED)` を報告する（割り込み時や失敗時は `PLAYBACK_INTERRUPTED` / `PLAYBACK_FAILED`）。
  - 報告には**「実際に再生完了した文数（`completed_sentence_count`）およびPCMバイト数（`completed_pcm_bytes`）」**を正確に含め（0は1文も完了していないことを明確に表す）、ホストは中断された発話について、未再生部分を「ユーザーに伝達済み」と誤認せず、会話履歴や要約文で事実と異なる認識が生じるのを防ぐ。
- **再生完了連携と会話リレー**:
  - クライアントでの発話再生完了（`ReportPlaybackStatus`）時、ホストは発話権を解放し、他の全コンパニオンへ `speech_completed` イベントを同報して自然な掛け合いを誘発する。
- **掛け合い上限（最大5往復）**:
  - コンパニオン同士の掛け合いが無限ループするのを防ぐため、ユーザー入力なしでの自律掛け合いリレーは**最大5ターン（往復）**で自動打ち切りとする。
- **通信途絶時の自動フロア解放と遅延報告の無視**:
  - クライアントのクラッシュや切断で完了報告が届かない場合に備え、ホストは「合成音声のPCM再生時間 ＋ 3.0秒」が経過した時点でフォールバックとして発話権を解放する。
  - タイムアウト後に遅れて届いた旧 `utterance_id` / `generation_id` の完了報告は安全に無視・破棄され、現在の別発話の発話権や会話リレーを誤って二重解放・誤作動させない。

### 5.4 端末間移動（Activate）時の音声・承認の失効
コンパニオンのアクティブクライアントが別端末へ移動し、`Activation.revision`が進んだ場合、ホストは旧クライアントの進行中処理を移動の瞬間に失効させる。

1. 旧クライアントが接続中なら、音声出力へ`FlushControl { reason: FLUSH_REASON_CLIENT_MOVED }`を送り、旧発話の再生バッファと再生所有権を失効させる。ホストは発話権も解放し、旧端末からの`ReportPlaybackStatus`で新しい発話のフロアを解放しない。
2. 旧`activation_revision`に束縛された`AudioInputChunk`、`PlaybackStatusReport`、`PerceptionEvent`、音声承認をすべて破棄する。旧端末から遅れて届いた結果やSTT確定は、新しい端末の発話・承認・フロア制御に適用しない。
3. 新しいクライアントは、旧端末の再生キュー、未決の承認バインド、`is_companion_speaking`の状態を引き継がない。移動後の音声は新しい`activation_revision`で束縛する。
4. 単なる切断・再接続は移動と区別する。切断では進行中の再生を`uncertain`として扱い、同一クライアントが同じ`activation_revision`で再接続した場合のみ状態を回復できる。再接続時は`GetSnapshot.activations`または`Subscribe`の`activation_updated`で現在のrevisionを取得してから、`SubscribeAudioRequest`と`AudioInputChunk`に付与する。`Activate`による移動は明示的な失効であり、旧端末の再接続で復活させない。

---

## 6. Connect-RPC サービス定義 (`voice.proto`)

> [!NOTE]
> `VoiceService` は `crates/ene-rpc/proto/host.proto` と共通の `ene.host.v1` パッケージに属する。
> 双方向ストリーミング RPC（`StreamAudioInput`）は Connect-RPC / gRPC の仕様上、Alphaでは HTTP/2 トランスポート（または h2c）を必須とする。ブラウザの Connect-Web 等、双方向ストリーミングを利用できない環境向けには、チャンク分割アップロード等の代替経路を将来許容する。Server Streaming の `SubscribeAudioOutput` は HTTP/1.1 Server-Sent Events / Chunked でも動作可能である。

```protobuf
syntax = "proto3";
package ene.host.v1;

service VoiceService {
  // マイク音声ストリーム入力（クライアント -> ホスト）
  rpc StreamAudioInput(stream AudioInputChunk) returns (stream TranscriptionEvent);

  // 音声ストリーム購読（ホスト -> クライアント Server Streaming）
  rpc SubscribeAudioOutput(SubscribeAudioRequest) returns (stream AudioOutputEvent);

  // クライアントからの再生状態報告（再生完了・中断・失敗、実再生進捗）
  rpc ReportPlaybackStatus(PlaybackStatusReport) returns (ReportPlaybackStatusResponse);
}

message AudioInputChunk {
  string client_id = 1;
  bytes pcm_data = 2; // pcm_s16le mono 16kHz / 24kHz
  uint32 sample_rate = 3;
  bool is_companion_speaking = 4; // 再生中フラグ
  uint64 activation_revision = 5; // 送信時の有効化revision
}

message TranscriptionEvent {
  string transcription_id = 1;
  string text = 2;
  bool is_final = 3;
  bool is_speaking = 4; // VADによる音声活動検出フラグ
}

message SubscribeAudioRequest {
  string client_id = 1;
  uint64 activation_revision = 2; // 購読時の有効化revision
}

message AudioOutputEvent {
  oneof event {
    AudioChunkEnvelope chunk = 1;
    FlushControl flush = 2;
    SentenceSkipped skipped = 3;
    UtteranceFinished finished = 4;
  }
}

// 発話送信完了（ホスト -> クライアント）: ホストはこの発話に対する追加の音声・スキップをもう送らない
message UtteranceFinished {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 total_sentences = 3; // 発話内で生成された総文数（スキップ分含む）
}

message SentenceSkipped {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 sequence_no = 3;
  string reason = 4;
}

message AudioChunkEnvelope {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 sequence_no = 3;
  bytes pcm_data = 4;
  uint32 sample_rate = 5;
  bool is_final_chunk = 6;
}

enum FlushReason {
  FLUSH_REASON_UNSPECIFIED = 0;
  FLUSH_REASON_BARGE_IN = 1;
  FLUSH_REASON_CANCELLED = 2;
  FLUSH_REASON_CLIENT_MOVED = 3;
}
message FlushControl {
  string utterance_id = 1;
  uint64 flush_generation_id = 2;
  FlushReason reason = 3;
}

// クライアントの再生状態・進捗報告（クライアント -> ホスト）: 全音声の再生完了、または中断・失敗
message PlaybackStatusReport {
  string utterance_id = 1;
  uint64 generation_id = 2;
  enum Status {
    STATUS_UNSPECIFIED = 0;
    PLAYBACK_COMPLETED = 1;
    PLAYBACK_INTERRUPTED = 2;
    PLAYBACK_FAILED = 3;
  }
  Status status = 3;
  uint32 completed_sentence_count = 4;   // 中断までに実際に再生完了した文の総数（0は未完了）
  repeated uint32 played_sequence_nos = 5;  // 実際に再生された文番号一覧
  repeated uint32 skipped_sequence_nos = 6; // スキップ・失敗した文番号一覧
  uint64 completed_pcm_bytes = 7;        // 実際に再生されたPCMデータ長
  string client_id = 8;                  // 報告元クライアント識別
  uint64 activation_revision = 9;        // 報告対象の発話を所有した有効化revision
}

message ReportPlaybackStatusResponse {
  bool acknowledged = 1;
}
```

---

## 7. 検証条件と受け入れ基準 (Alpha Voice & Body Invariants)

| 不変条件 | 検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **能力宣言に基づくツール公開** | `body.motion`を宣言するクライアントと宣言しないクライアントで、親エージェントのツール一覧を比較 | 宣言・許可された能力のツールだけが公開され、宣言のないクライアントでは該当ツールが現れないこと |
| **未許可能力の遮断** | `eye.watch`を宣言したが許可していない状態で`set_eye_watch`を呼ぶ | ツールが公開されないか、`DeviceOperation`を送出せず明示的な許可エラーになること |
| **能力仕様の検証** | 不正なJSON Schema、重複する`tool_name`、ホスト組み込みツール名を持つ宣言を登録 | 登録が拒否され、既存のツールや能力が破壊されないこと |
| **デバイス操作の再送と冪等性** | 接続断後に未配送（pending）の`DeviceOperation`を再接続で受信し、同じ`operation_id`の結果を再送 | クライアントが操作を重複実行せず、完了済みの結果をそのまま返すこと |
| **端末能力の操作単位承認** | `approval = CONFIRM`の能力で`device_request`を実行 | 能力の許可だけで配送されず、`Stage::Gate`と`ApprovalRequest`を経ること |
| **端末移動後の旧音声・承認の失効** | 発話Aの再生中にコンパニオンを別クライアントへ移動し、旧クライアントから遅延して`ReportPlaybackStatus`・音声入力・承認を送信 | 旧`activation_revision`の入力がすべて破棄され、新クライアントの発話・フロア・承認に影響しないこと |
| **世代別Flush破棄** | 発話Aの再生中に割り込み（Flush）を送り、遅れて発話Aの音声チャンクを送信 | 遅れて届いた旧世代チャンクが再生されず、即座に破棄されること |
| **TTS文順序保証** | 第2文が第1文より先に合成完了するモックTTSを接続 | クライアント側で `sequence_no` 順に整列して待機し、文章の順序逆転が起きないこと |
| **クライアント単一再生保証** | 発話Aの再生中に発話Bの音声チャンクを受信 | クライアント側で発話Aの再生が即座に停止・フェードアウトされ、音声の重複が物理的に防止されること |
| **発話切替後の旧発話無害化** | 発話Aから発話Bへ切替後、発話AのチャンクやFlushが遅延到着 | 旧発話Aのデータが安全に破棄され、発話Bの再生が妨害されたり発話Aへ巻き戻ったりしないこと |
| **文合成失敗時の非ブロッキング** | 第1文の合成が失敗するモックTTSを接続 | ホストから `SentenceSkipped` が送信され、クライアントが待機を解除して第2文の再生へ進めること |
| **送信完了と再生完了の分離** | 再生中（残り7秒）に `UtteranceFinished` を受信するケースと、全文スキップで再生対象が0個のケースを検証 | 音声が残っている場合は再生完了まで `ReportPlaybackStatus(COMPLETED)` が送信されず読み上げが継続し、再生対象が0個の場合のみ即座に完了報告が送信されること |
| **自己音声エコー誤承認防止** | コンパニオン発話再生中および再生直後の音響ループバック環境で音声承認を試行 | 再生中に収録された音声フレームが破棄され、再生停止後に新規開始された発話にのみ承認が束縛されること（AEC無効環境では自発話による誤Barge-inが発生しないよう明示操作割り込みへ縮退すること） |
| **作業委託の原子的受付登録** | 2体のコンパニオンが同一指示に対して同時に `start_work`（委託）を試行 | ホスト側で `try_claim_delegation` により先着1件のみが受付登録され、他方は安全に作業を控え、外部副作用の多重実行が遮断されること |
| **掛け合い上限制限** | 2体のコンパニオンが互いに発話（テキスト生成）を返し続ける設定で起動 | ユーザー入力なしでの掛け合いが5往復以内に自動停止し、無限ループしないこと |
| **再生完了連携とタイムアウト** | 再生完了イベント送信時、および完了イベントを意図的に落とした時を検証 | 通常時は完了通知でフロアが解放され、障害時もPCM長+3秒で安全に自動解放されること |
| **遅延完了報告の無害化** | タイムアウト後に旧発話の `PLAYBACK_COMPLETED` を意図的に遅延送信 | 新発話の発話権や会話リレーに影響を与えず、旧報告が安全に破棄されること |
| **単独モーション直接指示** | `body.motion`を宣言したアクティブクライアントに対し、会話静止状態で親から `body_motion` のみを呼び出し | クライアントが指定されたモーション・表情へスムーズにクロスフェードし、音声パイプラインを止めないこと |
| **フレーム連動リップシンク** | 再生PCMバッファの振幅とクライアントのブレンドシェイプ変位をフレーム単位で測定 | ネットワーク往復なしにローカル再生音量に連動して口パクが変位すること |
