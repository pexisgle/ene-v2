# 全プロバイダーのプラグイン化：設計提案

状態：提案（2026-11-15）。実装前のレビュー対象。[プラグイン設計](plugins.md)、[技術スタック](tech-stack.md)、[音声](voice-avatar.md)、[心と自動記憶](mind.md)を参照する。

---

## ABI 8 の LLM 生成契約（比較ブランチで実装）

この節は比較ブランチで実装した現在の契約であり、以下の 0 章以降の提案は
その時点の履歴である。ホストが受け入れる LLM の生成形式は、この 1 つだけである。

### 形式

- ホストが受け入れる生成形式は OpenAI Chat Completions 互換の HTTP だけである。
- 生成は `Operation::GenerateHttp { request: HttpRequest }` だけである。
- ホストは Rig の OpenAI Chat Completions wire でリクエストを組み立て、
  プロセス内クライアントから Extism へ渡す。ゲストは本文を解釈せず、
  そのままブローカーへ転送する。
- ゲストは応答を `Event::Http { chunk: HttpChunk }` として順に返す。最初の
  チャンクが status と headers を持ち、最後の `done` が終端である。
- SSE の復号、ツール呼び出しの組み立て、usage、finish reason はホストの
  Rig wire が担う。ゲストは SSE を解析しない。
- `ListModels` と `Models` は従来のままである。モデル一覧は OpenAI 互換の
  JSON 応答を読む。

### ハーネス（ABI 8 の複数ターン契約）

- `HarnessInput::Start { prompt, cwd }` がセッションを作り、最初のターンを
  始める。`cwd` は ACP `session/new` の cwd に相当し、ホストが自分の作業
  ディレクトリを渡す。
- `HarnessInput::Prompt { prompt }` が生存中のセッションへの追加ターン
  （ACP `session/prompt`）である。セッションは複数ターンを抱える。v7 の
  「セッションにつきプロンプト 1 回」の契約は廃止された。
- `RequestPermission` で中断したターンは `Respond` で再開し、停止中のターン
  には `Cancel` を 1 回の独立呼び出しとして届ける。実行中の呼び出しは
  ホストが中断し、`Cancel` はゲストに転送されない。
- 各呼び出しは `Stop` または `RequestPermission` で終わる。`Cancel` は
  `Stop` で完了する。セッションを閉じたあとに新しく開くセッションには
  前のセッションのゲスト状態を引き継がない。

### 宣言と拒否

- LLM プロバイダーは `ene_initialize` で `openai_http: true` を返さなければ
  ならない。返さない場合、ランタイムは `initialize` で拒否する。
- manifest の `contract_version` は 8 でなければならない。パッケージ読み込み
  時に旧版は拒否される。この検査は entrypoint を選ぶ前に走るため、LLM に
  限らず、embedding、TTS、STT、character、harness を含むすべてのパッケージ
  に及ぶ。同梱の 2 パッケージも 8 である。
- `Generate`、`Delta`、`ToolCall`、`Finished`、`GenerationRequest`、
  `ChatMessage`、`ToolSpec` は ABI 8 に存在しない。旧イベントを返すゲストや
  旧操作を送るゲストは、復号または契約検証で拒否される。
- 複数形式を切り替えるフレームワークは持たない。ホストが受け入れるのは
  OpenAI wire の 1 形式だけであり、旧形式へのフォールバックはない。

### 現在の同梱物

- `examples/plugins/openrouter-wasm`: ABI 8 へ移行済み。生の HTTP 転送、
  `openai_http: true`、manifest 8。
- `examples/plugins/reference`: LLM プロバイダーではないため、この生成契約の
  対象外である。harness 拡張は ABI 8 の複数ターン契約に従う。
- リポジトリ内に他の LLM プロバイダープラグインはない。外部の ABI 6 の LLM
  プラグインは非対応であり、ホストは拒否する。「全 LLM プラグインの形式を
  決めた」のではなく、ホストが受け入れる形式が 1 つに定まった、という意味で
  ある。外部プラグインを移行するかは別途決める。

### 境界

宛先の認可、資格情報の注入、リダイレクト禁止、応答 16 MiB、チャンク 64 KiB、
交換期限、イベントの上限は、従来どおりブローカーとランタイムが持つ。ゲストは
資格情報を見ず、宛先を選ばない。ツール呼び出しの認可はホストの既存ゲートが
担い、プロバイダーは呼び出しを返すだけである。16 MiB と認可ゲートは、ツール
呼び出しの件数・識別子長の代替ではない。

現在のホストでは、プロバイダーが HTTP ブローカー、拡張ゲストがログ・設定・
分離された状態を使う。拡張ゲストからの `ServiceRequest::Tool` は
`permission_required` を返す。公開された拡張ツールは、モデルの通常の
ツール呼び出しとしてホストのゲートを通る。

### ツール呼び出しの制限

復号後のゲート（`permission::gate`）で、1 つのモデル応答あたり 64 件のツール
呼び出しと、ID・名前それぞれ 256 バイトの上限を検査する。旧契約と同じ値・
同じ単位（1 応答あたり）である。応答ごとの呼び出しは effect の親でまとまる
ため、ゲートは復号後の呼び出しを親ごとに数える。SSE の解析は Rig のままで、
独自パーサーは持たない。

名前については、クライアントの capability ツールが登録時に 1-64 文字へ制限
されており（`CapabilitySpec::validate`）、256 バイトより厳しい。ゲートの 256
バイト検査は、それ以外の登録経路（MCP など）に対する二重の防御として残す。

### 取消と再読み込み

呼び出し側がストリームを drop した場合だけ、次の新しい要求の前に、死んだ
インスタンスを落として再読み込みする。明示的な取消の資源解放と、次の要求への
復帰は別であり、取消された元の要求は再送しない。trap、予算超過、ブローカーの
許可違反のように呼び出し自身が `Err` を返した場合は、従来どおり自動では
再読み込みせず、次の要求は失敗する。失敗後にプロバイダーを入れ替えるかは
運用の判断である。ゲートの拒否はモデル応答の後に起きるため、プロバイダーの
インスタンスには影響しない。

Rig の復号が途中で失敗してストリームが drop される場合も、ホストの境界からは
取消と同じ drop に見える。そのため、その場合も次の要求の前の再読み込み経路に
入る。呼び出し自体が `Err` を返した場合とは区別できないため、この境界の
限界として明記する。

再読み込みは同じパッケージ、同じ初期化検査、同じ上限値を通る。時間・サービス・
ストリームの予算とブローカーの 16 MiB、30 秒、チャンク上限は 1 呼び出し（交換）
あたりの保証である。一方、燃料はインスタンス生成時に設定され、呼び出しを
またいで消費される。したがって取消後の再読み込みは、新しいインスタンスに
新しい燃料枠を与える。インスタンスをまたぐ累積の燃料予算は持たないため、
「同じ上限だから累積予算を迂回しない」とは言えない。累積の燃料予算が必要なら、
それは再読み込み方針とは別の変更である。

20 回の取消・回復の反復は、この fixture で観測した範囲の証拠であり、すべての
資源リークがないことの証明ではない。

---

## 0. 目的と前提

LLM・Embedding・TTS・STT の全プロバイダーを `ene-plugin` / `ene-plugin-runtime` 基盤へ移す。プロバイダー追加をホストの再ビルドから切り離し、認証・宛先・上限の制御を1箇所に集約することが目的である。

現状の事実確認（本リポジトリで実測）：

| 事実 | 根拠 |
| --- | --- |
| `ene-host` は `ene-plugin` / `ene-plugin-runtime` を一切リンクしていない | `apps/ene-host/Cargo.toml` に依存行が0件 |
| `Services` トレイトの実装はテスト用の5つのみ。ホスト側の実装は存在しない | `grep "impl Services for"` の結果 |
| `Service::Http` の実処理は未実装。`DenyServices` が全て拒否する | `instance/invocation.rs:23` |
| `Extension` は `Llm` / `Character` / `Harness` の3つ | `manifest.rs:19-23` |
| `Operation` は `ListModels` / `Generate` / `Character` / `Harness` の4つ | `api/wire.rs:15-27` |
| `HttpOpen` は 512 KiB の単発ボディ前提。ストリーミング応答の上限は呼び出し単位 | `limits.rs` |

つまり本提案は「既存の配線を移す」のではなく、**未接続の基盤にプロバイダー契約を足して接続する**作業になる。この差分を過小に見積もると、音声の16MB上限やモデルのストリーミングが移行時に失われる。

---

## 1. `ene-plugin` 契約の拡張

### 1.1 `Extension` に3つを追加する

```rust
pub enum Extension {
    Llm,
    Embedding,
    Tts,
    Stt,
    Character,
    Harness,
}
```

`serde(rename_all = "snake_case")` により manifest の文字列は `embedding` / `tts` / `stt` になる。`contract_version` を 4 から 5 へ上げ、旧 manifest は明示的に拒否する（現行は `deny_unknown_fields` で、未知の `provides` 値は復号時点で落ちる）。

### 1.2 `Operation` に3つを追加する

```rust
pub enum Operation {
    // 既存
    ListModels { endpoint: String, options: Value },
    Generate { request: GenerationRequest },
    Character { target: CharacterTarget, event: CharacterEvent },
    Harness { input: HarnessInput },
    // 追加
    Embed { request: EmbeddingRequest },
    Synthesize { request: SynthesisRequest },
    Transcribe { request: TranscriptionRequest },
}
```

`extension()` の対応は `Embed → Embedding`、`Synthesize → Tts`、`Transcribe → Stt`。

### 1.3 入出力型（`api/audio.rs` と `api/embedding.rs` を新設）

```rust
// api/embedding.rs
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub inputs: Vec<String>,
    /// 要求次元数。0 はプロバイダー既定。
    pub dimensions: usize,
}

// api/audio.rs
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub text: String,
    pub voice: String,
    pub speed: Option<f32>,
    /// 期待する出力形式。現行ホストは PCM 24kHz を要求する。
    pub format: AudioFormat,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    /// 16bit モノラル PCM。
    pub pcm_base64: String,
    pub sample_rate: u32,
    pub language: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// 16bit モノラル PCM。サンプルレートは応答が持つ。
    Pcm16,
    /// RIFF/WAVE。プロバイダーが WAV しか返さない場合に使う。
    Wav,
}
```

応答は `Event` ではなく **`Operation` の戻り値**（`service` の応答）にする。音声は大きく、`Event` の `MAX_CHUNK`（128 KiB）に収まらないためである。戻り値は次の型にする。

```rust
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisResult {
    pub audio_base64: String,
    pub sample_rate: u32,
    pub format: AudioFormat,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingResult {
    /// `inputs` と同じ順・同じ長さ。
    pub vectors: Vec<Vec<f32>>,
}
```

**この設計の帰結。** 音声1回の応答は `MAX_MESSAGE_BYTES`（1 MiB）に収まらない。現行の `MAX_CHUNK` は 128 KiB である。したがって音声は「1回の呼び出しで返す」形にせず、`HttpOpen` / `HttpRead` / `HttpClose` と同じ**ハンドル方式のチャンク読み出し**を `Synthesize` にも用意する必要がある。具体的には `Operation::Synthesize` の戻り値をハンドルにし、`ServiceRequest::AudioRead { handle }` で 128 KiB ずつ引く。この追加は本提案の必須要素であり、省略すると長い発話が必ず失敗する。

### 1.4 `ProviderConfiguration` / `AuthProfile` の流用範囲

両方そのまま流用できる。理由は、どちらもプロトコル非依存だからである。

- `AuthProfile`（`None` / `Bearer` / `Header` / `Query`）は TTS・STT・Embedding の認証方式をすべて表現できる。VOICEVOX は `None`、OpenAI 系は `Bearer`、一部ゲートウェイは `Header` である。
- `ProviderConfiguration` の `destinations` は宛先制限にそのまま使える。`routing`（`Direct` / `OpenRouter`）は LLM 固有なので、音声・埋め込みでは `Direct` 固定で無視する。
- `settings` の `SettingField` は現行 `voice.rs` の `Options`（`voice` / `language` / `speed` / `speaker` / `path` 等）を表現できる。**`path` を含む演算子設定はここへ移す**。これが現行テストの契約（`voice/tests.rs:647`）を保つ道である。

追加が要るのは1点だけである。`ProviderConfiguration` に「この provider が提供する拡張」を持たせる。現状は `EntryPoint.provides` がそれを持つが、1つの provider 設定が複数拡張を提供する場合（OpenAI が LLM と TTS と STT を提供する）に、どの `provides` がどの `settings` に対応するかが曖昧になる。`ProviderConfiguration` に `extensions: Vec<Extension>` を足し、`settings` をその共通部分として扱う。

---

## 2. `Service::Http` 境界への一元化

### 2.1 現状の穴

`Service::Http` は `ServiceRequest::HttpOpen` として定義されているが、`ene-plugin-runtime` に実装がない。`DenyServices` が `permission_required` を返すだけである。つまり「認証情報の注入・宛先制限・リダイレクト禁止・タイムアウト・ボディ上限」は**まだどこにもない**。本提案の中核はここを1箇所に作ることである。

### 2.2 単一の実装点

`crates/ene-plugin-runtime/src/service/http.rs` を新設し、`HttpBroker` を1つだけ置く。

```rust
pub struct HttpBroker {
    /// 接続ごとの認可。ホストが渡す。
    grants: Arc<dyn HttpGrants>,
    /// プロセスで共有する reqwest クライアント。リダイレクト禁止。
    client: reqwest::Client,
    /// 開いているハンドル。上限付き。
    open: Mutex<HashMap<String, Open>>,
}

pub trait HttpGrants: Send + Sync {
    /// この宛先へこの provider が送ってよいか。
    fn allows(&self, provider: &str, url: &Url) -> bool;
    /// 認証ヘッダ。プラグインには値を見せない。
    fn authorize(&self, provider: &str, headers: &mut HeaderMap) -> Result<(), Failure>;
}
```

`HttpOpen` の処理は次の1本になる。

1. `grants.allows(provider, url)` で宛先を検査する。許可は `ProviderConfiguration.destinations` の origin 一致で決める。
2. `grants.authorize` で認証ヘッダを注入する。プラグインが同じヘッダを送ってきたら**拒否**する（上書きさせない）。
3. `client` で送る。`redirect(Policy::none)`、`timeout(既定30秒)`、`content_length` とストリーム読み出しの両方で **16 MiB** を上限にする。
4. 応答は `HttpRead` で 128 KiB ずつ返す。読み出し総量が上限を超えたら `Failure`。

### 2.3 上限値の置き場所

現行ホストの `MAX_BODY_BYTES = 16 MiB`（`voice.rs:48`）を `limits.rs` の `MAX_HTTP_BODY` として移す。`MAX_SERVICE_BYTES`（16 MiB、1呼び出しの合計）とは別物である。前者は1応答、後者は呼び出し全体の累計で、両方要る。

### 2.4 現行の防御を落とさないための対応表

| 現行の防御 | 現在の場所 | 移行先 |
| --- | --- | --- |
| 応答 16 MiB 上限 | `voice.rs:48` + `net::read_body` | `HttpBroker` の読み出し上限 |
| リダイレクト禁止 | `net.rs:17` | `HttpBroker` のクライアント |
| 接続ごとのタイムアウト | `voice.rs:169` | `HttpOpen` の `timeout` |
| ベアラ注入 | `voice.rs:316` `post()` | `HttpGrants::authorize` |
| 宛先の制限 | `ProviderConfiguration.destinations` | `HttpGrants::allows` |
| 認証情報をプラグインに見せない | 現行 `provider.rs` の方針 | 同じ（`authorize` はホスト側で走る） |

この表の全行が `HttpBroker` の1ファイルに来る。これが「一元化」の実体である。

---

## 3. `ene-host` との接続と移行パス

### 3.1 ホスト側の実行境界

ホストは既に4つの境界を持っている。これをそのまま受け口にする。

| 境界 | 現在の実装 | プラグイン版 |
| --- | --- | --- |
| `DynModel<Completion>` | `model::connect` が `catalog.connect_with` で構築 | `DynModel<Completion>` を実装するアダプタ |
| `DynModel<Embedding>` | `mind/embedding.rs::connect` が `ProviderConfig` を match | 同上 |
| `dyn TtsEngine` | `voice.rs` の `HttpEngine` | `PluginTts` |
| `dyn SttEngine` | `voice.rs` の `HttpEngine` | `PluginStt` |

`DynModel<Op>` は `rig_core::driver::DynModel` で、`call(request)` を持つ。プラグインアダプタはこの型を返せばよい。**トレイトを自作する必要はない。** これは監査時に確認した事実で、`DynModel` は `From<Model<W, T>>` を持つが、任意の実装を受け付けるコンストラクタは公開されていない。

ここに制約がある。`DynModel` をプラグインから作るには、`rig_core::wire::Wire` を実装した型を作り、`Model::new(wire, http)` に渡す必要がある。`Wire` は `encode` / `decode` / `describe` を持つ公開トレイトである。したがって**プラグイン LLM アダプタは `Wire` 実装として書く**のが正しい。`DynModel` を直接組み立てる道は公開されていない。

`TtsEngine` / `SttEngine` はホスト固有のトレイトなので、プラグイン実装を足すのは自由である。こちらは素直に `PluginTts` を書けばよい。

### 3.2 標準プロバイダーの扱い

**同梱 Wasm にしない。** 理由は3つある。

1. 起動時間。`MAX_INSTANCES = 16` に対し、LLM・Embedding・TTS・STT で4つ常時消費する。`INITIALIZE_TIMEOUT` は30秒で、起動のたびにコンパイルが走る。
2. デバッグ性。同梱 Wasm のスタックトレースはホストのものより読みにくい。
3. 移行リスク。既に動いている経路を一度 Wasm に通すと、不具合の切り分けが2層になる。

代わりに **`rig-core` のカタログ経路を標準として残し、プラグインは追加プロバイダーの経路にする**。つまり `model::connect` は「カタログに居る provider なら現行経路、居ないならプラグイン」と分岐する。この分岐は1箇所（`model::connect` の冒頭）に閉じる。

### 3.3 移行順序

各段階が独立に検証可能になるよう4段に切る。

**段階1：`HttpBroker` を作る。** プラグインを接続せず、`HttpBroker` 単体のテストを書く。宛先拒否・リダイレクト拒否・16 MiB 上限・認証ヘッダ注入の4つを固定する。この段階ではホストの挙動は変わらない。

**段階2：Embedding を移す。** 4つの中で最も小さい（入出力が文字列とベクトルだけ、ストリーミングなし、音声なし）。`Embedding` 拡張と `Operation::Embed` を追加し、`mind/embedding.rs::connect` を「プラグインがあれば使う、無ければ現行」に変える。既存の `CompactVectorIndex` はそのまま。

**段階2で判明した制約（2026-11-15、実装時に確認）。** 契約側（拡張・操作・ブローカー・実 Wasm 経路のテスト）は入った。ホスト側の接続で2つの壁が出た。

1. **`target()` が同期関数である。** `mind/embedding.rs:166` は同期で、`semantic_ids`（`:236`）、バックフィル（`:553`）、テスト（`:741`）から呼ばれる。プラグインのロードは `Instance::load(...).await` なので非同期であり、同期関数からは呼べない。埋め込みの経路を非同期化するか、プラグインを起動時に先読みして同期で引くレジストリを置くかのどちらかが要る。
2. **ホストにプラグイン探索が無い。** `ene-host` は `ene-plugin` も `ene-plugin-runtime` もリンクしておらず、プラグインのディレクトリを読む経路も無い。`Connection.endpoint().adapter` がプラグインを指すとき、どのパッケージをどのディレクトリから読むのかを決める層が先に要る。

この2点は段階2の前提ではなく、段階2が新たに必要とする作業である。`Wire` + `Transport` のアダプタ自体は書けて型検査も通ることを確認済み（`PluginEmbedding` が `rig_core::driver::Transport` を実装し、`Unreassembled` と `wire::Json` で `DynModel<Embedding>` に消去できる）。ブロッカーはアダプタではなく、同期境界とパッケージ探索である。

**段階3：LLM を移す。** `Wire` 実装のアダプタを書き、`model::connect` に分岐を足す。ここが最もリスクが高い。ストリーミング（`Operation::Generate` の `Delta` イベント）とツール呼び出しの両方を通す必要がある。`examples/plugins/openrouter-wasm` が `provides: ["llm"]` の manifest と `Wire` 相当の実装を持つので、これを雛形にする。ただしこの example は `just build-test-plugins` でビルドされるだけで、ホストから実行された実績はない（`grep` で実行箇所0件）。雛形であって動作実績ではない。段階3も段階2と同じくパッケージ探索を前提にする。

**段階4：TTS / STT を移す。** 音声は `SynthesisResult` のチャンク読み出しが要るので最後にする。`path` オプションは `ProviderConfiguration.settings` に移し、現行テストの契約を保つ。

### 3.4 移行期間中の共存

共存は**接続種別ごとに排他**にする。1つの `Connection` が「プラグイン経路」か「カタログ経路」のどちらか一方だけを通る。両方を通す fallback は作らない。理由は、失敗時にどちらが答えたか分からなくなるためである。

判定は `Connection.endpoint().adapter` が `provider_of()`（カタログ）に解決できるかで行う。解決できれば現行経路、できなければプラグイン経路。現行の `provider_of` は既にこの判定に使われている（`model.rs:154`）。

### 3.5 実装状況（2026-11-15）

| 段階 | 状態 | 根拠 |
| --- | --- | --- |
| 1. `HttpBroker` | 完了 | `crates/ene-plugin-runtime/src/service/http.rs`。7つのテストが宛先拒否・認証注入・予約ヘッダ拒否・チャンク読み出し・上限・close・既定拒否を固定 |
| 2. 契約 | 完了 | `Extension::{Embedding,Tts,Stt}`、`Operation::{Embed,Synthesize,Transcribe}`、`ProviderConfiguration.extensions`、`AudioRead`。`contract_version` は 5 |
| 2. レジストリ | 完了 | `apps/ene-host/src/host/providers/`。起動時に `<state_dir>/plugins/` を読み、接続が参照するプラグインだけを開始する |
| 2. Embedding | 完了 | `mind/plugin.rs` の `Wire` + `Transport` が `DynModel<Embedding>` に消去される |
| 3. LLM | 完了 | `model/plugin.rs`。`Completion` は `Emit = Assembled` なので、decoder に加えて `Reassemble<StreamEvent>` を実装した。`model::connect` / `probe` / `list_models` がレジストリを引く |
| 4. TTS / STT | 完了 | `voice/plugin.rs` の `PluginSpeech` が既存の `TtsEngine` / `SttEngine` を実装。認証は `providers/grant.rs` が持ち、ゲストには渡らない |

段階3で分かったこと。`Completion` の decoder は `Out` の part 書き込み API（`open` / `push` / `finish`）を使う。`Out::event` と `Out::raw` は `Completion` では型レベルで塞がれており、イベントを直接注入する道はない。したがって reassembler は「イベントを畳んで文書を作る」係であり、decoder は「イベントを part に変換する」係として分かれる。この分離は rig の他のプロバイダー実装と同じ形である。

ツール呼び出しは段階3では表現できなかった。`GenerationRequest` にツール定義がなく、`Event` にツール呼び出しの variant がなかったためである。これは契約拡張（`contract_version` 6）として別途実装した。

- `GenerationRequest.tools: Vec<ToolSpec>`（`name` / `description` / `parameters`）
- `ChatMessage` をタグ付きの3形にした。`Text { role, text }`、`ToolCalls { calls }`、`ToolResult { call_id, name, content, is_error }`。アシスタントの呼び出しとその結果を、通常のテキストと区別して ReAct の2ターン目以降に渡せる。
- `Event::ToolCall { id, name, arguments }`
- `LlmCapabilities.tool_calls`（宣言したプラグインにだけツールを渡す）

ホスト側は rig の `ToolDefinition` と履歴（`AssistantContent::ToolCall` / `UserContent::ToolResult`）をこの形にエンコードし、`Event::ToolCall` を `Block::Call` としてデコードする。`Block::Call` は `whole` で1ブロックとして書く（引数は1イベントで揃うため、ストリームにする余地がない）。`FinishReason` は rig が `Stop` を `ToolCalls` に整合させるので、プラグインが `stop` を返しても呼び出しがあれば正しく `ToolCalls` になる。

検証は `contract/llm.rs` が持つ。空の `id` / `name`、オブジェクトでない `arguments`、`MAX_TOOL_CALLS = 64` を超える呼び出しは `Error::Protocol` で拒否する。実装中に、`Events::accept` が `Finished` と `Models` しか LLM 検証に渡していないことが分かった。`ToolCall` の検証が到達不能だったため、ディスパッチを直した。この不具合は追加したテストが検出した。

往復テストは `crates/ene-plugin-runtime/tests/embedded.rs` に置いた。実 Wasm プラグインが1ターン目に `ToolCall` を返し、ホストが結果を返した2ターン目にテキストで答えて完了する。

---

## 4. この提案が引き受けるリスク

| リスク | 内容 | 対処 |
| --- | --- | --- |
| 音声のチャンク化 | `SynthesisResult` が 1 MiB を超える | `AudioRead` ハンドル方式を段階4の前提にする |
| `Wire` 実装の難度 | `DynModel` は公開コンストラクタを持たない | 解決済み。`Wire` + `Transport` + `Unreassembled` で消去できることを型検査で確認した |
| 同期境界 | `mind/embedding.rs::target()` は同期、プラグインのロードは非同期 | 段階2の前提作業。経路の非同期化か、起動時先読みのレジストリ |
| パッケージ探索 | ホストはプラグインをリンクも探索もしていない | 段階2の前提作業。接続の adapter からパッケージを解決する層 |
| インスタンス数 | 4拡張 × 接続数で `MAX_INSTANCES = 16` に当たる | 接続ごとに1インスタンスではなく、provider ごとに1つを共有する |
| 起動時間 | 初回コンパイルが30秒上限に近い | 段階2で実測する。超えるなら `MAX_CODE` かキャッシュを見直す |
| 契約破壊 | `contract_version` 4 → 5 | 旧 manifest は明示的に拒否する。`Package::load` が版不一致で落ちることをテスト済み |

---

## 5. 決定済みと、残っている決定

決定済み（2026-11-15、オペレーター承認）。

1. **`AudioRead` を `ServiceRequest` に足す。** 承認され、実装済み。`HttpRead` と対称なハンドル方式で、`Service::Http` に属する。
2. **`ProviderConfiguration.extensions` を足す。** 承認され、実装済み。`contract_version` は 5。`Package::load` は、provider を持つ entrypoint が provider 系の拡張を1つも提供しない場合と、`extensions` が `provides` に無い拡張を名指す場合を拒否する。
3. **標準プロバイダーはハイブリッド。** 承認され、`rig-core` のカタログ経路を残す。追加プロバイダーはプラグイン経路。

残っている決定。

なし。2点ともオペレーター承認済みで、実装者案で確定した（§6）。

---

## 6. 実装上の決定記録（2026-11-15）

オペレーター承認済みの2点は、実装者案で確定した。

1. **同期境界は起動時先読みで越える。** `providers::Providers` が ECS Resource として起動時にパッケージを読み、接続が参照するプラグインだけを `start_used` で開始する。`target()` / `model::connect` / `tts_source` はすべて同期のままレジストリを引く。
2. **パッケージは `<state_dir>/plugins/<id>/`、adapter 名 = `Manifest.id`。** `ENE_HOST_PLUGINS_DIR` で上書きできる。SQLite スキーマと `ConnectionEndpoint.adapter` の契約は変えない。

この2点により、段階2と段階4は同期境界を崩さずに接続できた。
