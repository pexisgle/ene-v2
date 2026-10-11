# Windows 11 x64

Windows版はMSVCでソースからビルドします。ホスト、会話・設定、アバター、Luau/WASM runnerを別プロセスとして扱います。配布パッケージ、インストーラー、ログイン時の自動起動、Windowsサービス登録は提供しません。

## ビルドと起動

Rustup、Visual Studioの「C++によるデスクトップ開発」（MSVC x64とWindows SDK）、CMakeを用意します。Rustはリポジトリの `rust-toolchain.toml` に合わせます。Pythonはサンプルアセットの取得時だけ必要です。通常起動にPythonやWebViewは不要です。

PowerShellから次を実行します。`just` は任意です。

```powershell
cargo build --locked -p ene-plugin-runner
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-targets --all-features --locked
python scripts/fetch-assets.py
cargo build --release --locked -p ene-avatar
$env:ENE_AVATAR_BIN = "$PWD/target/release/ene-avatar.exe"
cargo run --locked -p ene-desktop -- --demo
```

実ホストへの接続では、端末を2つ開いて `cargo run --locked -p ene-host` と `cargo run --locked -p ene-desktop` を起動します。`ENE_AVATAR_BIN` はデスクトップを起動する端末で設定します。`just verify`、`just run-host`、`just run-desktop`、`just connect-desktop` も利用できます。デスクトップの終了はホストを終了しません。

## 保存とOS連携

保存先は `%LOCALAPPDATA%\ene` 配下です。`config/client.json`、`cache/models`、`cache/motions`、`state/host.sqlite3`、`runtime/ene-host/connection.json`、`plugins`、`providers`、`plugin-code` を使います。`ENE_HOST_STATE_DIR`、`ENE_PLUGIN_DIR`、`ENE_PLUGIN_CACHE_DIR`、`ENE_PROVIDER_DIR`、`ENE_PLUGIN_RUNNER_BIN`、`ENE_AVATAR_BIN` は引き続き指定できます。

ホスト実行領域、DB、設定はユーザーとSYSTEMだけに許可したDACLで作成します。接続情報は開いたハンドルで所有者、ACL、reparse point、リンク数、サイズを検証します。危険な既存ファイルやフォルダの権限を自動で緩めません。共有フォルダやディレクトリジャンクションは実行領域に指定しないでください。

APIキーはWindows Credential Managerへ保存します。利用できない場合は現在のホストセッション内だけに保持し、その状態を画面へ表示します。ファイル選択はWindows標準ダイアログ、トレイはWindows通知領域を利用します。Explorerが再起動するとアイコンを再登録します。

アバターはDX12/DirectCompositionでGPU画像を提示します。描画と輪郭入力のHWNDを分け、GPUから取得した輪郭座標だけを入力領域に反映します。DPIはウィンドウごとに扱い、配置はモニター左上を原点とする96DPI基準の論理座標、描画と入力領域は物理ピクセルです。

## プラグイン

Luau、WASM、Process、RemocプロバイダーはワーカーごとのAppContainerで実行します。直接ネットワークへの権限を与えず、Host brokerで通信します。Job Objectでプロセスツリーを管理し、取消・Host終了時に停止します。隔離に失敗した場合は利用不可になります。

Luau/WASMパッケージのmanifest形式は共通です。ProcessはWindows用 `.exe` をパッケージへ含め、`command` に `/plugin/<相対パス>.exe` を指定します。DLLやPythonの埋め込みランタイムもパッケージ内へ配置します。Python版は例として `["/plugin/python/python.exe", "-I", "-u", "/plugin/provider.py"]` を使います。任意のホスト側インタープリター、`/usr/bin/python3`、LinuxのELFはWindowsで実行しません。

OpenRouterのRustサンプルは次で配置できます。

```powershell
cargo build --release --locked -p ene-provider-openrouter
$providerDir = if ($env:ENE_PROVIDER_DIR) { Join-Path $env:ENE_PROVIDER_DIR 'openrouter' } else { "$env:LOCALAPPDATA/ene/providers/openrouter" }
New-Item -ItemType Directory -Force $providerDir | Out-Null
Copy-Item target/release/ene-provider-openrouter.exe "$providerDir/provider.exe"
Copy-Item providers/openrouter/manifest.windows.json "$providerDir/manifest.json"
```

設定で再走査・有効化した後、接続とAPIキーと送信許可を登録します。プラグインごとに実行ファイルのOSが合う必要があります。パッケージ内のパスは `/` 区切りで、相対パスに限定します。Windowsの予約名、代替データストリーム、大文字小文字だけの重複は拒否します。

## 検証範囲

`.github/workflows/windows.yml` はMSVCでworkspaceのコンパイル・Clippy・テストを実行します。`ene-platform` は実AppContainerの秘密ファイル拒否、ネットワーク拒否、子プロセス停止、ネイティブIPCと異常終了を検証します。DX12画像提示はグラフィックスセッションで `cargo test -p ene-avatar -- --ignored` を明示実行します。実機のIME、ドラッグ、DPI変更、Explorer再起動、ファイル選択は別途手動確認が必要です。

Wasm PDKは `cargo build --release --locked -p ene-openrouter-wasm --target wasm32-unknown-unknown` 後、`python scripts/check-wasm-plugin.py` で合成HTTPのモデル一覧、UTF-8分割生成、終了を実runnerのRemoc通信で検証します。
