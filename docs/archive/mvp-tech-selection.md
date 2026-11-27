# MVP 技術選定と一次資料（旧調査記録）

> **注記**: 本ファイルは 2026年10月1日時点の MVP 検討時の履歴保管用アーカイブです。現行の採用設計は [全体アーキテクチャ](../architecture.md) および [設計文書一覧](../README.md) を参照してください。

## MVPの技術選定と一次資料

調査日：2026年10月1日。[全体アーキテクチャ](../architecture.md)のために、公式ドキュメント・公開ソースで確認できる機能と、eneで採用する設計判断を分けて整理します。以下の「採用」は技術選定を意味し、eneでの動作確認や性能測定の完了を意味しません。以前の製品調査と参照コミットは維持します。

<a id="mvp-products"></a>

### 既存製品から採用する構造

| 参考元 | 確認した資料・構造 | MVPで採用する考え方 | そのまま導入しないもの |
| --- | --- | --- | --- |
| [AIRI](#airi) | [固定コミットの構成図](https://github.com/moeru-ai/airi/blob/b40e3e87b149ea5fb75d4944440493829e601411/README.md#sub-projects-born-from-this-project)、[VRMのライフサイクル改善](https://airi.moeru.ai/docs/en/blog/DevLog-2026.03.14/) | 対話・音声・描画の分離、モデルのロード・解放と表示状態の検証 | Web・デスクトップの技術構成そのもの。eneではGPUIとネイティブ描画を使う |
| [OpenClaw](#openclaw) | [常駐Gateway](https://docs.openclaw.ai/concepts/architecture)、[モデルとランタイムの分離](https://docs.openclaw.ai/concepts/agent-runtimes) | UIと独立して稼働するホスト、モデル接続と実行ループの分離、状態の再取得 | 多数のチャネル・拡張・リモート運用をMVPへ持ち込むこと |
| [nanobot](#nanobot) | [固定コミットのアーキテクチャ](https://github.com/HKUDS/nanobot/blob/66f5f2df15455b34fc22e656bdc1ef3d4f32e328/docs/architecture.md) | 対話と作業を分離し、非同期処理で表示や会話を止めない構造 | Pythonの実行基盤やツール群をアプリへ組み込むこと |
| [OpenCode](#opencode) | [ヘッドレスHTTPサーバー](https://opencode.ai/docs/server/)、[用途別エージェント](https://opencode.ai/docs/agents/) | ホストをAPI経由で操作するUI、会話用と作業用のセッション・モデルの分離 | 開発用の任意シェル実行や細かな権限エディタ |

これらから参照するのは責務と体験の設計です。各製品の実行エンジンをMVPの依存として採用したり、ツール公開の制限だけでOS権限を制限できるとみなしたりはしません。

<a id="mvp-ui"></a>

### UIの比較と採否

| 技術 | 対応OSと描画 | 配布・実行時の依存 | 本体のライセンス | eneでのメモリ実測 | 判断 |
| --- | --- | --- | --- | --- | --- |
| Electron | Windows・macOS・Linux。ChromiumでWeb UIを描画 | Chromium・Node.js、ブラウザー由来の複数プロセス | MIT | 未計測 | 不採用。ブラウザー実行基盤を常駐させない要件に合わせる |
| Tauri | Windows・macOS・Linux。OSのWebViewでWeb UIを描画 | Rust本体とWebView2・WKWebView・WebKitGTK等 | MIT／Apache-2.0 | 未計測 | 不採用。Electronと同じランタイムではないが、今回のUIはWebViewを使わない構成にする |
| Qt Quick | Windows・macOS・Linux。Qt Quickの独自UIを描画可能 | Qtライブラリ、プラグイン、Rust連携のための橋渡し | LGPL／GPL／商用。モジュールごとに条件が異なる | 未計測 | 不採用。OS対応の不足ではなく、Rust連携とQtの配布依存を今回の構成へ追加しない判断 |
| GPUI | Windows・macOS・Linux（Wayland・X11）。GPU描画（macOSはMetal、WindowsはDirect3D 11、Linuxは公開版0.2.2がblade、mainがwgpu） | 選択した描画バックエンドとGPU等のOS依存 | Apache-2.0 | 未計測 | 会話・設定UIに採用。UIをRustの型付きコードで書き、Tailwind風のスタイルAPIを使う |
| Slint | Windows・macOS・Linux。独自コンポーネントと複数の描画バックエンド | 選択したネイティブバックエンド・GPU等のOS依存 | Royalty-free／GPLv3／商用 | 未計測 | 不採用。`.slint`マークアップとRustの二言語構成を避け、UIをRustコードに一本化する |
| Godot | Windows・macOS・Linux。2D・3D・UIをエンジンで描画 | エクスポートしたエンジン、VRM連携用の追加部品 | MIT。追加部品は別途確認 | 未計測 | 不採用。今回のRust構成ではBevyのVRM連携を選ぶ。Wayland常駐表示を未検証のまま対応済みとしない |
| Bevy | Windows・macOS・Linux。Rustの描画・アニメーション基盤 | 有効化したエンジン機能、GPU・ウィンドウ関連の依存 | MIT／Apache-2.0 | 未計測 | キャラクター描画に採用。会話・設定UIまでBevyで構築せず、機能を絞る |

一次資料：[Electronのプロセスモデル](https://www.electronjs.org/docs/latest/tutorial/process-model)、[Electronのライセンス](https://github.com/electron/electron/blob/main/LICENSE)、[Tauriの概要](https://v2.tauri.app/start/)、[Tauriのライセンス](https://github.com/tauri-apps/tauri#license)、[Qtの対応OS](https://doc.qt.io/qt-6/supported-platforms.html)、[Qtの配布](https://doc.qt.io/qt-6/deployment.html)、[Qtのライセンス](https://www.qt.io/development/qt-framework/qt-licensing)、[CXX-Qt](https://github.com/KDAB/cxx-qt)、[GPUIの公式サイト](https://gpui.rs/)、[GPUIのREADME](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)、[GPUIのライセンス](https://crates.io/crates/gpui)、[SlintのデスクトップUI](https://slint.dev/desktop)、[Slintのライセンス](https://github.com/slint-ui/slint/blob/master/LICENSE.md)、[Godotの機能](https://docs.godotengine.org/en/stable/about/list_of_features.html)、[Godotのライセンス](https://godotengine.org/license/)、[Bevyの公式リポジトリ](https://github.com/bevyengine/bevy)。

この比較は性能順位を示すものではありません。常駐時の消費資源はウィンドウ数、モデル、描画頻度、依存機能によって変わるため、採用構成をリリースビルドで測定します。Pythonは利用者の環境とは独立して配布することも可能ですが、ene本体ではランタイムと依存管理を追加せずRustを選びます。言語だけでファイル操作の隔離が成立するとは判断しません。

GPUIはUIをRustのコードとして書き、レイアウトとスタイルをTailwind風のAPIで組み立てます。`.slint`のような別言語のマークアップを挟まず、ビューは`Render`トレイトを実装した`Entity`です。Linuxでは`wayland`と`x11`の機能を明示して有効にします。GPUIはpre-1.0で破壊的変更があるとREADMEが明記しているため、採用時はバージョンを固定します。[出典：GPUIのREADME](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)、[gpui 0.2.2のcrates.io](https://crates.io/crates/gpui)

> [!IMPORTANT]
> **公開版0.2.2とmainの差**:
> 2026-10-22公開の`gpui` 0.2.2は、`WindowKind`が`Normal`・`PopUp`・`Floating`のみで、Linux描画にbladeを使います。一方、mainではLinux描画がwgpuへ移行し（[PR #46758](https://github.com/zed-industries/zed/pull/46758)）、`WindowKind::LayerShell`が追加されています。eneがlayer-shell常駐を実装する時点で、固定するバージョンがこの2機能を含むかを確認し、含まなければgitリビジョン固定か公開待ちを選びます。[出典：gpui 0.2.2のWindowKind](https://docs.rs/gpui/0.2.2/gpui/enum.WindowKind.html)、[mainのlayer_shellモジュール](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/platform/layer_shell.rs)

GPUIのライセンスはApache-2.0で、Zed本体のGPL-3.0とは別です。eneは`gpui`クレートをApache-2.0のまま利用するため、Slintで必要だった帰属表示の義務や、プロプライエタリ配布時の商用ライセンス選択は発生しません。テーマ・配色・配置のカスタマイズはene側のコードで行います。[出典：gpuiクレートのライセンス](https://crates.io/crates/gpui)、[Zedのライセンス一覧](https://zed.dev/software-overview)

<a id="mvp-runtime"></a>

### Rust実行基盤とVRM描画

現在の実装は`ene-vrm`を使い、glTF共通処理をBevy標準へ委譲します。以下のbevy_vrm1の記述は初期選定時の参考資料です。

[Tokio](https://tokio.rs/tokio/tutorial)は非同期処理、[Axum](https://docs.rs/axum/latest/axum/)はHTTPとWebSocketのホストAPIに用います。[Rig](https://rig.rs/docs/)はAIプロバイダー接続とストリーミングに利用します。[OpenRouterプロバイダーのAPI資料](https://docs.rs/rig/0.43.0/rig/providers/openrouter/index.html)には接続とルーティング設定の受け渡しがあり、モデル接続をeneのタスク実行制御から分離する材料になります。権限、停止、バックアップ、ファイル変更の成否確認はeneの責務として残します。

[bevy_vrm1の固定コミットのREADME](https://github.com/not-elm/bevy_vrm1/blob/c8601e2854bd9c6cce0f6d282c7d90a65a895fb7/README.md)はVRM 1.0のみを対象とし、Spring Bone、Look At、VRMAなどの対応を示しています。同時に開発初期で破壊的変更があり得ることを明記しています。[同コミットのCargo.toml](https://github.com/not-elm/bevy_vrm1/blob/c8601e2854bd9c6cce0f6d282c7d90a65a895fb7/Cargo.toml)はパッケージ0.9.5、Bevy 0.19系、MIT／Apache-2.0を示し、[MToonの実装](https://github.com/not-elm/bevy_vrm1/blob/c8601e2854bd9c6cce0f6d282c7d90a65a895fb7/src/vrm/mtoon/material.rs)も参照できます。これは確認したソースの組み合わせであり、eneでのモデル互換性や公開パッケージの採用バージョンを検証した記録ではありません。

eneは既存のVRM描画を利用し、独自のVRMレンダラーを一から作ることを避けます。基本的な待機モーション、透明な描画、モデル変更・終了時の資源解放を先行試作で確認します。Bevyの音声・UI・ゲームパッドなど不要な機能は無効にします。

<a id="mvp-wayland"></a>

### Waylandと描画の連携

[Smithay Client Toolkitのwlr layer-shell対応](https://smithay.github.io/client-toolkit/smithay_client_toolkit/shell/wlr_layer/index.html)と、[layer-shellの公式プロトコル定義](https://github.com/swaywm/wlr-protocols/blob/master/unstable/wlr-layer-shell-unstable-v1.xml)は、出力、レイヤー、アンカー、余白、作業領域の占有、キーボード入力の設定を提供します。透明部分のクリック透過はウィンドウ全体の透過設定ではなく、[Waylandのsurface入力領域](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_surface)を制御する必要があります。

[Bevy 0.19.0のウィンドウハンドル実装](https://github.com/bevyengine/bevy/blob/v0.19.0/crates/bevy_window/src/raw_handle.rs)には、ネイティブハンドルとウィンドウの寿命を保持して描画側へ渡す仕組みがあります。これは独自のWaylandウィンドウを接続する設計の根拠ですが、Smithayのlayer-shellウィンドウとBevyの連携が完成済みであることを示しません。OSごとのスレッド制約とウィンドウの寿命を守る接続、透明描画、入力領域、ドラッグ、DPI・出力変更の実機検証を必要とします。

eneでは通常のGPUI画面とキャラクターの常駐ウィンドウを分離し、最初の検証環境をKDE/KWinとします。GPUIのmainはWayland向けに`WindowKind::LayerShell`と`LayerShellOptions`を提供し、Overlayレイヤーへの配置、画面端へのアンカー、マージン、キーボード入力の可否を指定できます。公開版0.2.2にはこの変種が無いため、常駐ウィンドウの実装は公開状況の確認を前提とします。layer-shellを提供しない環境への対応や、Windows・macOSの常駐APIは別のOSアダプターで扱い、クロスプラットフォームのライブラリを選んだだけで常駐機能も対応済みとはしません。[出典：GPUIのlayer_shellモジュール](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/platform/layer_shell.rs)、[layer-shellのサンプル](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/layer_shell.rs)、[gpui 0.2.2のWindowKind](https://docs.rs/gpui/0.2.2/gpui/enum.WindowKind.html)

<a id="mvp-host"></a>

### ホストと接続の設計

[OpenCodeのServer](https://opencode.ai/docs/server/)はヘッドレスサーバーと外部UIの分離、[OpenClawのGateway architecture](https://docs.openclaw.ai/concepts/architecture)は常駐ホストと接続・状態管理を検討する参考です。eneでは同一PCの認証付きループバックHTTPとWebSocketを採用し、永続状態の取得と通知を分離します。各製品の通信プロトコルやSDKをそのまま採用するものではありません。

[systemdのユーザーサービス](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)をLinuxでのホスト運用に使い、GUIの終了からホストの寿命を分離します。再接続では保存された結果と現在の状態を取得し、要求IDで開始要求の重複を防ぎます。ホスト停止後のタスク自動再開はMVPに含めません。

[AxumのWebSocket資料](https://docs.rs/axum/latest/axum/extract/ws/index.html)はHTTPからの接続確立と送受信の並行処理を示しています。[Tokioのbroadcast資料](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html#lagging)は有限バッファで遅い受信側が通知を失い、`RecvError::Lagged`で検出できることを説明しています。eneでは通知と音声の接続・待ち行列を分け、通知欠落時は購読とスナップショットから再同期します。これはeneの通信契約であり、ライブラリが状態同期や停止制御まで自動で提供するという意味ではありません。具体的な設計は[ホスト API の概要](../api.md)にまとめます。

<a id="mvp-reference-config"></a>

### AIと音声の確認用構成

OpenRouterの[GPT-6 Lunaモデルページ](https://openrouter.ai/openai/gpt-6-luna)で、モデルID`openai/gpt-6-luna`の掲載とツール呼び出し対応を確認しました。eneの会話用・作業用の確認構成はこのモデルとします。価格やベンチマークを採用条件として固定せず、実際の会話、ストリーミング、ファイル操作に必要なツール呼び出しを接続テストで確認します。[出典：Tool calling](https://openrouter.ai/docs/guides/features/tool-calling)

モデルIDだけでは配信先は限定されません。[Provider routing](https://openrouter.ai/docs/guides/routing/provider-selection)の`only`と`allow_fallbacks`を用い、確認用構成は`provider.only: ["openai"]`、`provider.allow_fallbacks: false`とする方針です。送信先としてOpenRouterとOpenAIの双方を提示し、条件を満たす接続先が利用できなければエラーを報告します。指定モデル・配信先の実API呼び出しは本調査では行っていません。

音声の参考候補として同一PC上の外部VOICEVOX Engineを調査しましたが、MVPの接続先をこれに限定しません。音声APIはエンジンから独立したストリーミング契約とし、合成・配信・再生の開始遅延、発話の自然さ、中断性能を検証して構成を1系統選定します。資料で確認できるAPIと、eneからの合成・再生が動作したことは区別し、後者を実装後の検証条件にします。

<a id="mvp-audio"></a>

### 音声と認証情報

[VOICEVOX Engineの公式README・API](https://github.com/VOICEVOX/voicevox_engine)は、`audio_query`と`synthesis`による音声合成、話者の取得を提供します。この既存の調査結果は、接続アダプターの参考として維持します。eneは外部エンジンを利用し、アプリの配布物に音声合成ランタイムを組み込みません。ローカルエンジン側のPython等の依存は、ene本体の依存と区別して扱います。

2026年10月1日に追加確認した[公式READMEのストリーミング例](https://github.com/VOICEVOX/voicevox_engine/blob/master/README.md#ストリーミングで音声合成するサンプルコード)と[音声合成ルーターの公開ソース](https://github.com/VOICEVOX/voicevox_engine/blob/master/voicevox_engine/app/routers/tts_pipeline.py)には、`AudioQuery`を受けて音声を逐次返す`streaming_synthesis`があります。出力は`audio/wav`であり、WAVという形式とストリーミング配信は両立します。一方、増え続けるテキストの逐次入力への対応を示すものではありません。READMEは別リクエストの音声を結合する場合のつなぎ目にも注意を求めています。今回確認したのは`master`の資料で、採用リリース・コアの対応と実際の遅延は未検証です。

[キャンセル可能な合成の説明](https://github.com/VOICEVOX/voicevox_engine/blob/master/README.md#キャンセル可能な音声合成)には、通常の`synthesis`は切断後も計算を続ける一方、実験的な`cancellable_synthesis`は起動オプションで有効化する必要があると記載されています。ストリーミング出力と合成キャンセルは別の能力として確認します。eneのAPIではテキスト逐次入力・音声逐次出力・合成キャンセルを区別し、発話を取り消した後の音声を配信・再生しない契約を持ちます。エンジン側で計算を中断できることを一律に保証しません。

[rodio](https://docs.rs/rodio/latest/rodio/)をクライアントの音声再生基盤とし、音声デバイス連携にはその基盤の[CPAL](https://github.com/RustAudio/cpal#supported-platforms)を利用します。既存のWAV再生候補に加え、届いた音声チャンクから再生する連携を検証します。必要な再生・デコード機能だけを有効化し、UIのイベントループを再生待ちで止めません。コーデックやチャンクの解釈・バッファ量は未選定です。読み上げオフ・切断時の待ち行列の取り消し、遅着音声の破棄、合成・再生失敗時のテキスト継続はene側の設計です。

[keyring-rsの公式構成説明](https://github.com/open-source-cooperative/keyring-rs/wiki/Keyring)を参照し、共通基盤のkeyring-coreとOS別の資格情報ストアを使います。LinuxはSecret Service、将来はWindows Credential Manager・macOS Keychainへ接続する構成です。ストアが使えない場合に、APIキーを通常の設定やSQLiteへ平文保存する代替処理は設けません。

<a id="mvp-safety"></a>

### 隔離機構と権限

[bubblewrapの公式README](https://github.com/containers/bubblewrap)は、Linuxの名前空間やマウントを利用する隔離機構を提供する一方、実際の安全性は呼び出し側のポリシーに依存することを明記しています。eneでは許可フォルダ、最小限の読み取り専用ランタイム、専用の復旧・一時領域だけをワーカーへ公開し、ネットワークやホストの秘密情報を公開しない構成にします。

OSの隔離と、拡張子・文字コード・相対パス・競合・停止状態などの操作検証は併用します。LLMへの指示やツール名の制限だけをアクセス制限として扱いません。許可と停止は実行側が管理し、適用済み変更の報告と復旧用コピーを残すことを[環境設定・権限・拡張](../features/setup.md)に合わせます。

<a id="mvp-storage"></a>

### SQLiteと記憶検索

[SQLiteの公式用途説明](https://www.sqlite.org/whentouse.html)は、個々のアプリやデバイスのローカル保存を適した用途として挙げています。[WALの公式仕様](https://www.sqlite.org/wal.html)では読み取りと書き込みを並行できる一方、書き込みは同時に1つ、WALを利用するプロセスは同じホスト上である必要があります。eneではホストだけがDBを所有し、専用スレッドで処理を直列化する構成を選びます。クライアント数が増えただけでDBファイルをネットワーク共有することはしません。

[rusqlite](https://docs.rs/rusqlite/latest/rusqlite/)の組み込みSQLite構成を使い、OSごとのSQLiteバージョン差を避けます。SQLiteのトランザクションと作業フォルダのファイル操作は別であるため、変更前コピーと操作記録、実際の変更を照合して復旧する設計を採ります。

[FTS5のtokenizer仕様](https://www.sqlite.org/fts5.html#tokenizers)にはUnicodeの単語単位とtrigramによる部分一致があります。trigramの全文検索では3文字未満の部分文字列は一致しない制約があるため、日本語の短い名前・単語を含めて検索方法を検証します。FTS5を有効にするだけで日本語の意味検索まで完成するとはみなしません。

[sqlite-vecの公式README](https://github.com/asg017/sqlite-vec)には、SQLite内でのベクトル保存・検索、Rust向けの組み込み方法、Linux・Windows・macOSなどでの利用が示されています。ただし1.0未満で破壊的変更が想定されます。将来の評価候補とし、今回のMVPには導入しません。

eneの設計判断は、記憶本体と出典・所有者・共有範囲・訂正・優先度をSQLiteの正本に持ち、キーワード・ベクトル索引を再構築可能な検索手段として分離することです。複数ホストの大量同時書き込みや検索速度・メモリの実測で必要になれば、保存先または検索エンジンを再評価します。[記憶とスキル](../features/memory.md)は維持し、MVPでは会話履歴とタスク結果の永続化までを実装します。
