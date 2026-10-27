# Host Plugin 実行基盤

Host は Extism 1.41.0、mlua 0.11.6＋mlua-luau-scheduler 0.2.4を利用します。Wasm の ABI は Extism PDK に任せます。ネイティブ Lua モジュールを Host のプロセスへロードする機能は設けず、特殊なネイティブ SDK は後続の隔離サービスで接続します。

## 責務と契約

- `ene-plugin`：Luau／Extismのmanifest、worker・サービス・拡張API契約。互換性は `CONTRACT_VERSION = 1` と `contract_version` で管理する。
- `ene-rpc`：host.protoの生成RPC型。Model等を共有するが、プラグイン管理のRPCとworker通信は別の契約。
- `ene-plugin-runtime`：コードの検証・取り込み、worker の世代・通信・終了状態の検証、監督。Extism／mlua に依存しない。
- `ene-platform`：Linuxのbubblewrap、WindowsのAppContainer＋Job Objectによる共通の隔離起動。ネットワーク権限を与えず、起動失敗時は拒否する。
- `ene-plugin-sdk`：Extism Rust PDK の薄いラッパー。`sdk/luau/ene.d.luau` は他言語向けの契約。
- `ene-plugin-runner`：Extism／Luau の実行、非同期サービス待機、制御用 IPC。Wasm 呼び出しと IO を別々に処理する。
- `ene-core::ports::plugin`：管理・割り当て・呼び出しの port。`ene-host::infrastructure::plugins` が broker、SQLite、既存 Provider に接続する。

プラグイン同士の import は提供しません。Luau の `require` は `code_files` に列挙されたパッケージ内の相対パスだけを読みます。VM は sandbox 化し、Host のファイル、資格情報、DB、ネットワークへ直接アクセスさせません。

## パッケージとインスタンス

`ENE_PLUGIN_DIR/<package>/manifest.json` に `contract_version: 1`、`id`、`name`、`version`、`code_files`、`entrypoints` を記載します。各エントリポイントは ID、`target`（host／desktop）、`runtime`（extism／luau）、相対実行ファイル、`provides`、`uses` を持ちます。拡張APIとworkerメッセージも同じ `contract_version` を使います。

提供する拡張は `llm`、`tts`、`stt`、`character`、`harness`、`desktop`。利用サービスは `log`、`configuration`、`state`、`http`、`models`、`tools`、`character_actions`、`desktop` です。宣言は利用許可と同義ではありません。

発見時にはコードを実行しません。パッケージ ID・提供するLLMのID の重複を拒否します。有効化すると Host のエントリポイントを隔離環境で初期化します。ソース変更は `changed` として扱い、再走査と新 revision での有効化を要求します。実行コードは検証時のバイト列を `ENE_PLUGIN_CACHE_DIR`（既定：LinuxはXDG dataの `ene/plugin-code`、Windowsは `%LOCALAPPDATA%/ene/plugin-code`）へ読み取り専用で取り込みます。停止後の snapshot は削除します。

コード上限は合計 32 MiB／256 ファイル、manifest は 64 KiB。大きな assets は別にパス・サイズ・SHA-256 を宣言でき、コード制限に合算せず worker へマウントしません。現在は assets のメタデータを検証します。実データのハッシュ検証、リソースハンドル経由の読出し、Character のモデル取り込みは後続の asset サービスで行います。

Provider は要求ごと、Character は binding ごと、Harness はタスクごとに worker を使います。binding は現在の Host に保存されたコンパニオン ID を検証します。Character の状態キーはパッケージ・エントリポイント・コンパニオンで分離し、同じエントリポイントを同じコンパニオンに割り当てた binding 同士では状態を共有します。

## サービスと制御

実行 ID・世代、サービス呼び出し ID、イベント連番を照合します。重複した終了、古い応答、不正 IPC、拡張と異なるイベントを拒否します。キャンセル時は VM 中断だけでなくサービス待機も解除します。Host は協調停止後に worker を強制終了・回収し、隔離不能時には起動を拒否します。

初期化は 30 秒、実行は 180 秒、協調停止は 3 秒。Extism のメモリは最大 1,024 pages（64 MiB）、実行 fuel は 1 億、初期化 fuel は 1,000 万。Luau の VM メモリは 64 MiB、割り込みでキャンセル・期限を確認します。実行とチャネルの制限はruntimeが所有します。監督する Provider 要求と保持する拡張インスタンスは各 16 まで。状態はキーごと 64 KiB、コンパニオンのエントリポイントごと 256 キーです。

HTTP は `http_open` → `http_read` → `http_close`。open は最初のチャンク、read は続き、done は最後のチャンクを返します。ハンドルは broker が発行する参照で、ポインタやファイルパスではありません。HTTP のストリームは最大 8 本、各キューは 4。現行brokerの呼び出し上限は1 Invocationにつき8回、同時実行は1回です。Extism 組み込み HTTP と WASI を無効化し、Host が既存の送信許可、送信先・経路、資格情報、redirect を検証します。生成中の無効化や再有効化でも、旧世代の送信権限は復活しません。

binding の `services` は利用者が許可するサービスです。現在の汎用 binding は log／configuration／state を許可できます。HTTP は Provider の接続設定・送信許可に結び付けて提供します。汎用 Character／Harness に任意の HTTP や shell を与えません。API キーは既存の資格情報管理を利用し、manifest・binding の設定は非秘密値に限定します。

## 各拡張

LLM はモデル一覧、逐次テキスト、完了理由、ツール要求を通信型で区別します。従来の会話 port はテキスト生成だけを受け付けます。Luau／ExtismのLLMは `HostService/ListProviders` に表示し、接続設定、資格情報と送信許可の操作を引き継ぎます。提供IDの重複は拒否します。

Character は有効化・無効化、会話開始・完了、タスク変更、タイマー、名前空間付き独自イベントを受けます。binding 作成と会話開始・完了は application から配送し、残りは invoke API から配送できます。イベントはインスタンス単位で直列化し、application の配送数を制限します。プラグインによるイベントの自己配送 API は設けません。発話・表情・モーションは検証済みの要求として返り、`plugin.character_action` 通知には revision、世代、5 秒の期限を付けます。Avatar への実適用は Desktop 側の後続実装です。

Harness は開始、操作結果、相談への回答、キャンセルを受け、進捗、モデル・ツール要求、相談、最終報告を返します。Host が task ID、待機中の操作 ID、相談、終了状態、記録を保存し、別の操作への応答を拒否します。完了・取消後は worker を回収し、Host の再起動時には未完了タスクを失敗にして自動再開しません。現在の参照実装は invoke API から結果を渡す契約検証用です。モデル・ツールの自動実行、既存の Task API との統合、opencode2／DeepSeek の実アダプターは後続です。

音声は LLM と別の型です。初期の出力形式は PCM s16le。発話 ID、Host が付ける世代、連番、sample rate、channels、終端を検証します。参照実装は無音 PCM／固定 transcript を返し、実 TTS／STT エンジンと音声再生を提供するものではありません。

## Desktop と OS の後続設計

Desktop は同じ package／worker／service 契約を使います。Host では Desktop エントリポイントを実行せず、OS サービスは対応能力として宣伝しません。

| 型付き操作 | 接続する Rust サービス |
| --- | --- |
| ファイル選択 | 現在の ashpd file chooser |
| トレイ | 現在の ksni／Win32 トレイ管理（追加の型付き契約が必要） |
| Avatar | 現在の IPC を検証して配送（表情・モーション対応を追加） |
| 通知・clipboard | OS ごとのアダプター |
| 画面取得・入力 | Linux portal／他 OS の対応アダプター（入力操作は後続契約） |
| 大きな画像・音声 | 所有者・期限を broker が管理する `ResourceHandle` |

現在のアプリはLinux／Windowsのファイル選択とトレイを持ちます。プラグイン向けDesktopサービスの公開は後続です。利用可否は接続時の実サービスで判定します。サービス要求にプラグインが任意の端末IDを付けることはできず、認証済みDesktopの実行コンテキストから対象を決めます。切断でサービスとハンドルを無効化し、別端末へ自動切替しません。任意OS API／任意shellは公開しません。macOSは未対応です。

WindowsでもLuau／Extism runnerをAppContainerとJob Objectで隔離します。パッケージ内の相対パス、予約名・代替データストリーム・大文字小文字の衝突を検証します。[Windowsのビルド・配置と検証](../../windows.md)を参照してください。

隔離内でのsnapshot読込はパッケージroot以下のハンドルを検証し、AppContainerに許可しないHost側の親フォルダへ検証目的でアクセスしません。Hostでのソース検証は親フォルダのreparse point検証も行います。WindowsのWASM runnerは、隔離外の既定フォルダを参照するWasmtime共有コンパイルキャッシュを無効にします。

## 管理・開発・移行

認証済みの Connect `/rpc/ene.host.v1.PluginService/` に `List`、`Get`、`Reload`、`Enable`、`ListBindings`、`Bind`、`Invoke` を用意しました。公開契約は `host.proto`、生成型を `ene-rpc` が提供し、`ene-api-client` が型付き操作を提供します。有効化は現在の `revision` を要求し、更新前の許可を再利用しません。LLM用Provider APIの有効化も同じプラグイン実装へ接続します。

`ene-plugin-runner` は Host と同じバイナリディレクトリへ配置するか、`ENE_PLUGIN_RUNNER_BIN` を指定します。

```sh
cargo build --locked -p ene-host -p ene-plugin-runner
cargo run --locked -p ene-plugin-runner -- --package examples/plugins/reference --validate
rustup target add wasm32-unknown-unknown
cargo build --release --locked -p ene-openrouter-wasm --target wasm32-unknown-unknown
python3 scripts/check-wasm-plugin.py
```

Wasm パッケージは `examples/plugins/openrouter-wasm/manifest.json` とビルド済み `ene_openrouter_wasm.wasm`（`provider.wasm` に改名）をインストール先へコピーします。参照 Luau パッケージは `examples/plugins/reference` のファイル一式をコピーします。コードは現在の UID 所有、group／world 書込み不可、symlink／hardlink 不可です。

Wasm は `ene_initialize` と `ene_invoke` を Extism PDK で export します。Luau は initialize／invoke 関数を持つ table を返します。`ene.service` は scheduler が待機・再開し、`ene.emit` は bounded queue に非同期で出力します。SDK 更新時は pinned framework と `ene-plugin` の契約を合わせて更新し、`THIRD_PARTY_NOTICES` と `licenses/` を配布物に含めます。

`just verify` は整形とClippy、`just test` は契約、実runner、Hostのstate／取消、Luau／Extismと管理APIを検証します。Hostだけテストする場合は先にrunnerをビルドします。Linuxではbubblewrapが名前空間を作成できる環境、WindowsではAppContainerとJob Objectが必要です。PDKサンプルは合成HTTPのモデル一覧・UTF-8分割ストリームを検証し、外部APIを呼びません。Windowsの検証結果と未確認の実機項目は[Windows手順](../../windows.md#検証範囲)を参照してください。起動・コンパイル時間、初回出力、停止時間、メモリ、配布サイズの比較は今後の計測で決めます。

参考：[Plugin runtime の基盤](../../research/references.md#plugin-runtime-frameworks)。

workerの標準入出力はRemoc MPSCの双方向接続です。host→workerとworker→hostのキューを各16件に制限し、MessagePackで動的な拡張payloadを表現します。ExtismゲストABIはPDKのJSON入出力を維持します。LLMのHTTPサービスは現在のInvocationにruntimeの権限失効条件を追加し、元の送信許可と資格情報署名を保持したHTTP brokerへ接続します。
