# デスクトップクライアント（リファレンス実装）設計

状態：Alphaの採用設計（2026-11-11策定）。ホストとの共通契約は[知覚・身体・音声パイプライン共通仕様](voice-avatar.md)、採用ライブラリは[技術スタック](tech-stack.md)を参照する。

この文書は、デスクトップクライアントと`apps/ene-avatar`の実装固有仕様を定義する。ホストはここに書かれたプロセス構成、OSフック、描画ライブラリ、音声デバイスに依存しない。ホストとの接続は、クライアントが宣言する`ClientCapabilitySpec`と`DeviceOperation`の共通契約だけを使う。

---

## 1. プロセス境界とIPCトポロジー

```text
[ene-host] ◀── Connect-RPC ──▶ [デスクトップクライアント]
                                      │ stdin/stdout JSON Lines
                                      ▼
                               [apps/ene-avatar]
```

- **`apps/ene-host`（ホストデーモン）**:
  - バックエンドとして Connect-RPC サーバーを提供する。
  - 会話頭脳（`rig-ecs`）、記憶（SQLite）、作業セッション（WorkSession）、TTS合成オーケストレーションを担当する。
  - デスクトップ固有の描画・OSフック・音声デバイスを持たない。
- **デスクトップクライアント（Desktop Client）**:
  - Connect-RPC クライアントとして`ene-host`と通信する。
  - `RegisterClient`で`body.motion`、`eye.watch`、`body.window_perch`等の能力と入力スキーマを宣言する。
  - ホストから受け取った`Activation.revision`を保持し、音声入力・再生完了報告・知覚イベント・操作結果に付与する。再接続時は`GetSnapshot.activations`または`activation_updated`で現在のrevisionへ更新する。
  - ローカルサウンドデバイス（`cpal` + `rubato`）によるマイク録音・スピーカー再生を担当する。
  - 再生PCMデータからリアルタイムにRMS（実効値振幅）を算出し、子プロセスへ伝達する。
- **`apps/ene-avatar`（アバター描画プロセス）**:
  - デスクトップクライアントから子プロセスとして起動される。
  - OS透過・最前面・クリックスルー対応の Bevy ECS アプリケーション。
  - クライアントとは **標準入出力（stdin/stdout JSON Lines IPC）** で疎結合に通信する（`src/control.rs`参照）。モーション再生、表情指定、およびRMS音量レベルを即時に反映する。
  - ホストから直接接続されることはなく、ホストは`ene-avatar`の存在を知らない。

---

## 2. 知覚センサー（目）の実装

### 2.1 OS別の検出技術

| OS | 検出技術 | 取得できる客観的事実 |
| :--- | :--- | :--- |
| **Linux** | X11: `_NET_ACTIVE_WINDOW`<br>Wayland: `wlr-foreign-toplevel-management`（wlroots系: Hyprland/Sway等） | ウィンドウタイトル、`app_id`、アクティブ状態（GNOME/KDE等未対応環境では取得不可として安全にフォールバック） |
| **Windows** | `SetWinEventHook` (`EVENT_SYSTEM_FOREGROUND`)<br>`GetForegroundWindow`, `GetWindowTextW` | ウィンドウハンドル、プロセス名、ウィンドウタイトル |
| **macOS** | `NSWorkspace.shared.frontmostApplication`<br>Accessibility API (`AXUIElementCopyAttributeValue`) | アプリケーション名、ウィンドウタイトル |

### 2.2 知覚イベントの送信と`eye.watch`の適用

デスクトップクライアントは`ClientService.ReportPerception`でホストへ知覚イベントを送る。

- **アクティブウィンドウの変化**: OSフックで前面ウィンドウの変更を検出し、`PerceptionEvent { active_window: ActiveWindowChanged { title, app_id } }`を送信する。
- **アイドル状態**: 無操作時は15〜30秒間隔でサンプリングし、`PerceptionEvent { user_activity: UserActivityState { is_active, idle_seconds } }`を送信する。
- **要約の生成**: クライアント側で要約を作れる場合は`PerceptionSummary`を送信してよい。ホスト側でも受信内容から簡潔な要約を生成する。
- **`eye.watch`の受信**: ホストから`DeviceOperation { capability: "eye.watch", input: <ルールJSON> }`が届いたら、OSフックのフィルタ条件を動的に更新する。
  - `target_pattern`: 監視対象のウィンドウタイトルまたはアプリ識別子の部分一致・正規表現パターン（例: `"GitHub"`, `".*\\.rs"`）
  - `match_kind`: `"title"`（ウィンドウタイトル）または `"app_id"`（プロセス/アプリケーション名）
  - `notify_on`: 発火契機（`"focus"`: アクティブ化時、`"blur"`: 非アクティブ化時、`"title_change"`: タイトル変更時）
  - `enabled`: ルールの有効化（`true`）または解除（`false`）
- **優先割り込み**: ルールに合致したら、定期サンプリングを待たずに`ReportPerception`で優先割り込みイベントを返す。ホストはこれを親エージェントのリアクション推論へ渡す。
- **非対応環境**: WaylandのGNOME/KDE等、フックAPIが利用できない環境では、ウィンドウ情報を取得できないことを明示して安全に縮退する。取得できない項目を推測で補わない。

### 2.3 専用連携プラグイン

OSウィンドウ監視で取得できるのは表層情報に限られるため、エディタやブラウザの専用連携はクライアント側プラグインとして実装する。

- VS Code拡張やブラウザ拡張から、開いているファイル、プロジェクト、URL等を取得する。
- 取得した情報は`PerceptionSummary`または専用の知覚イベントとしてホストへ送る。
- 専用連携の導入・権限はクライアント側で管理し、ホストは受信した事実だけを利用する。

---

## 3. 身体表現（Body）の実装

### 3.1 Bevy ECS標準機能によるアニメーション制御

独自のアニメーション再生レイヤーを新設せず、Bevy標準のコンポーネントを活用する。

- **`AnimationGraph` ＋ `AnimationTransitions`**: 待機アニメーション（Idle）と各種アクションモーションをBevy標準の`AnimationTransitions`でスムーズにクロスフェード（約300ms）遷移させる。
- **`ene-vrm`の責務**: VRM/VRMAの読み込みとボーン・モーフターゲットへの対応付けに専念し、再生時間やループ、ブレンディングはBevy標準の`AnimationPlayer`に委ねる。
- **表情ブレンドシェイプ**: 急激なステップ変化を避け、`smooth_damp`による線形補間（約150〜250ms）で滑らかに遷移。
- **視線追従（LookAt）**: マウスポインタやフォーカスウィンドウへ自然に頭部・視線を追従。

### 3.2 クライアント側RMS音量解析によるリップシンク

- サーバー側での音素（Viseme）解析やネットワーク同期を排し、**クライアントが受信した音声PCMデータのRMS（実効値）振幅をローカルで直接計測**してブレンドシェイプ（`aa`等）を変位させる。
- サーバー往復遅延がなく、音声再生と同一フレームで口パクが連動する。
- 計測したRMSは`ene-avatar`へ高頻度のワンウェイ通知として送る（§3.3）。

### 3.3 `apps/ene-avatar`標準入出力 JSON Lines IPCスキーマ

デスクトップクライアントと`apps/ene-avatar`子プロセス間の標準入出力通信仕様。各行は最大64 KiBの改行区切りJSONテキスト。

- **実装済み機能と将来拡張の境界**:
  - **実装済み機能**: `src/control.rs`の`add`, `set`, `get`, `remove`, `center`, `fps`, `quit`コマンド。VRM 1.0モデルロード、単一VRMA再生、SpringBone物理演算。
  - **段階的拡張**: 表情ブレンドシェイプ（`expression`）、アニメーション遷移クロスフェード、および高頻度ワンウェイ口パク通知（`lip_sync`）。
- **リクエスト形式（Client ➔ Avatar stdin）**:
  ```json
  {"id": 1, "op": "add", "character": "ene", "config": {"model": "/abs/path/ene.vrm", "motion": "/abs/path/idle.vrma", "playing": true, "looping": true, "size_percent": 100.0}}
  {"id": 2, "op": "set", "character": "ene", "config": {"model": "/abs/path/ene.vrm", "motion": "/abs/path/nod.vrma", "playing": true, "looping": false, "size_percent": 100.0}}
  {"id": 3, "op": "center", "character": "ene", "monitor": "eDP-1"}
  {"id": 4, "op": "fps", "value": 60}
  {"id": 5, "op": "remove", "character": "ene"}
  {"id": 6, "op": "quit"}
  ```
- **高頻度ストリーム（ワンウェイ通知型: 応答待機なし）**:
  リップシンクは毎秒60回発生する高頻度ストリームであるため、ID付きリクエスト・レスポンスではなく、**標準出力の詰まりを防止するワンウェイ通知（Fire-and-forget）**として送信する：
  ```json
  {"op": "lip_sync", "character": "ene", "rms": 0.42}
  ```
- **レスポンス・通知形式（Avatar stdout ➔ Client）**:
  ```json
  {"id": 1, "ok": null}
  {"id": 2, "ok": null}
  {"event": "finished", "character": "ene"}
  ```

### 3.4 デスクトップギミック

画面端への座り込み、アクティブウィンドウの上に乗る、ウィンドウ移動への追従、落下・跳ねなどの演出は、クライアント側のギミックプラグインとして実装する。

- クライアントは`body.window_perch`等の能力を`ClientCapabilitySpec`で宣言する。ツール名と入力スキーマもクライアントが決める。
  - 例: `tool_name = "window_perch"`、入力`{window: String, edge: "top" | "bottom" | "left" | "right", enabled: bool}`
- ホストは`window_perch`を他の能力ツールと同じ`DeviceOperation`経路で呼ぶだけで、ウィンドウAPIや描画の詳細を知らない。
- ウィンドウの位置・サイズ・フォーカスはクライアントがOS APIから取得する。取得できない環境では、ギミックを無効化するか画面端固定へ縮退する。

---

## 4. 音声入出力の実装

### 4.1 デバイス所有と単一再生

- デスクトップクライアントは`cpal`でマイク入力とスピーカー出力を開き、音声デバイスを排他所有する。
- 同時に再生する音声ストリームは常に1件とする。新しい発話のチャンクまたは`FlushControl`を受信したら、旧音声を即座に停止・フェードアウトする。
- 再生完了・中断・失敗は`ReportPlaybackStatus`でホストへ報告する。新旧の発話が混在しないよう、`utterance_id`と`generation_id`で再生状態を管理する。音声入力・再生完了報告・知覚イベントには、保持している`activation_revision`を付与する。
- ホストから`FlushControl { reason: FLUSH_REASON_CLIENT_MOVED }`を受信したら、旧発話の再生を即座に停止し、以降の旧発話チャンクと再生完了報告を破棄する。別端末へ移動した後は、旧端末の状態を新しい端末へ引き継がない。

### 4.2 `cpal` / `rubato`によるリサンプリング

- `cpal`はハードウェアネイティブのサンプリングレート（多くのデバイスで48kHzまたは44.1kHz、2chステレオ）しか受け付けない場合が多い。
- クライアント側で公式の高品質リサンプラー`rubato`を用いて、マイク入力（48kHz stereo ➔ 24kHz mono）およびスピーカー再生（24kHz mono ➔ 48kHz stereo）の双方向リサンプリング・チャンネル変換を行う。
- ワイヤー標準は24kHz / 16-bit mono PCMとし、デバイス固有の形式へ変換するのはクライアントの責務とする。

### 4.3 エコー・Barge-inの実装

- 参照信号AECを利用できる環境では、再生PCMを参照信号としてエコーを減算し、残差からユーザーの割り込みを検出する。
- AECを利用できない、または検証できないスピーカー環境では、マイク入力による自動Barge-inを無効化し、GUIの停止ボタン、Escキー、プッシュ・トゥ・トーク等の明示操作に限定する。
- `AudioInputChunk.is_companion_speaking`は再生状態のソフトウェアフラグであり、音響的静寂を保証しない。再生中に収録した音声フレームは音声承認の判定対象から破棄する。

---

## 5. クライアントが宣言する能力の例

デスクトップクライアントが`RegisterClient` / `SetCapabilities`で宣言する`ClientCapabilitySpec`の例を示す。ホストはこれらを組み込みで持たず、宣言と許可に応じてツールを登録する。

| capability | tool_name | input_schemaの概要 | presetsの例 | approval |
| :--- | :--- | :--- | :--- | :--- |
| `body.motion` | `body_motion` | `{motion: string, expression?: string}` | `idle`, `nod`, `tilt_head`, `stretch` | `AUTO` |
| `eye.watch` | `set_eye_watch` | `{target_pattern: string, match_kind: "title" \| "app_id", notify_on: "focus" \| "blur" \| "title_change", enabled: boolean}` | - | `AUTO` |
| `body.window_perch` | `window_perch` | `{window: string, edge: "top" \| "bottom" \| "left" \| "right", enabled: boolean}` | `screen_edge`, `active_window` | `AUTO` |
| `device.files` | （なし） | ファイル読み書きの入力スキーマ | - | `CONFIRM` |
| `device.screen` | （なし） | 画面キャプチャの入力スキーマ | - | `CONFIRM` |

- `tool_name`を持つ能力は、ホストが親エージェント向けのツールとして動的に登録する。
- `tool_name`を持たない能力は、汎用の`device_request(capability, input)`からのみ呼ばれる。
- `presets`はモデル向けのツール説明に反映され、利用可能なモーション・表情の選択肢をモデルが把握できるようにする。
- `approval`は能力の既定の承認要否であり、`CONFIRM`は操作ごとに`Stage::Gate`と`ApprovalRequest`を必須にする。`AUTO`でもハード禁止や明示的毎回確認ポリシーは迂回できない。
- `approval_target`には承認カードの対象説明テンプレート（例: `"{path}"`, `"画面キャプチャ"`）を宣言する。ホストは検証済み入力で埋め、クライアントの自己申告だけを承認根拠にしない。
