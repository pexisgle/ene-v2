# Windows 実機手動スモークテスト（5 分）

PR-4.1〜4.3b で Win32 FFI を `crates/ene-platform-os` へ移し、`apps/ene-desktop` に
`#![forbid(unsafe_code)]` を入れ、`unsafe` 系 2 lint と `let_underscore_must_use` を
`deny` にした。**ゲートはコンパイルと単体テストしか保証していない。**
以下は自動テストで代替できない挙動の確認手順。所要 5 分。

## 準備

```pwsh
cd C:\dev\ene-v2
cargo build --profile ci -p ene-host -p ene-desktop
# 別ターミナルで host を起動
cargo run --profile ci -p ene-host
# さらに別ターミナルで desktop を起動
cargo run --profile ci -p ene-desktop
```

`%LOCALAPPDATA%\ene\` に discovery ファイルが書かれていること（host 起動時）。

---

## 1. 起動確認（30 秒）

| # | 操作 | 期待 |
| :--- | :--- | :--- |
| 1.1 | desktop を起動 | ウィンドウが出る。panic も即時終了もしない |
| 1.2 | タスクバーを見る | **アイコンが 1 個だけ**（`WS_EX_TOOLWINDOW` を外した副作用で 2 個出ていないこと） |
| 1.3 | ウィンドウの枠を見る | DWM の枠・角丸・影が**無い**（`suppress_dwm_frame` が効いている） |

失敗時: 1.2 は `tray.rs::suppress_dwm_frame` の style ビット、1.3 は `DwmSetWindowAttribute` 3 連呼を疑う。

## 2. トレイ（90 秒）

| # | 操作 | 期待 |
| :--- | :--- | :--- |
| 2.1 | トレイアイコンを左クリック | Pod が前面に出る |
| 2.2 | もう一度左クリック | Pod が隠れる（トグル） |
| 2.3 | 右クリック | コンテキストメニューが**マウス位置**に出る |
| 2.4 | メニューの「チャット画面を開く」 | ハブが開く。メニューが閉じる |
| 2.5 | 右クリック →「終了」 | プロセスが終了する。**アイコンがトレイに残らない** |
| 2.6 | 再度起動 → 右クリック | メニューが**1 回目のクリックで**開く（`PostMessageW(WM_NULL)` の workaround が効いている） |

失敗時: 2.5 は `WM_DESTROY` の `delete_tray_icon`、2.6 は `show_context_menu` の末尾 2 呼び出しを疑う。

## 3. Pod のウィンドウ挙動（60 秒）

| # | 操作 | 期待 |
| :--- | :--- | :--- |
| 3.1 | サイズを変える（モード切替） | **下端が動かない**（`anchor_bottom` が効いている） |
| 3.2 | 他のウィンドウを前面に出す | Pod が**前面に残る**（`keep_topmost`） |
| 3.3 | Pod を表示/非表示 | 表示時もフォーカスを奪わない（`SW_SHOWNOACTIVATE`） |
| 3.4 | マルチモニタで負座標側へ移動 | 座標がずれない |

失敗時: 3.1 は `PodWindow::bottom`/`anchor_bottom`、3.3 は `set_visible` を疑う。

## 4. スクリーンキャプチャ（60 秒）

| # | 操作 | 期待 |
| :--- | :--- | :--- |
| 4.1 | Eyes を有効化して数秒待つ | ホスト側ログに perception イベントが届く |
| 4.2 | 別アプリを前面にして再度有効化 | その前面ウィンドウが見える（`GetForegroundWindow`） |
| 4.3 | ロック画面にする | キャプチャが**失敗する**（`unlocked()` が false） |
| 4.4 | 復帰して再試行 | 再び取れる |
| 4.5 | 4K モニタで取得 | 画像が 1280px 以下に縮小されている（`MAX_FRAME_EDGE`） |

失敗時: `capture.rs` の `physical`（DPI コンテキスト）と `copy`（DIB/SelectObject の復元）を疑う。

## 5. 入力注入（90 秒）

Computer Use を有効化し、メモ帳を前面にして実行。

| # | 操作 | 期待 |
| :--- | :--- | :--- |
| 5.1 | クリック | メモ帳にフォーカスが移る |
| 5.2 | 文字入力 | 正しい位置に入力される（`point` のスケーリング） |
| 5.3 | **修飾キーを押したまま**実行 | 拒否される（「modifier or pointer button is held」） |
| 5.4 | **ene のウィンドウ**を対象に実行 | 拒否される（「cannot send input to ene windows」） |
| 5.5 | 実行中に別ウィンドウへ切り替え | 拒否され、再キャプチャを促される |

失敗時: `input.rs` の `check_before_send`（3 段階の拒否）と `point`/`movement` を疑う。

---

## 結果の記録

失敗した項目は `docs/refactoring/PROGRESS.md` の「実機検証」節に追記してください。
`#![forbid(unsafe_code)]` は**これらの挙動を何も保証しない**——不安全でないことと
正しく動くことは別である。
