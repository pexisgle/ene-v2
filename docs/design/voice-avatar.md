# 知覚（目）・アバターモーション・音声パイプライン詳細設計

状態：Alphaの採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[技術スタック](tech-stack.md)、[作業相手API](work.md)、[音声機能仕様](../features/voice/README.md)を参照する。

---

## 1. 目的と全体アーキテクチャ

デスクトップ常駐AIコンパニオンにおいて、「沈黙の壁」（話し終えてから数秒間固まり、長文を一気に喋る従来のターン制カスケード）は実在感を大きく損なう。
本設計は、環境を常時センシングする**「目」（知覚 / Perception）**、発話と独立していつでも動ける**「体」（リアルタイムモーション / Body）**、そして自然なテンポ・先行合成・割り込み停止を可能にする**「口と耳」（プラガブル音声パイプライン：参考 Shizuku-Duplex）**の協調仕様を定義する。

```text
 ┌────────────────────────────────────────────────────────────────────────┐
 │ ユーザー・デスクトップPC環境                                            │
 └───────┬───────────────────────────────▲────────────────────────▲──────┘
         │ OSイベント / サンプリング     │ 音声ストリーム (Server │ 透過VRM描画
         │ (Active Window / Process)     │  Streaming) / Flush    │ (モーション/リップ)
         ▼                               │                        │
 ┌───────────────────────────────┐ ┌─────┴────────────────────────┴──────┐
 │ 「目」（Perception Engine）   │ │ 「体と口」（ene-avatar / Bevy App）  │
 │  ・イベントフック + サンプリング │ │  ・クライアント即時RMSリップシンク  │
 │  ・セマンティック要約生成      │ │  ・VRMAアニメーション・表情ブレンド  │
 │  ・動的Watcher優先割り込み    │ │  ・世代別音声バッファ破棄 (Flush)   │
 └──────────────┬────────────────┘ └─────────────▲──────────────────────┘
                │ 知覚要約 / Watcher通知         │ 音声・モーション指令
                ▼                                │ (talk / body_motion)
 ┌───────────────────────────────────────────────┴──────────────────────┐
 │ 「頭」（Brain / 親コンパニオンAgent）                                │
 │  ・マルチプロバイダーLLM ＋ Tokio非同期ステートマシン                │
 │  ・音声対話オーケストレーター（先行並行合成 & 再生順序制御）         │
 │  ・先着優先フロア制御 ＆ 作業二重実行防止 ＆ 会話リレー（上限5往復）  │
 └──────────────────────────────────────────────────────────────────────┘
```

---

## 2. 「目」（知覚 / Perception）の技術仕様

### 2.1 ハイブリッド知覚アーキテクチャ
リソース消費を最小限に抑えつつ作業変化を捉えるため、OSイベントフックと定期アイドルサンプリング（15〜30秒）を併用する。

| OS | 検出技術 | 取得できる客観的事実 |
| :--- | :--- | :--- |
| **Linux** | X11: `_NET_ACTIVE_WINDOW`<br>Wayland: `wlr-foreign-toplevel-management`<br>`inotify`（ファイル変更） | ウィンドウタイトル、プロセス名、PID、アクティブファイルパス |
| **Windows** | `SetWinEventHook` (`EVENT_SYSTEM_FOREGROUND`)<br>`GetForegroundWindow`, `GetWindowTextW` | ウィンドウハンドル、プロセス名、実行ファイルパス、ウィンドウタイトル |
| **macOS** | `NSWorkspace.shared.frontmostApplication`<br>Accessibility API (`AXUIElementCopyAttributeValue`) | アプリケーション名、ドキュメントURL/パス、ウィンドウタイトル |

### 2.2 セマンティック知覚要約と推測の分離
- **客観的事実と解釈の区分**:
  - ウィンドウタイトルやプロセス名は「観測された事実」として扱う。
  - 「VS Codeで mind.md を編集中」などの文脈推測は、ホスト側で簡潔な要約（Perception Summary）として整形し、取得できなかった項目は不明とする。
- **配置位置**: プロンプトのPrefix Cacheを保護するため、最新ターンの直前（動的文脈枠）へ注入する。

### 2.3 動的Watcher（`set_eye_watch`）と優先割り込み
親（頭）は対話やタスクの文脈に応じて、特定の監視条件を「目」に登録できる。
- 条件合致時（例: 特定PIDのビルド終了）、定期サンプリングを待たずに**即時優先割り込みイベント（Priority Interrupt）**が親へ届き、リアクション推論を起床させる。

---

## 3. アバター描画・リアルタイムモーション・リップシンク（Body連携）

アバターの描画と同期制御はクライアントプロセス（`apps/ene-avatar` / Bevy App）が担当する。

### 3.1 発話と独立した単独モーション直接制御
親（頭）は発話（`talk`）の有無にかかわらず、**`body_motion` ツールをいつでも単独で呼び出すことができる**。
- 傾聴中の頷き（`nod`）、作業画面注視時の首傾げ（`tilt_head`）、アイドル時の伸び（`stretch`）などを自律実行。

### 3.2 Bevy ECS上でのアニメーション・表情合成
- **アニメーションレイヤー**: 常にループ再生される `BaseIdleLayer` に対し、指示されたVRMAアクションを300msクロスフェードで加算ブレンド。
- **表情ブレンドシェイプ**: 急激なステップ変化を避け、`smooth_damp` による線形補間（約150〜250ms）で滑らかに遷移。
- **視線追従（LookAt）**: マウスポインタやフォーカスウィンドウへ自然に頭部・視線を追従。

### 3.3 クライアント側RMS音量解析によるリップシンク
- サーバー側での音素（Viseme）解析やネットワーク同期を排し、**クライアントが受信した音声PCMデータのRMS（実効値）振幅をローカルで直接計測**してブレンドシェイプ（`aa` 等）を変位させる。
- サーバー往復遅延がなく、音声再生と同一フレームで口パクが連動する。

---

## 4. 音声対話パイプライン設計（プラガブルエンジン適応）

> [!NOTE]
> Shizuku-Duplex（常時傾聴、ひとりごとステートマシン、文節ストリーミング、適応型バージイン等）は**低遅延対話の参考モデル（リファレンス）**である。
> 実際のパイプラインは、採用するTTSエンジンの通信仕様（文単位REST API / ストリーミング / 双方向Live API）に合わせて柔軟に実装する。

### 4.1 TTSエンジンの特性に応じた実装パターン

```rust
pub struct TtsCapabilities {
    pub chunk_mode: TtsChunkMode, // Sentence(文単位), Phrase(文節), FullText(全文)
    pub supports_streaming_audio: bool,
    pub supports_cancellation: bool,
}
```

1. **パターンA: 文単位 HTTP API エンジン（VOICEVOX / Style-Bert-VITS2 等）**
   - LLM出力を文区切り（`。`, `！`, `？`, 改行）で分割し、文ごとに非同期でHTTPリクエスト。
   - 完成したWAV/PCMをクライアント再生キューへ順次投入。第1文の再生中に第2文を並行合成することで体感待ち時間を極小化。
2. **パターンB: ネイティブ・ストリーミングエンジン（sherpa-onnx / ElevenLabs 等）**
   - 読点や文節ごとに逐次ストリーム投入し、合成完了したPCMチャンクを順次配信。
3. **パターンC: 双方向リアルタイム Live API（OpenAI Realtime / Gemini Live）**
   - WebSocketでモデルとクライアント音声を直結し、モデル側のネイティブVAD・割り込み機構に任せる。

### 4.2 世代管理（Generation ID）と再生順序制御

レビューで指摘された「Flush後の遅延到着再生」および「文完成順の逆転」を防ぐため、以下の順序・世代管理を厳格に実装する。

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioChunkEnvelope {
    pub utterance_id: String,
    pub generation_id: u64,
    pub sequence_no: u32,
    pub pcm_data: Vec<f32>,
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
       talk("了解だよ！")                     noop()
                │
                ▼ 最初にtalk確定した方がフロア獲得
         [ 音声再生開始 ]
         【作業排他ロック】Aのみが instruct_hand 実行可能
                │
                ▼ 発話再生終了
         [ speech_completed イベント同報 ]
                │
                └───────────────────────────────┐
                                                ▼
                                        [ コンパニオン B ]
                                        「Aが話し終えた。補足しよう」
                                                │ (上限5往復まで)
                                                ▼
                                        talk("私も手伝うね！")
```

### 5.1 STT同報配信と自律的該当性判断
- 音声認識テキストは、アクティブな全コンパニオンへ等しく同報配信される。
- 各コンパニオンは自身のシステムプロンプトと文脈から「自分宛てか」を自律判断し、無関係なら `noop()` を返して静観する。

### 5.2 発話フロア制御と「作業二重実行」の排他ロック
- 最初に `talk` ツールを確定させたコンパニオンが発話権（Floor）を獲得する。
- **実務タスク（`instruct_hand`）の排他制御**:
  - 同一のユーザー指示に対し、**フロアを獲得した主担当の1体のみが実務タスク（`instruct_hand`）を開始できる排他ロック**をかける。
  - これにより、2体が同時に重い作業を開始してしまう二重実行事故を防止する。

### 5.3 会話リレー（`speech_completed`）と上限管理
- 主担当コンパニオンの発話完了時、他の全コンパニオンへ `speech_completed` イベントが配信され、自然な掛け合いや相槌を誘発する。
- **掛け合い上限（最大5往復）**:
  - コンパニオン同士の掛け合いが無限ループするのを防ぐため、ユーザー入力なしでの自律掛け合いリレーは**最大5ターン（往復）**で自動打ち切りとする。

---

## 6. Connect-RPC サービス定義 (`voice.proto`)

```protobuf
syntax = "proto3";
package ene.host.v1;

service VoiceService {
  // マイク音声ストリーム入力
  rpc StreamAudioInput(stream AudioInputChunk) returns (stream TranscriptionEvent);
  // 音声ストリーム購読（ホストからクライアントへのServer Streaming）
  rpc SubscribeAudioOutput(SubscribeAudioRequest) returns (stream AudioChunkEnvelope);
  // バージイン等による音声破棄要求
  rpc FlushAudio(FlushAudioRequest) returns (FlushAudioResponse);
}

message AudioChunkEnvelope {
  string utterance_id = 1;
  uint64 generation_id = 2;
  uint32 sequence_no = 3;
  bytes pcm_data = 4;
  uint32 sample_rate = 5;
  bool is_final_chunk = 6;
}

message FlushAudioRequest {
  string utterance_id = 1;
  uint64 flush_generation_id = 2;
}

message FlushAudioResponse {
  bool success = 1;
}
```

---

## 7. 検証条件と受け入れ基準 (Alpha Voice & Body Invariants)

| 不変条件 | 検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **世代別Flush破棄** | 発話Aの再生中に割り込み（Flush）を送り、遅れて発話Aの音声チャンクを送信 | 遅れて届いた旧世代チャンクが再生されず、即座に破棄されること |
| **TTS文順序保証** | 第2文が第1文より先に合成完了するモックTTSを接続 | クライアント側で `sequence_no` 順に再生され、文章の順序逆転が起きないこと |
| **作業二重実行防止** | 2体のコンパニオンが同時に同一指示を自分宛てと判断する入力を投入 | 音声フロアを獲得した主担当のみが `instruct_hand` を実行し、二重タスクが防止されること |
| **掛け合い上限制限** | 2体のコンパニオンが互いに `talk` を返し続ける設定で起動 | ユーザー入力なしでの掛け合いが5往復以内に自動停止し、無限ループしないこと |
| **単独モーション直接指示** | 会話静止状態で親から `body_motion` のみを呼び出し | アバターが指定されたVRMAモーション・表情へスムーズにクロスフェードすること |
| **フレーム連動リップシンク** | 再生PCMバッファの振幅とVRMブレンドシェイプ変位をフレーム単位で測定 | ネットワーク往復なしにローカル再生音量に連動して口パクが変位すること |
