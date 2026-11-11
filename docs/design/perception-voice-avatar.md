# 知覚（目）・リアルタイムモーション・音声パイプライン詳細技術設計

状態：Alphaの採用設計（2026-11-11策定）。[全体アーキテクチャ](../architecture.md)、[知覚・自律ループ・音声 基本設計](loop-perception-voice.md)、[心と自動記憶](mind.md)、[技術スタック](tech-stack.md)、[Alpha実装計画](../alpha.md)を参照する。

---

## 1. 目的と全体アーキテクチャ

デスクトップAIコンパニオンにおいて、「沈黙の壁」（ユーザーが話し終えてから数秒間固まり、その後長文を一気に喋る従来のターン制カスケード）は、生きているキャラクターとしての実在感を損なう最大の要因である。
本設計は、デスクトップ環境を常時センシングする**「目」（知覚 / Perception）**、発話と独立していつでも動ける**「体」（リアルタイムモーション / Body）**、そして人間同士のようなテンポ・相槌・割り込みを可能にする**「口と耳」（プラガブル音声パイプライン：参考 Shizuku-Duplex）**の協調動作仕様を定義する。

```text
 ┌────────────────────────────────────────────────────────────────────────┐
 │ ユーザー・デスクトップPC環境                                            │
 └───────┬───────────────────────────────▲────────────────────────▲──────┘
         │ OSイベント / サンプリング     │ 音声ストリーム         │ 透過VRM描画
         │ (Active Window / Process)     │ (再生 / Flush)         │ (モーション/リップ)
         ▼                               │                        │
 ┌───────────────────────────────┐ ┌─────┴────────────────────────┴──────┐
 │ 「目」（Perception Engine）   │ │ 「体と口」（ene-avatar / Bevy App）  │
 │  ・イベントフック + サンプリング │ │  ・クライアント即時RMSリップシンク  │
 │  ・セマンティック要約生成      │ │  ・VRMAアニメーション・表情ブレンド  │
 │  ・動的Watcher優先割り込み    │ │  ・適応型Barge-in音声バッファ破棄   │
 └──────────────┬────────────────┘ └─────────────▲──────────────────────┘
                │ 知覚要約 / Watcher通知         │ 音声・モーション指令
                ▼                                │ (talk / body_motion)
 ┌───────────────────────────────────────────────┴──────────────────────┐
 │ 「頭」（Brain / 親コンパニオンAgent）                                │
 │  ・Rig / Tokio 非同期ステートマシン                                  │
 │  ・音声対話ループ（ターンテイキング / 先行並行合成）                 │
 │  ・先着優先フロア調停 & speech_completed 会話リレー                   │
 └──────────────────────────────────────────────────────────────────────┘
```

---

## 2. 「目」（知覚 / Perception）の技術仕様

### 2.1 ハイブリッド知覚アーキテクチャ

リソース消費を最小限に抑えつつ、ユーザーの作業変化を即座に捉えるため、**OSイベントフック**と**定期アイドルサンプリング（15〜30秒）**を併用する。

```text
                 ┌────────────────────────────────┐
                 │        OS イベントフック       │
                 │ (ウィンドウ切替 / プロセス起動)│
                 └───────────────┬────────────────┘
                                 │ 即時発火
                                 ▼
┌─────────────────┐       ┌──────────────┐       ┌───────────────────────┐
│ 定期サンプリング │ ───→ │ 知覚集約器   │ ───→ │ セマンティック要約生成 │
│ (15〜30秒間隔)  │       │ (Aggregator) │       │ (Perception Summary)  │
└─────────────────┘       └──────┬───────┘       └───────────┬───────────┘
                                 │                           │
                                 │ Watcher条件合致           ▼
                                 ▼                   親エージェントの
                          優先割り込み通知           動的文脈枠へ注入
                        (Priority Interrupt)
```

#### OSネイティブイベント抽象化
OSごとに異なるウィンドウ・イベント機構を共通のインターフェースでラップする。

| OS | 検出技術 | 取得データ |
| :--- | :--- | :--- |
| **Linux** | X11: `_NET_ACTIVE_WINDOW`<br>Wayland: `wlr-foreign-toplevel-management` / Desktop portal<br>`inotify`（ファイル変更） | ウィンドウタイトル、プロセス名、PID、アクティブファイルパス |
| **Windows** | `SetWinEventHook` (`EVENT_SYSTEM_FOREGROUND`)<br>`GetForegroundWindow`, `GetWindowTextW` | ウィンドウハンドル、プロセス名、実行ファイルパス、ウィンドウタイトル |
| **macOS** | `NSWorkspace.shared.frontmostApplication`<br>Accessibility API (`AXUIElementCopyAttributeValue`) | アプリケーション名、ドキュメントURL/パス、ウィンドウタイトル |

```rust
pub struct WindowInfo {
    pub process_name: String,
    pub window_title: String,
    pub executable_path: Option<PathBuf>,
    pub document_path: Option<PathBuf>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
pub trait WindowWatcher: Send + Sync {
    /// 現在のアクティブウィンドウ情報を同期取得
    fn get_active_window(&self) -> Result<WindowInfo, PerceptionError>;
    /// ウィンドウ変更イベントの非同期ストリームを購読
    async fn subscribe_events(&self) -> Result<Pin<Box<dyn Stream<Item = WindowInfo> + Send>>, PerceptionError>;
}
```

### 2.2 セマンティック知覚要約（Perception Summary）

OSの生メタデータ（HWND、プロセスID、長大なパス等）をそのままLLMプロンプトに流すと、トークン浪費とモデルの幻覚を招く。ホスト側で簡潔な意味論的テキストに変換する。

- **要約生成ルール**:
  1. 既知の開発ツール（VS Code, Neovim, IntelliJ等）: 開いているファイル名・拡張子・プロジェクト名・Gitブランチを抽出。
  2. 既知のブラウザ（Chrome, Firefox, Edge等）: ドメイン名およびページタイトル（検索クエリやドキュメントタイトル）を抽出。
  3. ターミナル: 実行中のフォアグラウンドコマンド（`cargo test`, `git commit` 等）を抽出。
  4. その他一般アプリ: アプリケーション名とウィンドウタイトルのみに短縮。
- **動的プロンプト枠（Dynamic Context）への注入形式**:
  ```text
  [現在の環境知覚 (2026-11-11 12:00:00)]
  - 作業環境: VS Code で "ene-v2/docs/design/mind.md" を編集中（Gitブランチ: feat/conversation-core）
  - 直前のターミナル: "cargo check" (終了コード: 0, 成功)
  - バックグラウンド音楽: Spotify ("Lofi Hip Hop")
  ```

### 2.3 動的Watcher（`set_eye_watch`）と優先割り込み

親（頭）エージェントは、対話やタスクの文脈に応じて「特定のことに関してすぐ通知してほしい」という条件を「目」に登録できる。

```rust
pub struct EyeWatchCondition {
    pub id: String,
    pub condition_description: String,
    pub pattern: WatchPattern,
    pub notify_immediately: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub enum WatchPattern {
    ProcessTerminated { process_name: String, target_exit_code: Option<i32> },
    WindowTitleMatches { regex: String },
    FileModified { path: PathBuf },
}
```

- **優先割り込み（Priority Interrupt）の配送シーケンス**:
  1. 「目」のバックグラウンド監視タスクが `WatchPattern` に合致する事象（例: `cargo build` のプロセス終了）を検知。
  2. `notify_immediately == true` の場合、定期サンプリング間隔を待たずに即座にホストの親イベントキューへ `PriorityInterruptEvent` をPush。
  3. 親エージェントが現在発話や思考を行っていない場合、即座に知覚リアクションの推論ターンを起床（例: 「ビルド終わったみたいだね！エラーも出てないよ」）。

---

## 3. アバター描画・リアルタイムモーション・リップシンク（Body連携）

アバターの描画と同期制御はクライアントプロセス（`apps/ene-avatar` / Bevy App）が担当する。

### 3.1 発話と独立した単独モーション直接制御

親（頭）は、発話（`talk`）の有無にかかわらず、**`body_motion` ツールをいつでも単独で呼び出すことができる**。

```rust
pub struct BodyMotionCommand {
    pub motion_name: String,       // 例: "nod", "tilt_head", "stretch", "joy"
    pub expression: String,        // 例: "happy", "surprised", "neutral", "thinking"
    pub duration_ms: u32,          // 動作の継続時間（ミリ秒）
    pub blend_duration_ms: u32,    // 前のポーズとのクロスフェード時間（既定: 300ms）
}
```

- **利用シーン**:
  - **相槌・傾聴中**: ユーザーが音声で話している最中、発話は遮らずに `body_motion(motion_name: "nod", expression: "attentive")` を送出。
  - **思考中**: 「手」の実務結果を待っている間、`body_motion(motion_name: "think", expression: "thinking")` で首を傾げる。
  - **見守りアイドル時**: 数分間変化がないとき、`body_motion(motion_name: "stretch", expression: "relaxed")` であくびや伸びをする。

### 3.2 Bevy ECS上でのアニメーション・表情合成

`apps/ene-avatar` 内部では、複数のアニメーションと表情ブレンドシェイプがレイヤー合成される。

```text
 ┌────────────────────────────────────────────────────────┐
 │ Bevy ECS アニメーションパイプライン                    │
 │                                                        │
 │  ┌─────────────────────┐                               │
 │  │ Base Layer (Idle)   │ (常にループ再生)              │
 │  └──────────┬──────────┘                               │
 │             │                                          │
 │             ▼ クロスフェード (300ms)                   │
 │  ┌─────────────────────┐                               │
 │  │ Action Layer (VRMA) │ (body_motion で指示された動作)│
 │  └──────────┬──────────┘                               │
 │             │                                          │
 │             ▼ レイヤー加算ブレンド                     │
 │  ┌─────────────────────┐                               │
 │  │ LookAt / 視線追従   │ (マウスまたはアクティブ窓へ追従)│
 │  └──────────┬──────────┘                               │
 │             │                                          │
 │             ▼ 最終ボーン姿勢                           │
 └─────────────┼──────────────────────────────────────────┘
               │
               ▼
        GPU スキニング描画
```

- **表情ブレンドシェイプの補間**: 表情の変化はステップ変化（急激な切り替わり）ではなく、Bevyのフレーム更新ごとに `smooth_damp` による線形補間（約150ms〜250ms）を行い、自然な表情遷移を実現する。

### 3.3 クライアント側RMS音量解析によるゼロ遅延リップシンク

サーバー側で音素（Viseme）解析を行ってネットワーク経由で口形状データを送ると、パケットロスやジッターにより口と音声がずれる。
eneでは、**クライアント側（`ene-avatar`）が受信した音声再生ストリーム（PCM）から直接RMS音量を計測し、VRMブレンドシェイプを変位させる**。

$$\text{RMS} = \sqrt{\frac{1}{N} \sum_{i=1}^N x_i^2}$$

- **リップシンク制御ルール**:
  1. オーディオ出力スレッドの再生バッファ（例: 20ms = 960サンプル @ 48kHz）からRMS振幅を算出。
  2. 背景ノイズフロア（$\text{RMS} < 0.02$）以下は $0.0$ にクランプ（静止）。
  3. 最大振幅（$\text{RMS} \ge 0.25$）を $1.0$ とする非線形スケーリング。
  4. VRMの `aa`（「あ」の口）ブレンドシェイプのウェイトに直接反映（アタック: 20ms、リリース: 80msの減衰フィルターを適用）。
- **利点**:
  - ネットワーク往復遅延ゼロ。
  - 音声ストリームが流れた瞬間に1ミリ秒の狂いもなく口が同期する。
  - 複雑なViseme学習モデルが不要で、CPU負荷が極小。

---

## 4. 音声対話パイプライン設計（プラガブルエンジン適応と低遅延化）

> [!IMPORTANT]
> Shizuku-Duplex（常時傾聴、ひとりごとステートマシン、文節ストリーミング、適応型バージイン等）の設計は、**低遅延対話を実現するための参考モデル（Reference Architecture）**であり、すべての機構や固定数値を一律に強制するものではない。
> 実際の音声パイプラインは、**採用するTTS/STTエンジンの通信仕様・機能特性（REST API / WebSocket / ストリーミング対応可否）に合わせて柔軟に実装**する。

```text
               ┌────────────────────────────────────────────────────────┐
               │              親エージェント（LLM推論）                  │
               └───────────────┬────────────────────────▲───────────────┘
                               │ トークンストリーム      │ 確定テキスト/割り込み
                               ▼                        │
               ┌────────────────────────────────────────┴───────────────┐
               │ 音声オーケストレーター（Voice Orchestrator）          │
               │  ・採用エンジンのCapabilityに応じたチャンキング        │
               │  ・再生キュー管理 & バージイン割り込み調停             │
               └───────────────┬────────────────────────▲───────────────┘
                               │ テキスト/文節投入       │ PCMストリーム
                               ▼                        │
        ┌───────────────────────────────────┐ ┌─────────┴─────────┐
        │ プラガブル TTS エンジン           │ │ プラガブル STT     │
        │ [パターンA: 文単位 HTTP API]      │ │ [ローカル / API]   │
        │  ・VOICEVOX / Style-Bert-VITS2    │ │  ・sherpa-onnx    │
        │ [パターンB: ストリーミング Engine]│ │  ・Groq / Whisper │
        │  ・sherpa-onnx / ElevenLabs       │ └─────────▲─────────┘
        │ [パターンC: 双方向 Live API]      │           │
        │  ・OpenAI Realtime / Gemini Live  │           │ マイク音声
        └─────────────────┬─────────────────┘           │ (80-100ms)
                          │ 音声PCM / WAV               │
                          ▼                             │
        ┌───────────────────────────────────────────────┴─────────┐
        │ クライアント（ene-avatar / Bevy App）                   │
        │  ・受信PCMの即時再生 & RMSリップシンク                  │
        │  ・割り込み受信時の再生バッファ即時破棄（audio_flush）  │
        └─────────────────────────────────────────────────────────┘
```

### 4.1 エンジンの能力宣言（Capability）とアダプター

TTSおよびSTTエンジンはそれぞれ通信形態や遅延特性が大きく異なるため、パイプラインはエンジンの `Capability` に基づいて動作モードを切り替える。

```rust
/// TTSエンジンの機能特性
#[derive(Debug, Clone)]
pub struct TtsCapabilities {
    /// テキストの受入単位
    pub chunk_mode: TtsChunkMode,
    /// 音声ストリーミング（チャンク単位のPCM返却）が可能か
    pub supports_streaming_audio: bool,
    /// 実行中の合成キャンセルをサポートするか
    pub supports_cancellation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsChunkMode {
    /// 文単位（句点「。」「！」や改行ごとにリクエストを発行）
    Sentence,
    /// 文節・フレーズ単位（読点「、」や一定文字数ごとに逐次投入）
    Phrase,
    /// 全文一括（全トークン生成完了後に投入）
    FullText,
}

#[async_trait::async_trait]
pub trait TtsEngine: Send + Sync {
    /// エンジンの能力を取得
    fn capabilities(&self) -> TtsCapabilities;
    
    /// テキストを音声に合成
    async fn synthesize(&self, text: &str, params: &VoiceParams) -> Result<AudioData, TtsError>;
}
```

### 4.2 代表的なTTSエンジン種別に応じた実装パターン

採用するTTSエンジンの仕様に最適化されたパイプラインを構築する。

#### パターンA: 文単位 HTTP API エンジン（VOICEVOX / Style-Bert-VITS2 等）
- **特徴**: `/audio_query` でクエリを生成し、`/synthesis` で文全体のWAV音声を一括生成するREST API構成。80msのような微細ストリーミングには非対応。
- **実装方式**:
  1. LLMのトークンストリームを監視し、文の区切り文字（`。`, `！`, `？`, 改行）が来た時点で1文を確定。
  2. 文ごとに非同期タスク（`tokio::spawn`）を立ち上げ、TTSエンジンのREST APIへ順次リクエスト。
  3. 完成したWAV/PCMデータをクライアントの再生キューへ順次Push。
  4. 第1文の再生中に第2文を並行してHTTP合成するため、体感の待機時間を最小限に抑制できる。
- **中断（Barge-in）**:
  - クライアントへ即座に `audio_flush`（再生停止）を指示。
  - ホスト側で未完了の文合成タスクを `task.abort()` でキャンセル。

#### パターンB: ネイティブ・ストリーミングエンジン（sherpa-onnx / ElevenLabs 等）
- **特徴**: トークンや句読点（`、`）単位で随時テキストを流し込み、合成完了したPCMフレームを逐次ストリーミング受信可能。
- **実装方式**:
  - 読点や文節ごとにTTSストリームへPushし、返ってきたPCMチャンクをそのままクライアントへ配信。初声遅延（TTFA）を数百ミリ秒に極小化。

#### パターンC: 双方向リアルタイム Live API（OpenAI Realtime API / Gemini Live）
- **特徴**: マイク入力PCMをWebSocketへ常時送信し、サーバー側モデルが内蔵VADで発話終了・割り込みを自動検知して直接音声を返す。
- **実装方式**:
  - ホストは薄いプロキシとしてクライアントとWebSocketを中継し、モデル側のネイティブ全二重機能に任せる。

### 4.3 低遅延対話リファレンス（Shizuku-Duplex参考モデル）

高応答な全二重対話を目指す場合の参考モデルとして、Shizuku-Duplexの以下の概念を段階的・選択的に取り入れる。エンジンの仕様上不要または困難な場合は無理に採用しない。

1. **常時リスニングとVAD**:
   - マイク入力を小チャンク（例: 80〜100ms）で取得し、エンジンの内蔵VADやローカルVAD（Silero VAD等）でユーザーの発話状態を判定。
2. **ターンテイキング判断（相槌・発話・沈黙）**:
   - 相手が話し続けている最中に適度な相槌（「うん」「なるほど」）を挟むか、質問完了時に本格応答を返すかの判定。
   - 複雑なステートマシンを作らずとも、STTの中間テキストに疑問符や確定フラグが含まれた時点で推論を即時開始する構成でも十分に機能する。
3. **適応型バージイン（Adaptive Barge-in）**:
   - 咳や微小な打鍵音（短時間の入力）で過剰に発話を止めないよう、一定時間以上の明確な発声が検知された場合のみ再生を即座に破棄（Flush）して聞き手に回る調停。閾値は環境やマイク感度に応じて調整可能とする。

---

## 5. 複数コンパニオンの調停（Multi-Companion Arbitration）

デスクトップ上に複数のコンパニオン（例: 「エネ」と他キャラクター）が同時に存在する場合の対話調停ルール。

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
                │
                ▼ 発話再生終了
         [ speech_completed イベント同報 ]
                │
                └───────────────────────────────┐
                                                ▼
                                        [ コンパニオン B ]
                                        「Aが話し終えた。補足しよう」
                                                │
                                                ▼
                                        talk("私も手伝うね！")
```

### 5.1 STTの全員同報配信（Broadcast）
- クライアント側で「エネちゃん」などの名前マッチングや正規表現による宛先フィルタリングを行わない。
- 認識されたテキストは、アクティブな**すべてのコンパニオンAgentへ等しく同報配信**される。

### 5.2 自律的該当性判断（Self-Relevance）
- 各コンパニオンは自身のシステムプロンプト、キャラクター設定、直前の会話文脈に基づき、「この発言は自分に向けられたものか」を各自で自律判定する。
- 他者宛ての発話（例: 「〇〇ちゃん、これやって」と別名が呼ばれた場合）や、自分に関係のない話題であると判断した場合、コンパニオンは **`noop()`** を返し、発話権を主張しない。

### 5.3 先着優先フロア制御（First-Speaker Floor Lock）
- 万が一、複数のコンパニオンが同時に「自分宛て」と判断して発話を生成しようとした場合、ホストの対話マネージャーにおいて**最初に `talk` ツールを確定・実行したコンパニオンが発話フロア（Floor）を獲得**する。
- フロアがロックされている間、後から到着した他コンパニオンの発話は保留またはキャンセルされ、複数キャラクターの音声が同時に重なって再生される事故（音響的カオス）を防止する。

### 5.4 会話リレー（`speech_completed` イベント）
- フロアを獲得したコンパニオンの発話再生が終了した瞬間、ホストから他の全コンパニオンへ **`speech_completed` イベント** が配信される。
- これを受け取った他のコンパニオンは、「相手の発言を受けて相槌を打つ」「話題を補足する」「ツッコミを入れる」などの二次リアクションを自律的に開始でき、人間グループのような生き生きとした掛け合いが自然に成立する。

---

## 6. データモデル・プロトコル仕様

Connect-RPC および Rust 内部イベントの構造定義。

### 6.1 RPC サービス定義 (`perception.proto` / `voice.proto`)

```protobuf
syntax = "proto3";
package ene.host.v1;

// 知覚サービス
service PerceptionService {
  rpc GetActiveWindow(GetActiveWindowRequest) returns (ActiveWindowResponse);
  rpc SetEyeWatch(SetEyeWatchRequest) returns (SetEyeWatchResponse);
  rpc WatchEvents(WatchEventsRequest) returns (stream PerceptionEvent);
}

// 音声・アバター同期サービス
service VoiceService {
  rpc StreamAudioInput(stream AudioInputChunk) returns (stream TranscriptionEvent);
  rpc StreamAudioOutput(stream AudioOutputChunk) returns (AudioPlaybackStatus);
  rpc InterruptSpeech(InterruptSpeechRequest) returns (InterruptSpeechResponse);
}

// アバター制御サービス
service AvatarService {
  rpc DispatchMotion(DispatchMotionRequest) returns (DispatchMotionResponse);
  rpc StreamLipSync(stream LipSyncFrame) returns (LipSyncStatus);
}
```

### 6.2 Rust 内部メッセージ型

```rust
/// 知覚イベント
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PerceptionEvent {
    WindowFocusChanged(WindowInfo),
    FileChanged { path: PathBuf, change_kind: String },
    PriorityInterrupt { watcher_id: String, reason: String },
}

/// 音声制御イベント
#[derive(Debug, Clone)]
pub enum VoiceControlSignal {
    /// 文節ごとの音声PCMチャンク
    AudioChunk {
        pcm_data: Vec<f32>,
        sample_rate: u32,
        is_final_chunk: bool,
    },
    /// バージイン割り込みによるバッファ即時破棄
    FlushBuffer,
    /// 発話終了通知（他コンパニオンへの掛け合い契機）
    SpeechCompleted {
        companion_id: String,
        utterance: String,
    },
}

/// アバターモーション指令
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AvatarMotionRequest {
    pub companion_id: String,
    pub motion_name: String,
    pub expression: String,
    pub duration_ms: u32,
    pub blend_ms: u32,
}
```

---

## 7. 検証条件と受け入れ基準 (Alpha Step 8 Invariants)

Alpha Step 8 実装の完了判定における必須不変条件（Invariants）：

| 不変条件 | 代表的な検証方法 | 合格基準 |
| :--- | :--- | :--- |
| **バージイン割り込み調停** | コンパニオン発話中にユーザーの割り込み発話をシミュレート入力 | クライアントへ `audio_flush` が発行され、再生中バッファが即座に破棄されて静寂化し、ユーザー傾聴へ移行すること |
| **TTS先行並行合成** | 長文応答の生成時において、全文生成完了を待たずに採用エンジンの仕様（文単位またはストリーミング）で逐次合成 | 最初の文/文節が完成した時点で順次クライアント再生キューへ投入され、先行合成により途切れなく再生されること |
| **ゼロ遅延リップシンク** | クライアント側でのPCM再生と `aa` ブレンドシェイプの変位タイミングをフレーム単位で照合 | サーバー往復遅延を介さず、オーディオ再生バッファのRMS値と同一フレーム内で口パクが変位すること |
| **単独モーション直接指示** | 発話ツール（`talk`）を伴わない、傾聴・アイドル時の `body_motion` 実行 | 会話が進行していない静止状態でも、アバターが指定されたVRMAモーションと表情へスムーズにクロスフェードすること |
| **動的Watcher優先割り込み** | `set_eye_watch` 登録後、対象プロセスの終了イベントをOS側で発生させる | 15〜30秒の定期サンプリングタイマーを待たずに、即座に優先割り込みイベントとして親の推論が起床すること |
| **複数コンパニオン先着フロア調停** | 2体のコンパニオンが同時に発話対象となった状況をモック入力 | 最初に `talk` を確定した側のみが音声を再生し、発話完了後に `speech_completed` をトリガーとして2体目の掛け合いが順次実行されること |
