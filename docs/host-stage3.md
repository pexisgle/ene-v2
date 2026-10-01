# 第3段階：ホスト接続の実装状況

## 今回成立した実装単位

`feat/host-api`は、[共通IPCのPR #1](https://github.com/pexisgle/ene-v2/pull/1)と[会話画面・VRM/VRMAのPR #2](https://github.com/pexisgle/ene-v2/pull/2)の後続作業ブランチです。第3段階の最初の単位として、独立して起動できるホストの認証・初期化・履歴・通知購読を実装しています。**Slint画面の実モデル対話への接続はまだ実装していません。**

実装済みのAPIは次のとおりです。すべてBearer認証が必要です。

- `GET /v1/snapshot`：任意の`connection_id`を検証し、状態と通知カーソルを同じ境界で返す。
- `GET /v1/companions`、`GET /v1/companions/{companion_id}`：初期コンパニオンと未設定理由を返す。
- `GET /v1/companions/{companion_id}/messages`、`GET /v1/companions/{companion_id}/messages/{message_id}`：履歴と保存済みメッセージ。ページは既定50件、上限200件、ページ内はsequence昇順。
- `GET /v1/settings`、`GET /v1/grants`：初期の空設定・許可を返す。
- `GET /v1/events`：通知用WebSocket。購読登録後に`ready`を送り、切断・配送遅延時に購読を無効化する。

capabilitiesは実装済みの`history`と`notifications`だけを含みます。アダプター一覧は空です。会話送信、設定変更、資格情報、送信許可、Task、ファイル作業、音声はまだ公開していません。空のTask・Question・Grant一覧は対応済みを意味しません。

## 起動と保存

```sh
cargo run --locked -p ene-host
```

`XDG_RUNTIME_DIR`は現在のユーザーが所有する非公開ディレクトリが必要です。ホストはIPv4ループバックのOS割当ポートで待ち受け、`$XDG_RUNTIME_DIR/ene-host/connection.json`に`endpoint`と起動ごとの`token`を原子的に公開します。ディレクトリは0700、ファイルは0600とし、公開領域・シンボリックリンクを拒否します。ロックにより同じruntimeでの二重起動を拒否します。終了時は接続情報を削除し、WebSocketが接続中でもSIGTERMで終了します。通常ログへトークンを出しません。

DBは`$XDG_STATE_HOME/ene/host.sqlite3`、未設定なら`~/.local/state/ene/host.sqlite3`です。検証では`ENE_HOST_STATE_DIR`で一時領域を指定できます。DBの専用ディレクトリは0700、DBは0600です。番号付きの`001_initial.sql`と`PRAGMA user_version`でスキーマを管理し、新しい未知のスキーマは拒否します。

DB処理は有限の要求キューと専用スレッドに集約し、Tokioスレッドで同期SQLite処理を待ちません。初期化はトランザクションでホスト・環境・認証主体・コンパニオン・設定を保存します。これらのIDとrevisionは再起動で作り直しません。`queued`・`streaming`のMessageが残っていれば、部分本文を保持して`failed / host_interrupted`に確定し、自動再実行しません。冪等受付用のテーブルも用意していますが、受付・再送APIの実装は次の単位です。

systemdユーザーサービスのテンプレートは`packaging/systemd/ene-host.service`です。この環境ではsystemd起動を検証していません。

```sh
cargo build --release --locked -p ene-host
install -Dm755 target/release/ene-host ~/.local/bin/ene-host
install -Dm644 packaging/systemd/ene-host.service ~/.config/systemd/user/ene-host.service
systemctl --user daemon-reload
systemctl --user enable --now ene-host.service
```

## 通知の再同期

共有クレートの`host::Replica`に通知の適用処理を置きました。購読→スナップショット→スナップショット以下の通知を破棄→連番適用の順で使います。未知の通知も連番を進めます。欠番・epoch変更・本文offset不一致・壊れたペイロードではカーソルと本文を変更せず、再同期を要求します。offsetはUTF-8のバイト数や書記素数ではなく、Rustの`chars()`と一致するUnicodeスカラー値数です。Slint側のバックグラウンド接続・再試行への組み込みは次の単位です。

## OpenRouterの実接続確認

2026-10-01に環境変数`OPENROUTER_API_KEY`の存在を確認し、固定の日本語検証文だけを送りました。キー、会話、ファイル内容は保存していません。`openai/gpt-6-luna`、`provider.only=["openai"]`、`provider.allow_fallbacks=false`を指定し、応答のモデル名と配信先OpenAIを確認しました。本文は6チャンク、最初の本文まで1.133秒、合計1.263秒でした。この計測は接続確認の1回分で、UIの応答速度を示しません。

この一度限りの疎通確認スクリプトは削除し、結果の記録を保持しています。現在のホスト自体はこの環境変数を取り込まず、外部送信もしません。資格情報ストア、session_onlyの表示、通常の会話・疎通確認に必要な送信Grantの検証は後続実装です。疎通確認の成功を会話送信許可成立やUI接続完了とは扱いません。

## 検証

```sh
just verify
```

自動テストは認証拒否、秘密情報の非表示、初期化IDの維持、未来のスキーマの拒否、ページ境界と範囲外の生成中Message、起動後の中断処理、通知のUnicode offset・欠番・epoch・重複と終了時正本を確認します。初期実装時には実ホストを一時runtimeとDBで起動し、実HTTP/WebSocket、購読とsnapshot、切断後の購読無効化、二重起動、接続中の終了、トークンとepochの更新、再起動後のID維持を検証しました。この検証用Pythonスクリプトと実行コマンドは削除し、[検証記録](validation/host-stage3-foundation-2026-10-01.txt)を保持しています。

次の実装単位は、設定・資格情報・送信許可とモデル割当、RigのOpenRouterストリーミング、永続的な冪等メッセージ受付、Slintの接続・再試行・履歴表示です。第3段階の完成条件は、これらの実装と実モデル対話のUI検証が済むまで未達です。
