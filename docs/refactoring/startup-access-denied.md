# 起動時の `os error 5`: 原因と対処

## 症状

```
ene-host: cannot use the state directory C:\Users\<user>\AppData\Local\ene\data
or the runtime directory ...\ene\cache\runtime (アクセスが拒否されました。 (os error 5))
```

ディレクトリを手動で作っても再現する。

## 原因

`private-fs` は状態ディレクトリの**祖先をすべて現在ユーザーが所有していること**を
要求する（所有権の付け替えによる権限昇格を防ぐため）。この環境では:

```
C:\Users                 -> NT AUTHORITY\SYSTEM   ← ここで検証が落ちる
C:\Users\<user>          -> p-desktop\<user>      (icacls で修正済み)
C:\Users\<user>\AppData  -> p-desktop\<user>
```

`C:\Users` が `SYSTEM` 所有なので、その配下の `%LOCALAPPDATA%` は
`PrivateDir::ensure` に拒否される。**コードのバグではなく、この PC の
ユーザープロファイルの所有権が標準と違うことによる。**

`C:\Users` の所有者変更は管理者権限が要る:

```pwsh
# 管理者 PowerShell で
icacls C:\Users /setowner "<user>"
```

## 対処（管理者権限なしで動く）

状態ディレクトリを、所有権チェーンが正しい場所へ移す:

```pwsh
$env:ENE_HOST_STATE_DIR   = "C:\dev\ene-v2\.cargo-tmp\estate"
$env:ENE_HOST_RUNTIME_DIR = "C:\dev\ene-v2\.cargo-tmp\ert"
```

永続化するなら、ユーザー環境変数として登録する:

```pwsh
[Environment]::SetEnvironmentVariable('ENE_HOST_STATE_DIR',   'C:\dev\ene-v2\.cargo-tmp\estate', 'User')
[Environment]::SetEnvironmentVariable('ENE_HOST_RUNTIME_DIR', 'C:\dev\ene-v2\.cargo-tmp\ert',    'User')
```

`C:\dev` 系は `p-desktop\<user>` 所有なので検証を通る。

## 検証済みの起動

上記の設定で `ene-host` と `ene-desktop` の両方が起動し、デスクトップが host に
接続するところまで確認した（2026-11-23）。`ene-desktop` のログ:

```
INFO ene_desktop::session::transport: the client is an active device companion=ene
INFO ene_desktop::session::audio_pipeline: the output device is open rate=24000 channels=2
```

## 補足

- `ENE_HOST_STATE_DIR` と `ENE_HOST_RUNTIME_DIR` は**両方**設定すること。
  片方だけだと残る側で同じ失敗をする。
- 同じ設定で 2 つ目の host を起動すると
  `state or runtime directory already in use` になる（意図した単一ホスト排他）。
- 状態を捨ててよければ、ディレクトリごと消してから起動し直せばよい。
