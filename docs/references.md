# 参考元

<a id="alpha-design-sources"></a>
## Alphaホスト設計の調査・採否（2026-11-03）

現在の技術判断はこの節と[採用設計](architecture.md)を参照する。以前のMVP選定や製品比較は末尾に履歴として保持し、最新実装・完成機能と混同しない。一次資料67件のIDと確認範囲を[調査索引](research/mind-sources.md)に収録した。全文精読・実機検証を一律に実施したという意味ではない。

<a id="alpha-rig"></a>
### Rigへの委譲

[S01](research/mind-sources.md#s01)〜[S05](research/mind-sources.md#s05)を基に、調査基準版v0.46.0のrig-ecs、会話保存、ツール、モデル、拡張点を優先する。独自の第二カーネル・受信箱・履歴の複製を作らない。ステップ1の実装でv0.46.0を採用し`Cargo.lock`へ固定した。接続、途中状態の記録、停止、待機時負荷は[全体アーキテクチャ](architecture.md)および[技術スタック](design/tech-stack.md)を基準とする。

<a id="alpha-mind"></a>
### 人格・心

| 参照 | 取り入れるもの | そのまま採らないもの |
| --- | --- | --- |
| [AIRIのカード](research/mind-sources.md#p02)、[SillyTavern](research/mind-sources.md#p11)、[MRPrompt](research/mind-sources.md#r03) | 小さな人格の核、関連する側面・例・背景の利用 | 全設定の常時投入、固定台詞表、カードの指示を権限へ昇格させること |
| [AIRIのADR・実装](research/mind-sources.md#p03)・[記憶ツール](research/mind-sources.md#p04) | 人格別の記憶、詳細の必要時参照、子の出所付き通知 | 親の保存ツールだけへの依存、eneと異なる削除・共有・履歴保持数 |
| [SocioMind](research/mind-sources.md#r04)・[実装](research/mind-sources.md#r05) | 人格・感情・関係・経験の結合 | 心理尺度の固定採用、全社会シミュレーター、多数のモデル呼び出し |
| [DualMem](research/mind-sources.md#r06) | 出来事と人格に沿う解釈を区別し、根拠で結ぶ | 学習手順・実験成績がeneでも成立するという推定 |
| [AITuberKit](research/mind-sources.md#p07)、[OnAir](research/mind-sources.md#p09)、[表情分類](research/mind-sources.md#p14) | 応答と声・表情をつなぐ表現情報 | 表情タグを持続する感情・関係性そのものにすること |

作者が作った会話例・架空の背景は実体験ではない。共有された話を自分の直接経験にせず、想起・再整理を新しい経験として強化しない。これらはene側の設計判断であり、研究の心理モデルを科学的に確定した人間の心として実装するものではない。

<a id="alpha-memory"></a>
### 自動記憶と検索

[Letta](research/mind-sources.md#m01)、[LangMemの遅延処理](research/mind-sources.md#m04)から、会話中の更新と後続の増分整理を分ける。[Mem0](research/mind-sources.md#m06)の既存記憶との照合、[Nemori](research/mind-sources.md#m11)の経験をまとまりとして扱う考え方を参照する。これらの実行基盤を丸ごと重ねず、Rig上の有限の内部処理へ接続する。自動処理の起動機会と、正確な記憶を形成する品質は別に検証する。

[RisuAI](research/mind-sources.md#p17)の由来と複数の想起手掛かりを参考に、全文・意味・時間・関連を利用する。[Graphiti](research/mind-sources.md#m08)や[A-Mem](research/mind-sources.md#m10)は比較対象だが、初版で全記憶をグラフDBへ入れない。[MemoryBank](research/mind-sources.md#m12)等の時間・忘却の考え方を、固定の消去式へ変換しない。

検索接続は[SQLite FTS5](https://www.sqlite.org/fts5.html)のtrigram全文検索と、SQLite BLOBに保存した埋め込みベクトルのRust内コサイン類似度計算によるハイブリッド検索を採用し、Rust側でRRF（Reciprocal Rank Fusion）統合する。日本語分割、索引更新、資源使用量を実測し、外部C拡張や重厚なベクトルDBを抱え込まず、最小・堅牢な構成とする。

<a id="alpha-permissions"></a>
### 権限・隔離

[Rigのガイド](research/mind-sources.md#s05)と[承認実装](research/mind-sources.md#s06)のGate・Hold・Ask・理由集約を再利用する。回答者不在で即拒否する既定動作は、eneの離席時保留へ接続を要する。[sandbox corpus](research/mind-sources.md#s07)は例であり汎用の安全性保証ではない。

[Sandbox Runtime](research/mind-sources.md#s08)等の既存隔離を比較する。既定の広い読取、配布依存、実行中に変更できる規則の制約を検証する。[Codex](research/mind-sources.md#s11)・[OpenCode](research/mind-sources.md#s13)からも、確認頻度と実効アクセス範囲を別軸として扱う。[MCPの注釈](research/mind-sources.md#s14)を未信頼ツールへの認可根拠にしない。[Cedar](research/mind-sources.md#s15)等のDSLは必要性のない段階では採用しない。

<a id="alpha-work"></a>
### 作業相手とACP・MCP

[ACPセッション](https://agentclientprotocol.com/protocol/session-setup)、[prompt turn](https://agentclientprotocol.com/protocol/prompt-turn)、[公式Rust SDK](https://agentclientprotocol.com/libraries/rust)を基準に、eneのsession・入力・更新・停止をアダプターへ写像する。ACP全機能をプラグインへ強制しないが、対応して公開する部分では要求ID・エラー・取消・完了の意味を守る。

2026-11-05時点の採用版は `agent-client-protocol 5.0.0`（ACP v1）と `agent-client-protocol-schema 1.15.1`。JSON-RPCのフレーミング・要求ID・応答対応付け・既知メソッドへの型付けはSDKへ委譲し、ene側は子プロセスの起動、環境変数許可リスト、workspace境界、terminal権限・上限、ターン状態と停止だけを持つ。公式SDKの`AcpAgent`にはプロセス起動を委譲しない（`env_clear`と許可環境変数を表現できないため）。子プロセスのグループ終了とkill-on-dropは `process-wrap 10.0.1` に委譲し、cancel/timeout時に孫プロセスを残さない。未知の`stopReason`やRPCエラーは`end_turn`へ丸めない。

共通ツールの外部公開は[MCP公式Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)を優先する。ACPのモデルや認証は接続先の能力に従う。独自機能は能力と名前空間で表し、未対応を成功や自動的な別動作へ変換しない。実際のアダプター版で双方向の最小互換性を検証する。

<a id="alpha-evaluation"></a>
### 評価の採り方

[InCharacter](research/mind-sources.md#r10)、[REMP](research/mind-sources.md#r12)、[LongMemEval](research/mind-sources.md#m15)、[LoCoMo](research/mind-sources.md#m17)を評価観点の資料とする。要旨のみの研究から個別の実装方式や数値保証を導かない。形成・検索・利用、人格・関係・感情・記憶、実務の正確さと資源を分ける。

[Persistent Personas?](research/mind-sources.md#r13)を踏まえ雑談以外の実務も長期に評価し、[匿名評価](research/mind-sources.md#r14)を踏まえ独自キャラクターも使う。評価シナリオは実行時の分岐表ではない。論文の条件・モデル・言語・採点法をeneの性能保証へ置き換えない。

---

## 旧MVPおよび初期構想の調査記録

以下は以前の選定・比較の記録。本文中の「今回」「MVP」は当時の範囲を指す。現在の規範と矛盾する場合は上記Alpha節と各設計文書を優先する。

## 設計における参照プロダクト

キャラクターとしての臨場感や存在感の設計には AIRI や Desktop Mate、タスクの依頼から成果物の受け取りに至る体験設計には Grok Bot・Claude Cowork・ChatGPT Work を参考にします。また、nanobot・OpenClaw・OpenCode・Hermes Agent は、外部連携、アクセス権限、記憶保持、自律実行アーキテクチャを検討するための技術的材料とします。

記憶システムについては、Replika や Kindroid が提供する継続的な関係性の育成体験と、Claude・Hermes Agent・OpenClaw・Letta が持つ実務文脈や手順の引き継ぎ構造を対比・分析します。[記憶機能の比較と採否](#記憶機能の比較と採否)には、2026年9月30日時点の公式資料に基づく調査結果と、eneにおける設計判断をまとめています。

以下の表は、各プロダクトの主要な課題解決アプローチと、eneにおける着目点を示したものです。たとえば Claude Cowork と ChatGPT Work はいずれも成果物のプレビューや修正を扱いますが、本書では Cowork の「対話入口とアクセス権限の管理」、Work の「成果物の確認体験とプラグイン構造」にそれぞれ焦点を当てて参照します。

| 参考元 | 主な解決領域 | 実行環境・稼働の前提条件 | eneにおける着目点 |
| --- | --- | --- | --- |
| [AIRI](#airi) | 姿と声を持つキャラクターとの対話・インタラクション | ブラウザ／デスクトップ等。推論場所は接続先依存 | 描画・音声・対話の統合アーキテクチャと部品交換性 |
| [Desktop Mate](#desktop-mate) | PC作業中もキャラクターがそばに寄り添う体験 | ローカルPC上で常駐起動 | 画面上の配置、自然なリアクション、作業を妨げない常駐性 |
| [Grok Bot](#grok-bot) | 継続的な実務タスクを複数の専門Botへ委任 | ユーザー専用の常設クラウド仮想PC | 役割分担、タスクの引き継ぎ、必要最小限の対話確認 |
| [Claude Cowork](#claude-cowork) | ローカルファイルや外部サービスを横断する多工程タスク | クラウド実行環境（ベータ）。ローカル資源利用にはDesktop接続が必須 | 通常対話から実務へのシームレスな移行とアクセス権限管理 |
| [ChatGPT Work](#chatgpt-work) | プレビュー・修正・再利用が容易な成果物の作成 | クラウド／ローカル。利用条件はアカウント種別等に依存 | 成果物の確認・調整フロー、プラグイン構造 |
| [nanobot](#nanobot) | 個人でセルフホスト運用可能な軽量エージェント基盤 | ローカル／サーバー。定期実行にはGatewayの常時稼働が前提 | 軽量な実行ループ設計、Web検索・MCPツールの柔軟な構成 |
| [OpenClaw](#openclaw) | 多様な対話チャネルとバックエンド実行基盤の統合 | 常駐型Gatewayと各種実行ランタイム | UI、接続先、推論モデル、実行エンジンの明確な分離 |
| [OpenCode](#opencode) | 用途に応じたモデル選定によるソフトウェア開発支援 | クライアントとヘッドレスHTTPサーバー | 専門用途別モデルの使い分け、権限ルール、外部UI連携 |
| [Hermes Agent](#hermes-agent) | 知識・記憶・作業手順を蓄積し、反復作業を自律実行 | ローカル／サーバー。標準のスケジュール実行はGatewayが管理 | 長期記憶・スキル体系・実行履歴のレイヤー分離 |

## AIRI

[AIRI](https://github.com/moeru-ai/airi/blob/b40e3e87b149ea5fb75d4944440493829e601411/README.md#project-airi) は、会話やゲームを共に楽しむAIキャラクターの実現を目指すオープンソースプロジェクトです。ユーザーがキャラクターの外観、声、対話モデルを自由に組み合わせられる点で、eneが掲げる「体・声・心」のコンセプトと親和性の高い着想を持っています。

### 描画・音声・対話をモジュールとして組み合わせる構造

公式READMEでは、Webブラウザ版の Stage Web、デスクトップ版の Stage Tamagotchi、実験的なモバイル版の Stage Pocket が提示されています。アーキテクチャ図によれば、これらは対話・音声・描画などの共通パッケージを共有し、Discordやゲームとの連携は外部サービスとして疎結合に接続されます。[出典：READMEの構成図](https://github.com/moeru-ai/airi/blob/b40e3e87b149ea5fb75d4944440493829e601411/README.md#sub-projects-born-from-this-project)

確認できる主要な機能構成は以下のとおりです。[出典：Current Progress & Roadmap](https://github.com/moeru-ai/airi/blob/b40e3e87b149ea5fb75d4944440493829e601411/README.md#current-progress--roadmap)

- **体の表現**：VRM形式の3DアバターおよびLive2Dに対応し、まばたき、視線追従、待機モーションなどを制御。
- **音声入力**：ブラウザやDiscordからの音声入力、クライアントサイドでの音声認識（STT）、発話区間検出（VAD）を処理。
- **声の生成**：音声合成（TTS）の接続先として、ElevenLabs、Azure Speech、OpenAI互換API、ローカル実行のKokoro TTSなどを選択可能。

### 常駐表示におけるモデルのロード・アンロード設計

2026年3月14日付の開発ログでは、キャラクター切り替え時におけるGPUメモリおよびRAMの肥大化、非同期ロードに伴う複数ウィンドウ間の競合が報告されています。遅延して到着した古い読み込み結果が最新状態を上書きしてしまったり、設定画面のプレビュー表示とメイン画面の表示が互いの状態を干渉し合ったりする現象です。これに対し、表示領域ごとのキャッシュ分離、ロード要求の世代管理（Generation ID）、切り替え時の明示的なリソース解放機構が導入されました。[出典：VRMステージのライフサイクル改善](https://airi.moeru.ai/docs/en/blog/DevLog-2026.03.14/)

この事例から、eneにおいても単に「モデルを描画できた」段階で完成と見なすのではなく、設定画面の開閉、モデルの連続変更、キャラクターの非表示化といった実運用操作を含めた常駐品質の検証が不可欠であると判断されます。

### 実装済み機能とロードマップの峻別

AIRIのドキュメントでは、VRM・Live2Dの基本描画や音声連携といった実装済み機能と、開発途上にある記憶システムやWebGPUを用いたブラウザ内完全ローカル推論とが区別されています。「クライアントアプリがローカルで起動する」からといって、推論・音声・記憶の全工程が完全オフラインで完結しているわけではありません。[出典：Roadmapとモデル接続先](https://github.com/moeru-ai/airi/blob/b40e3e87b149ea5fb75d4944440493829e601411/README.md#current-progress--roadmap)

**eneへの反映案**：描画、音声入力、音声合成、対話モデルを疎結合なモジュールとして独立して交換可能とし、各接続先へ送信されるデータの範囲を明示します。対話パイプラインの進行状態（「傾聴中」「思考中」「発話中」など）をキャラクターの描画系へ即座に伝達し、音声生成のレイテンシ中も視線や仕草で生き生きと表現します。なお、「体・声・心」を一体化して配布するフォーマットはAIRIの仕様に依存せず、[ene独自のキャラクターパッケージ](features/character/create.md)として策定します。

## Desktop Mate

[Desktop Mate](https://www.infiniteloop.co.jp/desktopmate/) は、株式会社インフィニットループが提供するデスクトップマスコットです。公式ライセンスキャラクターを追加コンテンツ（DLC）として導入できるエコシステムを持ちます。eneにとっては、日常のPC作業環境とキャラクターのインタラクションをどのように調和させるかを検討する格好のリファレンスです。

### マウス操作とウィンドウ配置をキャラクターの居場所とする演出

キャラクターをドラッグして任意のウィンドウ上へ配置するとウィンドウの上辺に腰掛け、ウィンドウの移動に追従したり、マウスポインタの動きに反応したりします。クリックや頭を撫でる操作に応じた音声リアクションや、アラーム通知機能も備えています。表示スケールの柔軟な調整や、作業の邪魔にならない画面端への一時退避機能も実装されています。[出典：Steamの機能紹介](https://store.steampowered.com/app/3301060/Desktop_Mate/)

この体験から学ぶべき点は、明示的な会話を行っていない時間であっても、ユーザーのPC操作に対してキャラクターが自然かつ愛着の湧く反応を返す点です。eneにおいても、ドラッグ中の浮遊姿勢、着地時の落ち着いた仕草、通知時の視線追従などを丁寧に作り込むことで、デスクトップ常駐と対話体験を自然に結びつけられると考えられます。

### 動作環境とキャラクター利用規約の考慮

公式FAQではWindowsおよびAppleシリコン搭載Macへの対応が案内されています。複数キャラクターの同時表示は調査時点で最大2体（オープンベータ）であり、配信利用等のガイドラインはキャラクターごとに個別に定められています。[出典：公式FAQ](https://www.infiniteloop.co.jp/desktopmate/)

公式資料の範囲では、LLMによる自由対話、任意のVRMの汎用インポート、自律的な作業代行といった機能は含まれていません。単なるボイス付きリアクションと、自律対話エージェントとしての音声対話機能は明確に区別して設計する必要があります。また、透明ウィンドウのクリック透過やウィンドウ境界の検出ロジックはOS固有の低レイヤーAPIに依存します。

**eneへの反映案**：[デスクトップ表示](features/character/desktop.md)において、自由な画面配置とクリック透過を基盤としつつ、ウィンドウ境界への着座やポインタ追従、作業集中時の自動退避などを段階的に導入します。他者の作成したモデルデータを導入する際には、著作権や利用条件をユーザーが直感的に確認できる導線を整えます。

## Grok Bot

[Grok Bot](https://x.ai/bot) は、明確な役割を持つ複数のBotへチャット形式で仕事を依頼し、バックグラウンドで継続的にタスクを遂行させるサービスです。外部アプリケーションを操作して成果物を持ち帰る自律的なワークフローを志向しています。

### 役割分担されたBot間のタスク連携と引き継ぎ

公式発表では、情報収集、営業支援、経費精算、不具合再現といった専門Botを作成し、統括役のBotやグループチャットを通じて連携させるユースケースが紹介されています。Bot同士は対話のコンテキストを共有しながらタスクをリレーします。また、ユーザーがブラウザ上で一度操作手順を実演してみせることでタスク手順を学習し、適宜修正を加えながら反復利用する仕組みを備えています。[出典：Introducing Grok Bot](https://x.ai/news/introducing-grok-bot)

たとえば「リサーチを行う」「調査結果に基づき連絡文面を下書きする」「確認用の一覧表をまとめる」という一連の業務を、役割分担された複数セッションで分担する体験です。ただし、公式事例における高い生産性は特定の業務シナリオに基づくものであり、あらゆるタスクにおいて同様の精度が保証されているわけではありません。

### 単一の仮想PC環境の共有と環境分離のトレードオフ

製品FAQによれば、各Botはクラウド上の常設仮想PC上で稼働し、ユーザーが端末を閉じてオフラインになっても作業を継続します。同一ユーザーに紐づくBot群はファイルシステム、ブラウザ環境、ログインセッションを共有しており、サンドボックスの隔離単位はBot単位ではなくユーザー単位です。定期的なバッチ実行や、安全性が懸念される機微な操作に対する自動レビュー機構（Auto Review）も案内されています。[出典：Grok Botの機能・FAQ](https://x.ai/bot)

この設計から導かれる洞察として、「Bot（エージェント）のインスタンスを増やすこと」と「アクセス権限や作業環境を厳格に分離すること」は別個の設計課題です。共有環境は情報の引き継ぎを容易にする反面、同一ファイルや同一ブラウザセッションを並行操作する際の競合制御が課題となります。

**eneへの反映案**：タスクの受付、サブエージェントへの委任、進捗管理、ユーザーへの確認という各フェーズを整理します。eneでは[用語集](terms.md)に定めたとおり、ユーザーとの対話窓口は常にひとりのコンパニオンが一貫して担います。複数の内部エージェントが稼働する場合も、共有すべき資料と各タスク内部に閉じる作業領域を明確に定義します。なお、Grok Botの24時間自律稼働は常設クラウド仮想PCを前提としており、eneでもホストは常時稼働を前提としますが、操作対象となるクライアント端末がオフラインの場合には端末依存タスクを安全に保留する[eneの動作モデル](features/autonomy/schedule.md#制約と検討事項)として仕様を策定します。

## Claude Cowork

[Claude Cowork](https://support.claude.com/en/articles/13345190-get-started-with-claude-cowork) は、Claude Codeで培われた自律コーディングエージェントの機構を、ファイル整理、リサーチ、ドキュメント作成といった一般的なナレッジワーク全般へと拡張したサービスです。ターミナル上で低水準なコマンドを組み立てるのではなく、求めるゴールを自然言語で伝えて作業を任せる体験を提供します。

### 通常対話とタスク実行のインターフェース統合

調査時点の公式案内において、従来のChat機能とCowork機能の境界をなくし、単一の対話画面へとシームレスに統合する体験が段階的にロールアウトされています。ユーザーが作業モードを手動で切り替える必要はなく、Claude側が依頼内容の性質に応じて、即答、リサーチ、複数ファイルの生成・編集などを自動で判断して進めます。[出典：Claude Cowork and chat are one Claude](https://support.claude.com/en/articles/16761823-claude-cowork-and-chat-are-one-claude)

ワークフローとしては、依頼内容に基づく実行計画の立案、サブタスクへの分解、コードやツールの実行、生成された成果物のインラインプレビューおよびダウンロードという流れが確立されています。作業の途中で追加の指示を与えることが可能で、数式入りのスプレッドシートやスライド資料など多様なファイル形式に対応します。また、作業全体に適用するグローバル指示と、特定フォルダに限定したローカル指示を使い分けることが可能です。[出典：タスクの実行手順と指示設定](https://support.claude.com/en/articles/13345190-get-started-with-claude-cowork)

### クラウド隔離環境とローカルリソースの接続管理

セキュリティガイドラインによれば、タスク処理はセッションごとに隔離されたクラウドサンドボックス上で安全に実行され、手元のローカルフォルダやブラウザへのアクセスは Claude Desktop を中継して行われます。デスクトップアプリが起動していない場合は手元のローカルリソースへ到達できず、ローカルファイルを開いた場合もその内容はAnthropicの推論基盤上で処理されます。[出典：Use Claude Cowork safely](https://support.claude.com/en/articles/13364135-use-claude-cowork-safely)

アクセス権限管理においては、接続対象フォルダやコネクタの明示的な選択と、ツール実行時の確認ポリシーが用意されています。統合版インターフェースでは、操作前にユーザーの確認を求めるManualモードが既定となっており、Autoモードでは自動安全性チェックを経てツールが実行されます。[出典：統合後の確認設定](https://support.claude.com/en/articles/16761823-claude-cowork-and-chat-are-one-claude)

**eneへの反映案**：[作業の依頼](features/task/request.md)において、雑談から実務へとスムーズに移行できる対話入口を確保しつつ、何を生成し、どのリソースを変更するかを明瞭に提示します。フォルダや外部ツールを利用し始める際には、読み取り専用か編集権限を含むか、どのデータがモデルへ送信されるかを事前に明示します。[権限とデータの管理](features/settings/permissions.md)に則り、合意された範囲内の通常作業は自律的に進行させ、操作ごとの過度な承認確認を排して軽快なユーザー体験を担保します。

## ChatGPT Work

[ChatGPT Work](https://learn.chatgpt.com/docs/get-started-with-work) は、調査、データ分析、資料作成などを、ユーザーが確認・修正可能な具体的な成果物へと落とし込むための機能群です。タスクを委任した後にユーザーがどのような画面で進行状況を把握し、どの段階でフィードバックを与えるかというUX設計において大いに参考になります。

### 要件定義から成果物の確認・調整に至るUX

公式ガイドラインでは、目的、参照すべき資料やツール、制約事項、完了基準、途中でユーザー確認を挟むチェックポイントを明確に伝えることが推奨されています。資料に基づくスライド生成、競合他社の比較表作成、ドキュメントの定期更新といった実務ユースケースが想定されており、作業中の進捗可視化、追加情報の対話的な投入、軌道修正、重要操作の承認フローが整備されています。[出典：Get started with ChatGPT Work](https://learn.chatgpt.com/docs/get-started-with-work)

出力される成果物は、リッチテキストドキュメント、スライド、スプレッドシート、PDFなど多岐にわたり、画面上で直接プレビューを確認しながら、同一チャット内で修正を指示できます。エンジニア向けのCodexがコード差分や低レイヤーの実行ログを前面に出すのに対し、デスクトップ版のWorkはビジネスパーソン向けに自然言語の解説と完成物のビジュアルプレビューを中心に据えています。[出典：Use ChatGPT](https://learn.chatgpt.com/docs/use-chatgpt)

### 手順（スキル）と外部接続（MCP）のパッケージ化

プラグイン構成として、特定の業務手順や参照ドキュメントをまとめた「スキル」と、外部サービスのデータ取得やAPI操作を担う「MCPサーバー」を統合して利用できます。MCP（Model Context Protocol）は、AIモデルと外部ツールを標準化されたプロトコルで接続するための共通仕様です。[出典：Plugins](https://learn.chatgpt.com/docs/plugins)

この構造の優れた点は、「何ができるか」というAPIツールの定義と、「どう進めるか」というノウハウや思考フレームワークを一体として扱える点にあります。eneにおいても、単に検索APIを繋ぐだけでなく、情報の信頼性確認手順や比較表のまとめ方まで含めた拡張を提供することで、ユーザーが毎回細かなプロンプトを記述する負担を大幅に軽減できます。

### クラウドとローカルにおけるタスク継続性の差異

Webブラウザ版は管理されたクラウド環境で実行されるためブラウザを閉じてもタスクが継続する一方、デスクトップ版ではローカルリソースを扱うタスクとクラウドタスクとで動作条件が異なります。[出典：実行環境の選択](https://learn.chatgpt.com/docs/use-chatgpt)

**eneへの反映案**：長時間のタスク実行においては、「現在何を調べているか」「何が完成したか」「何を確認してほしいか」をコンパニオンが簡潔に要約して対話画面へ提示し、詳しい説明や追加の指示へと自然につなげます。最終報告は各クライアントから閲覧でき、作成・編集したファイルの保存先とは区別して扱います。詳細な実行ログはバックグラウンドの履歴に残しつつ、表層ではキャラクターとの自然なやりとりを維持します。[調査の依頼](features/task/research.md)では、情報源の出典リンク、比較軸、未確認事項を調査結果に明記することを完了基準に組み込みます。

## nanobot

[nanobot](https://github.com/HKUDS/nanobot) は、Pythonで実装された、個人のPCやサーバー上で自律運用可能なオープンソースの軽量AIエージェントフレームワークです。WebUI、ターミナル、チャットツールから利用でき、ファイル操作、Web検索、MCP、長期記憶、定期実行をコンパクトに組み合わせています。

### 対話の受付と推論・ツール実行サイクルの分離

アーキテクチャ設計として、各チャネルから届いた入力を MessageBus へ流し、`AgentLoop` がセッション、ワークスペース、コンテキスト、イベント通知を統括します。そして `AgentRunner` がモデル呼び出しとツール実行のイテレーションを反復し、最終結果の生成または反復上限に達するまで処理を継続します。[出典：Architecture](https://github.com/HKUDS/nanobot/blob/66f5f2df15455b34fc22e656bdc1ef3d4f32e328/docs/architecture.md)

この明確な責務分離は、キャラクターのフロントエンド描画とバックエンドのエージェント実行処理を独立させたいeneのアーキテクチャに合致しています。キャラクターのアニメーション描画や音声再生の処理待ちが、バックグラウンドでのタスク進行やログ永続化をブロックしない設計を実現できます。

### 検索プロバイダとページ取得プロバイダの分離

nanobotでは、検索クエリを発行してURL一覧を取得する処理と、該当URLの本文コンテンツを取得する処理が別々のツールとして分離されています。本文取得には既定で Jina Reader が使用され、その場合は対象URLが外部のReader APIへ送信されます（設定によりローカルでのHTMLパーサー変換への切り替えも可能）。また、プライベートIPやループバックアドレスへの不正アクセスを抑止するSSRF保護機能も備えています。[出典：Configure Web Search](https://github.com/HKUDS/nanobot/blob/66f5f2df15455b34fc22e656bdc1ef3d4f32e328/docs/guides/configure-web-search.md)

この仕組みは、検索APIキーを設定しただけではWeb取得全体のデータ送信経路が定まらないことを示しており、セキュリティとプライバシー設計の観点で非常に重要です。

### MCPの接続・認証・ツール公開制御

MCPサーバーの登録において、ローカルプロセスと標準入出力（stdio）で通信する方式と、HTTP/SSEで通信する方式の両方に対応しています。OAuth認証をサポートし、`enabledTools` 設定によってモデルへ露出させるツールをきめ細かくフィルタリングできます。[出典：Configure MCP Tools](https://github.com/HKUDS/nanobot/blob/66f5f2df15455b34fc22e656bdc1ef3d4f32e328/docs/guides/configure-mcp-tools.md)

ただし、モデルに露出するツール名を絞ることと、ローカルで起動されたMCPプロセス自体のOS権限をサンドボックス化することは別問題です。

**eneへの反映案**：[Web検索](features/tools/search.md)の設定において検索エンジンと本文抽出サービスの送信先を明確に区別してユーザーに提示します。[機能の拡張](features/extensions/README.md)においては、「未接続」「認証待ち」「利用可能」といったステータスを可視化し、日常利用ではツールごとの細かな許可を求めず、導入時にまとめて権限を付与できる設計とします。

## OpenClaw

[OpenClaw](https://github.com/openclaw/openclaw) は、複数のチャットインターフェースや端末から利用可能な、セルフホスト型のAIアシスタント基盤です。常駐する Gateway デーモンが、対話チャネル、セッション、ツール群、イベントディスパッチを一元管理します。[出典：Gateway architecture](https://docs.openclaw.ai/concepts/architecture)

### 推論モデルと実行基盤（ランタイム）の階層分離

OpenClawの設計では、以下のレイヤーが明確に定義されています。[出典：Agent runtimes](https://docs.openclaw.ai/concepts/agent-runtimes)

| レイヤー | 担当責務 | eneにおける対応領域 |
| --- | --- | --- |
| チャネル | 入出力インターフェース（メッセージの送受信） | 各クライアントアプリ（デスクトップ、Web、スマホ） |
| プロバイダ | 認証情報の管理、接続先API、モデル一覧の取得 | APIキー管理、ローカル推論サーバー連携 |
| モデル | 応答や判断を生成するLLMインスタンス | 日常会話用モデル、タスク推論用モデル |
| ランタイム／ハーネス | プロンプト構築、ツール呼び出しループの実行基盤 | ene内蔵エージェントエンジン、外部ハーネス連携 |

組み込みランタイムだけでなく、外部プラグインによるハーネスやローカルCLIランタイムなど、多様な実行基盤を差し替え可能です。ランタイムごとにコンテキスト管理方針やツール実行制御が異なるため、単純にモデルを差し替えることと実行エンジンを差し替えることは明確に区別されます。

### 認証情報の保存とモデル疎通確認の分離

管理画面において、APIキーの保存と利用可能モデル一覧の取得、および実際の疎通テストが段階的に分離されています。キーの保存に成功しても、API側のクォータ制限やネットワーク不通などの要因を切り分けてエラー表示します。[出典：Control UI and API keys](https://docs.openclaw.ai/concepts/model-providers/control-ui-and-keys)

この設計は、「認証設定を保存できたこと」と「実際にモデルが正常稼働すること」を混同しない点で優れています。eneでも接続検証ステップを明確に設けることで、エラー発生時のリカバリーを容易にします。

### 長期記憶における要約と階層化

OpenClawの記憶システムでは、日々の会話ログを蓄積する記録層と、恒久的に役立つ事実・決定事項を整理・抽出した `MEMORY.md` とが明確に分離されています。日々の履歴は必要に応じてベクトル検索等で参照され、バックグラウンド処理によって重要な要点が長期記憶へと昇華されます。[出典：Memory overview](https://docs.openclaw.ai/concepts/memory)

**eneへの反映案**：コンパニオンの人格・対話層と、実務を担う実行ハーネスを疎結合に分離します。[機能の拡張](features/extensions/README.md)では、開始、中断、進捗報告、実行結果の報告といった標準インターフェースを定義し、エンジンが差し替えられても一貫した操作性を保ちます。また記憶機能においても、日々の細かな会話ログと長期的に保持すべき知識・思い出を分離し、ユーザーが手動でテキストファイルを編集することなく、対話を通じて自然に整理・訂正できる体験を構築します。

## OpenCode

[OpenCode](https://opencode.ai/docs/agents/) は、複数の特化型エージェントとLLMモデルを協調させながら開発作業を進めるツールです。用途に応じたモデルの使い分けや、きめ細かなアクセス権限管理、ヘッドレスサーバーアーキテクチャが参考になります。

### 専門役割ごとのモデル選定と権限の局所化

ユーザーと対話するプライマリエージェントと、コード実装や設計分析を担当するサブエージェントが分離されています。エージェントごとにプロンプト、適用モデル、実行権限、反復回数上限を個別に設定可能であり、モデル未指定のサブエージェントは呼び出し元の設定を継承します。[出典：Agents](https://opencode.ai/docs/agents/)

### 3段階の権限モデルと有効スコープ

権限設定として `allow`（即時実行）、`ask`（都度確認）、`deny`（拒否）の3段階が定義されています。ファイルパス、シェルコマンド、ツール種別のパターンマッチングによってルールが評価されます。[出典：Permissions](https://opencode.ai/docs/permissions/)

確認ダイアログにおける `always` は「現在のセッション中に限り同一パターンの要求を自動承認する」という一時的なスコープであり、設定ファイル上の永続的な `allow` とは区別されます。

### ヘッドレスHTTPサーバーによるUIの分離

`opencode serve` コマンドにより画面を持たないHTTPサーバーが起動し、OpenAPI仕様およびSDKを通じて外部の任意のUIやプログラムからエージェント機能を利用できます。[出典：Server](https://opencode.ai/docs/server/)

**eneへの反映案**：キャラクターとの日常対話と、高度な調査・作業を担当するエージェントとで異なるモデルを割り当てられる構成を採用します。ただし、一般ユーザー向けのeneにおいては、操作ごとの複雑な3段階パーミッションエディタを前面に出すことは避け、[権限とデータの管理](features/settings/permissions.md)に定めたとおり、機能単位での包括的な許可と、重要判断時の対話相談というシンプルな体験に昇華させます。

## Hermes Agent

[Hermes Agent](https://hermes-agent.nousresearch.com/docs/) は、Nous Researchが開発するオープンソースの自律型エージェントです。長期記憶、再利用可能なスキル体系、cron形式による定期実行機能を備えています。

### 事実としての記憶と再利用可能な手順（スキル）の分離

標準の記憶管理として、環境情報や学習した客観的事実を記録する `MEMORY.md` と、ユーザーの個人的な好みや要望を記録する `USER.md` が分離されています。さらに、作業の手順やノウハウは「スキル」として独立して管理され、エージェント自身が新しいスキルを作成・改善していく自己進化型の構造を持ちます。[出典：Persistent Memory](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory)、[Skills System](https://hermes-agent.nousresearch.com/docs/user-guide/features/skills)

ここでいう「学習」とは、LLMモデル自体の重み（ウェイト）を書き換えることではなく、蓄積されたプロンプトや手順ファイルを動的にコンテキストへ読み込むインコンテキスト学習を指します。

### バックグラウンド定期実行の独立セッション設計

単発・定期のスケジュールタスクは、Gatewayデーモンによってバックグラウンドで監視・トリガーされます。各回の実行は独立した新しいエージェントセッションとして起動され、指定された手順や文脈のみが入力として渡されます。そのため、メインの会話履歴が暗黙のうちにすべて持ち越されるわけではありません。[出典：Scheduled Tasks](https://hermes-agent.nousresearch.com/docs/user-guide/features/cron/)

**eneへの反映案**：[スケジュール実行](features/autonomy/schedule.md)において、タスクの内容、タイミング、報告方法を対話で取り決め、コンパニオンがスケジュールを登録します。不在時に重要な判断が生じた場合は該当箇所のみを保留し、再接続時にまとめて報告します。「タスクの予約解除」と「実行中タスクの強制停止」を明確に区別し、失敗時のリカバリーを容易にします。また、客観的な事実の記憶と作業手順（スキル）の蓄積を分離するアプローチをeneの記憶基盤にも採用します。

## Replika

[Replika](https://help.replika.com/hc/en-us/articles/37208679176077-How-does-Replika-s-memory-work) の公式ヘルプによると、ユーザーとの自然な会話の中から自動的に記憶ファクトが生成され、手動での追加・編集も可能です。管理画面に表示される明示的な記憶ファクトの裏で、対話傾向からユーザーの性格や嗜好を捉える暗黙的な層が存在します。

**eneでの採否**：自然な対話を通じて相手への理解が深まる体験設計を採用します。一方で、内部状態がブラックボックス化し、一部の情報しか確認できない管理方式は採用しません。eneでは、何を記憶し、どのような根拠に基づいているのかをユーザーが常に把握・訂正・忘却できる透明性を重視します。

## Kindroid

[Kindroid](https://kindroid.ai/v2/docs/ember-llm-guide/) の公式ガイドでは、キャラクターの核となる人格設定（Backstory）には世界観や口調の基本のみを記述し、過去の出来事やエピソードはキーワード検索可能な日誌形式（Journal Entries）へ分離することが推奨されています。

**eneでの採否**：初期プロファイルとしての「心」の設定と、交流の中で蓄積される「思い出」を厳格に分離する構造を採用します。ただし、キーワードやプロンプト文の手動調整を一般ユーザーに強いる運用は採用せず、対話を通じてシステムが自動で抽出し、必要に応じて対話の中で訂正できる仕組みを基本とします。

## Claudeの記憶

[Claude](https://support.claude.com/en/articles/11817273-use-claude-s-chat-search-and-memory-to-build-on-previous-context) では、過去の会話履歴検索、プロジェクトごとの独立したメモリ、記憶の編集・削除・一時停止がサポートされています。ChatとCoworkの間で、会話で得た知識を作業に活かし、作業の文脈を対話へ引き継ぐ双方向の継続性が確立されています。

**eneでの採否**：記憶の根拠へのトレーサビリティ、作業文脈の適切な分離、対話と実務の連続性を採用します。eneではコンパニオンごとに記憶を個別に保持し、共有の要否、内容、相手は、会話の文脈、情報の性質、ユーザーの意向を踏まえてコンパニオンが適切に判断します。共有のたびにユーザーの明示的な指示を求める運用は前提としません。

## Letta

[Letta](https://docs.letta.com/v1-sdk/memory/memory-blocks) の Memory Blocks は、LLMのコンテキストに注入するメモリ空間を構造化ブロックとして分割管理し、アタッチ／デタッチや読み取り専用属性の付与を可能にしています。

**eneでの採否**：記憶の種別とアクセス権限を分離し、実務を担当するサブエージェントにはそのタスクに必要な情報ブロックのみを限定して渡す考え方を採用します。すべてのサブエージェントに個人的な記憶やキャラクター設定の書き換え権限を無制限に与える構成は採用しません。

## 記憶機能の比較と採否

調査日：2026年9月30日。各プロダクトの公式仕様から確認できる特性と、eneへの適合性の判断を整理しました。ここでの「採用しない」という判断は、製品自体の優劣を示すものではなく、eneの設計思想やターゲットユーザーに適さない運用形態を避けるための設計方針です。

| 参考プロダクト | 確認された主要な特徴 | eneにおいて採用する設計 | 採用を見送る設計とその理由 |
| --- | --- | --- | --- |
| [Replika](#replika) | 対話からの自動記憶形成と手動登録、不可視な傾向学習層 | 日常の自然なやりとりから理解が深まる体験 | 内部状態のブラックボックス化。ユーザーによる訂正・忘却が困難になるため |
| [Kindroid](#kindroid) | 人格の基本定義と過去の出来事記録の明確な分離 | 初期プロファイルと思い出の分離構造 | キーワード調整やプロンプト直接編集を前提とした複雑な日常運用 |
| [Claude](#claudeの記憶) | プロジェクト別メモリ、管理UI、対話と実務の文脈連続性 | 記憶根拠の透明性、タスクごとの文脈分離、対話と実務の連携 | 複数キャラクター間での無条件な記憶共有（プライバシー境界の保護のため） |
| [Hermes Agent](#hermes-agent) | ユーザー情報・環境事実・作業手順（スキル）の階層管理 | 好み・事実・作業手順の明確な分類 | 記憶訂正の反映にセッション再起動を要求する運用（即時反映を目指すため） |
| [OpenClaw](#openclaw) | 日常のログ蓄積と長期的な要点ファイルの分離 | ログと長期記憶の分離、必要に応じた検索 | ユーザー自身にMarkdown編集や容量管理を行わせる運用負荷 |
| [Letta](#letta) | メモリブロックの構造化、アタッチ制御、権限設定 | 必要な情報ブロックのみをサブエージェントへ渡す最小権限の原則 | 全サブエージェントに個人的記憶や人格の改変を無制限に許す構成 |

これらを踏まえた[eneの記憶機能](features/memory/README.md)は、コンパニオンごとの自動記憶形成と、対話を通じた直感的な確認・訂正・忘却を基本とします。共有の要否、内容、相手は文脈やユーザーの意向に応じて自律的に判断し、古い記憶や忘れるよう求められた記憶は、想起優先度を下げて思い出しにくくします。訂正の即時反映、端末やモデルを切り替えても失われない永続性、配布パッケージからの個人的記憶の完全除外は、eneが堅持する中核仕様です。

## Shizuku-Duplex

Shizuku-Duplex は、日本語に対応したフル二重（全二重）音声対話モデルです。従来のターン制（ASR ➔ LLM ➔ TTS）ではなく、80ms tick単位で音声を双方向にストリーミング処理し、自然な相槌や発話中の割り込み（バージイン）を実現します。また、即時応答を司るInteraction Modelと非同期でツール実行や思考を担うBackground Modelを分離するアーキテクチャを持ちます。eneにおける音声対話の低遅延化、バージイン挙動、および「頭」と「手」の役割分離の重要な参考とします。

## Neuro-sama

Neuro-sama は、Vedal氏が開発する自律型AITuberです。定期Tickループと優先度キューによる入力調停、無言（Idle）時間の検知に基づく自発的な独り言生成、ゲーム状態や外部イベントの非同期センシングを備えています。eneにおける自発性ループおよび「目」からの知覚情報に基づく行動選択（Speak / Work / NOOP）の設計の参考とします。

## AITuberKit

[AITuberKit](https://github.com/tegnike/aituber-kit) は、AIキャラクターやバーチャル配信者を構築するためのオープンソースツールキットです。カメラ等のセンサーによる知覚、コメントストリームの処理、音声再生終了イベントに連動した独り言ハンドラを持ちます。eneの見守り知覚および自発的発話トリガーの参考とします。

## Generative Agents

[Generative Agents](https://arxiv.org/abs/2304.03442)（Park et al., スタンフォード大学 / Google）は、自律AIキャラクターの記憶と行動シミュレーションに関する研究です。記憶ストリームから想起する際、LLMが評価した重要度（Importance: 1〜10）、経過時間による指数減衰（Recency）、ベクトル類似度（Relevance）を合算する数理モデルを提案しました。eneにおけるエピソード記憶の想起および自然な忘却メカニズムの基礎数理として採用します。

## Mem0

[Mem0](https://mem0.ai) は、パーソナライズAIコンパニオン・エージェント向けの記憶管理基盤です。会話から事実や嗜好をLLMが自律抽出し、競合解決（ADD / UPDATE / DELETE / NOOP）を行うパイプライン、およびアクセス頻度と経過時間に応じたソフト減衰リランキングを備えています。eneにおける記憶抽出・整理のライフサイクルおよびアクセス日時の更新ガードの参考とします。

## deepseek-harness

deepseek-harness は、高度にモジュール化されたエージェント実行ハーネスです。「Everything is a plugin」の設計思想に基づき、ツールや推論エンジンだけでなく、各種コンポーネントをプラグインとして追加・差し替え・フックできる柔軟性を備えています。eneにおける将来的な全方位の拡張機能アーキテクチャの指針とします。

## 今後先行して検証すべき論点

キャラクターとしての情緒豊かな存在感と、実務エージェントとしての確実な遂行能力を両立させるためには、両者の連携インターフェースを精緻に検証する必要があります。以下のユースケースを通じて、実機試作とUX検証を先行して進めます。

| 検証シナリオ | 検証すべき中核課題 | 主な着目参考元 |
| --- | --- | --- |
| 会話の流れから調査作業を依頼する | 作業中もキャラクターの自然な反応が維持され、進捗や最終報告の確認、詳しい説明の依頼を同一窓口で行えるか | AIRI、Claude Cowork、ChatGPT Work |
| キャラクターを連続して切り替える | 設定プレビューと常駐画面の間で競合が発生せず、GPUメモリやリソースが安全に解放されるか | AIRI、Desktop Mate |
| モデルの接続や外部認証でエラーが発生する | 障害箇所と復旧手順が明快に示され、未許可の外部サービスへ勝手にフォールバックしないか | nanobot、OpenClaw |
| サブエージェントがファイル操作を行う | 許可された範囲内では余計な確認を挟まず自律進行し、権限外の操作は適切にコンパニオンが相談できるか | OpenClaw、OpenCode |
| クライアント端末がオフライン中にスケジュール実行される | ホスト上でタスクが継続し、端末依存タスクが安全に保留され、再接続時に正しく通知されるか | Hermes Agent |

## 参照コミット・資料一覧

本ドキュメントにおけるGitHubリポジトリの参照情報は、以下のコミットハッシュに固定して調査を行いました。商用製品やクラウドサービスについては、調査日時点（2026年9月30日）の公式ドキュメントおよび利用規約に基づいています。

| リポジトリ | 参照コミットハッシュ | 確認した一次資料 |
| --- | --- | --- |
| AIRI | [b40e3e87b149](https://github.com/moeru-ai/airi/commit/b40e3e87b149ea5fb75d4944440493829e601411) | README、Architecture構成図、Roadmap、DevLog |
| nanobot | [66f5f2df1545](https://github.com/HKUDS/nanobot/commit/66f5f2df15455b34fc22e656bdc1ef3d4f32e328) | README、Architecture、Web Search設定ガイド、MCP設定ガイド |
| OpenClaw | [2de50896b3e3](https://github.com/openclaw/openclaw/commit/2de50896b3e32abbd9bdc26aea1a62a6676d3bc0) | README、Gateway Architecture、Agent Runtimes、Memory Overview |

<a id="mvp-technology"></a>

## MVPの技術選定と一次資料

調査日：2026年10月1日。[MVPアーキテクチャ](architecture.md)のために、公式ドキュメント・公開ソースで確認できる機能と、eneで採用する設計判断を分けて整理します。以下の「採用」は技術選定を意味し、eneでの動作確認や性能測定の完了を意味しません。以前の製品調査と参照コミットは維持します。

<a id="mvp-products"></a>

### 既存製品から採用する構造

| 参考元 | 確認した資料・構造 | MVPで採用する考え方 | そのまま導入しないもの |
| --- | --- | --- | --- |
| [AIRI](#airi) | [固定コミットの構成図](https://github.com/moeru-ai/airi/blob/b40e3e87b149ea5fb75d4944440493829e601411/README.md#sub-projects-born-from-this-project)、[VRMのライフサイクル改善](https://airi.moeru.ai/docs/en/blog/DevLog-2026.03.14/) | 対話・音声・描画の分離、モデルのロード・解放と表示状態の検証 | Web・デスクトップの技術構成そのもの。eneではSlintとネイティブ描画を使う |
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
| Slint | Windows・macOS・Linux。独自コンポーネントと複数の描画バックエンド | 選択したネイティブバックエンド・GPU等のOS依存 | Royalty-free／GPLv3／商用 | 未計測 | 会話・設定UIに採用。宣言的な外観とRust連携を使い、バックエンドを絞る |
| Godot | Windows・macOS・Linux。2D・3D・UIをエンジンで描画 | エクスポートしたエンジン、VRM連携用の追加部品 | MIT。追加部品は別途確認 | 未計測 | 不採用。今回のRust構成ではBevyのVRM連携を選ぶ。Wayland常駐表示を未検証のまま対応済みとしない |
| Bevy | Windows・macOS・Linux。Rustの描画・アニメーション基盤 | 有効化したエンジン機能、GPU・ウィンドウ関連の依存 | MIT／Apache-2.0 | 未計測 | キャラクター描画に採用。会話・設定UIまでBevyで構築せず、機能を絞る |

一次資料：[Electronのプロセスモデル](https://www.electronjs.org/docs/latest/tutorial/process-model)、[Electronのライセンス](https://github.com/electron/electron/blob/main/LICENSE)、[Tauriの概要](https://v2.tauri.app/start/)、[Tauriのライセンス](https://github.com/tauri-apps/tauri#license)、[Qtの対応OS](https://doc.qt.io/qt-6/supported-platforms.html)、[Qtの配布](https://doc.qt.io/qt-6/deployment.html)、[Qtのライセンス](https://www.qt.io/development/qt-framework/qt-licensing)、[CXX-Qt](https://github.com/KDAB/cxx-qt)、[SlintのデスクトップUI](https://slint.dev/desktop)、[Slintのバックエンドと描画](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/)、[Slintのライセンス](https://github.com/slint-ui/slint/blob/master/LICENSE.md)、[Godotの機能](https://docs.godotengine.org/en/stable/about/list_of_features.html)、[Godotのライセンス](https://godotengine.org/license/)、[Bevyの公式リポジトリ](https://github.com/bevyengine/bevy)。

この比較は性能順位を示すものではありません。常駐時の消費資源はウィンドウ数、モデル、描画頻度、依存機能によって変わるため、採用構成をリリースビルドで測定します。Pythonは利用者の環境とは独立して配布することも可能ですが、ene本体ではランタイムと依存管理を追加せずRustを選びます。言語だけでファイル操作の隔離が成立するとは判断しません。

SlintはLinuxでQtがインストールされている場合、既定構成でQtバックエンドが選ばれ得ます。eneでは既定機能を無効にし、WinitとFemtoVG/WGPUを明示します。Rust APIの機能名ではLinux用に`backend-winit-wayland`、描画に`renderer-femtovg-wgpu`を選定対象とします。UIはビルド時にRustへコンパイルし、任意のUIコードを実行時に読み込む仕組みは採用しません。[出典：Slint Rust API 1.18.1](https://docs.rs/slint/1.18.1/slint/)、[Winitバックエンド](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/)

SlintのRoyalty-free LicenseにはSlintの利用表示が必要です。eneはこの方式を採り、アプリ内に表示を設ける方針です。テーマ・配色・配置のカスタマイズと、任意コードのスキン実行は別に扱います。[出典：Slintのライセンス選択](https://github.com/slint-ui/slint/blob/master/LICENSE.md)

<a id="mvp-runtime"></a>

### Rust実行基盤とVRM描画

現在の実装は[ene-vrm](../crates/ene-vrm/README.md)を使い、glTF共通処理をBevy標準へ委譲します。以下のbevy_vrm1の記述は初期選定時の参考資料です。

[Tokio](https://tokio.rs/tokio/tutorial)は非同期処理、[Axum](https://docs.rs/axum/latest/axum/)はHTTPとWebSocketのホストAPIに用います。[Rig](https://rig.rs/docs/)はAIプロバイダー接続とストリーミングに利用します。[OpenRouterプロバイダーのAPI資料](https://docs.rs/rig/0.43.0/rig/providers/openrouter/index.html)には接続とルーティング設定の受け渡しがあり、モデル接続をeneのタスク実行制御から分離する材料になります。権限、停止、バックアップ、ファイル変更の成否確認はeneの責務として残します。

[bevy_vrm1の固定コミットのREADME](https://github.com/not-elm/bevy_vrm1/blob/c8601e2854bd9c6cce0f6d282c7d90a65a895fb7/README.md)はVRM 1.0のみを対象とし、Spring Bone、Look At、VRMAなどの対応を示しています。同時に開発初期で破壊的変更があり得ることを明記しています。[同コミットのCargo.toml](https://github.com/not-elm/bevy_vrm1/blob/c8601e2854bd9c6cce0f6d282c7d90a65a895fb7/Cargo.toml)はパッケージ0.9.5、Bevy 0.19系、MIT／Apache-2.0を示し、[MToonの実装](https://github.com/not-elm/bevy_vrm1/blob/c8601e2854bd9c6cce0f6d282c7d90a65a895fb7/src/vrm/mtoon/material.rs)も参照できます。これは確認したソースの組み合わせであり、eneでのモデル互換性や公開パッケージの採用バージョンを検証した記録ではありません。

eneは既存のVRM描画を利用し、独自のVRMレンダラーを一から作ることを避けます。基本的な待機モーション、透明な描画、モデル変更・終了時の資源解放を先行試作で確認します。Bevyの音声・UI・ゲームパッドなど不要な機能は無効にします。

<a id="mvp-wayland"></a>

### Waylandと描画の連携

[Smithay Client Toolkitのwlr layer-shell対応](https://smithay.github.io/client-toolkit/smithay_client_toolkit/shell/wlr_layer/index.html)と、[layer-shellの公式プロトコル定義](https://github.com/swaywm/wlr-protocols/blob/master/unstable/wlr-layer-shell-unstable-v1.xml)は、出力、レイヤー、アンカー、余白、作業領域の占有、キーボード入力の設定を提供します。透明部分のクリック透過はウィンドウ全体の透過設定ではなく、[Waylandのsurface入力領域](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_surface)を制御する必要があります。

[Bevy 0.19.0のウィンドウハンドル実装](https://github.com/bevyengine/bevy/blob/v0.19.0/crates/bevy_window/src/raw_handle.rs)には、ネイティブハンドルとウィンドウの寿命を保持して描画側へ渡す仕組みがあります。これは独自のWaylandウィンドウを接続する設計の根拠ですが、Smithayのlayer-shellウィンドウとBevyの連携が完成済みであることを示しません。OSごとのスレッド制約とウィンドウの寿命を守る接続、透明描画、入力領域、ドラッグ、DPI・出力変更の実機検証を必要とします。

eneでは通常のSlint画面とキャラクターの常駐ウィンドウを分離し、最初の検証環境をKDE/KWinとします。layer-shellを提供しない環境への対応や、Windows・macOSの常駐APIは別のOSアダプターで扱い、クロスプラットフォームのライブラリを選んだだけで常駐機能も対応済みとはしません。

<a id="mvp-host"></a>

### ホストと接続の設計

[OpenCodeのServer](https://opencode.ai/docs/server/)はヘッドレスサーバーと外部UIの分離、[OpenClawのGateway architecture](https://docs.openclaw.ai/concepts/architecture)は常駐ホストと接続・状態管理を検討する参考です。eneでは同一PCの認証付きループバックHTTPとWebSocketを採用し、永続状態の取得と通知を分離します。各製品の通信プロトコルやSDKをそのまま採用するものではありません。

[systemdのユーザーサービス](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)をLinuxでのホスト運用に使い、GUIの終了からホストの寿命を分離します。再接続では保存された結果と現在の状態を取得し、要求IDで開始要求の重複を防ぎます。ホスト停止後のタスク自動再開はMVPに含めません。

[AxumのWebSocket資料](https://docs.rs/axum/latest/axum/extract/ws/index.html)はHTTPからの接続確立と送受信の並行処理を示しています。[Tokioのbroadcast資料](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html#lagging)は有限バッファで遅い受信側が通知を失い、`RecvError::Lagged`で検出できることを説明しています。eneでは通知と音声の接続・待ち行列を分け、通知欠落時は購読とスナップショットから再同期します。これはeneの通信契約であり、ライブラリが状態同期や停止制御まで自動で提供するという意味ではありません。具体的な設計は[ホストAPI設計案](api.md)にまとめます。

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

OSの隔離と、拡張子・文字コード・相対パス・競合・停止状態などの操作検証は併用します。LLMへの指示やツール名の制限だけをアクセス制限として扱いません。許可と停止は実行側が管理し、適用済み変更の報告と復旧用コピーを残すことを[MVPの権限方針](features/settings/permissions.md)に合わせます。

<a id="mvp-storage"></a>

### SQLiteと記憶検索

[SQLiteの公式用途説明](https://www.sqlite.org/whentouse.html)は、個々のアプリやデバイスのローカル保存を適した用途として挙げています。[WALの公式仕様](https://www.sqlite.org/wal.html)では読み取りと書き込みを並行できる一方、書き込みは同時に1つ、WALを利用するプロセスは同じホスト上である必要があります。eneではホストだけがDBを所有し、専用スレッドで処理を直列化する構成を選びます。クライアント数が増えただけでDBファイルをネットワーク共有することはしません。

[rusqlite](https://docs.rs/rusqlite/latest/rusqlite/)の組み込みSQLite構成を使い、OSごとのSQLiteバージョン差を避けます。SQLiteのトランザクションと作業フォルダのファイル操作は別であるため、変更前コピーと操作記録、実際の変更を照合して復旧する設計を採ります。

[FTS5のtokenizer仕様](https://www.sqlite.org/fts5.html#tokenizers)にはUnicodeの単語単位とtrigramによる部分一致があります。trigramの全文検索では3文字未満の部分文字列は一致しない制約があるため、日本語の短い名前・単語を含めて検索方法を検証します。FTS5を有効にするだけで日本語の意味検索まで完成するとはみなしません。

[sqlite-vecの公式README](https://github.com/asg017/sqlite-vec)には、SQLite内でのベクトル保存・検索、Rust向けの組み込み方法、Linux・Windows・macOSなどでの利用が示されています。ただし1.0未満で破壊的変更が想定されます。将来の評価候補とし、今回のMVPには導入しません。

eneの設計判断は、記憶本体と出典・所有者・共有範囲・訂正・優先度をSQLiteの正本に持ち、キーワード・ベクトル索引を再構築可能な検索手段として分離することです。複数ホストの大量同時書き込みや検索速度・メモリの実測で必要になれば、保存先または検索エンジンを再評価します。[長期記憶の機能仕様](features/memory/README.md)は維持し、MVPでは会話履歴とタスク結果の永続化までを実装します。
