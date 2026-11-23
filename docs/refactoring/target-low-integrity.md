# `target` が Low 整合性レベルになっている件

## 症状

`cargo run -p ene-host` が `%LOCALAPPDATA%` に書けず、こう落ちる:

```
ene-host: cannot use the state directory C:\Users\<user>\AppData\Local\ene\data ...
(アクセスが拒否されました。 (os error 5))
```

## 原因

`C:\dev\ene-v2\target` に **Low 整合性ラベル**が付いており、配下の全ファイルへ継承される。

```
C:\dev\ene-v2          -> Mandatory Label\Medium Mandatory Level   ← 正常
C:\dev\ene-v2\target   -> Mandatory Label\Low Mandatory Level      ← 原因
C:\dev\ene-v2\target\ci\ene-host.exe -> Low（継承）
```

Low IL のプロセスは Medium IL の `%LOCALAPPDATA%` に書き込めない（Windows の
No-Write-Up ポリシー）。そのため `ene-host` が状態ディレクトリを作れずに落ちる。

`.cargo-tmp` も同じく Low になっている。

**`apps/` は Medium で正常**なので、リポジトリ全体ではなく `target` と
`.cargo-tmp` の 2 つだけが該当する。

## 確認コマンド

```pwsh
icacls C:\dev\ene-v2\target | Select-String Mandatory
# → Mandatory Label\Low Mandatory Level:(I)(OI)(CI)(NW)  が出れば該当
```

## 修正（管理者 PowerShell が必要）

Low IL のファイルは Medium から削除も変更もできない。管理者として実行する。

```pwsh
# 1. 管理者 PowerShell を開き
cd C:\dev\ene-v2

# 2. 該当ディレクトリを消す（中身ごと作り直させる）
Remove-Item .\target -Recurse -Force
Remove-Item .\.cargo-tmp -Recurse -Force

# 3. ラベルが付いていないことを確認（親の Medium を継承する）
New-Item -ItemType Directory .\target | Out-Null
icacls .\target | Select-String Mandatory
# → 何も出なければ正常
```

そのあと通常のターミナルで:

```pwsh
cargo run --profile ci -p ene-host
```

`ene-host` が `%LOCALAPPDATA%\ene` を自分で正しく作る（ACE 1個・継承フラグなし・
保護ありの DACL になる）ので、手動で ACL を触る必要はない。

## なぜ Low になったか（未確定）

`C:\dev\ene-v2` には以前 `CodexSandboxUsers` というグループの ACE が付いていた
形跡がある（`C:\dev\ene-v2` の DACL、および `%LOCALAPPDATA%\ene` の継承 ACE）。
別のサンドボックスツールが `target` を Low IL で作った可能性が高いが、
**確証はない**。

## 回避策（管理者権限なしで今すぐ動かす）

ターゲットを Low でない場所へ移す。ただしこれは根本解決ではない。

```pwsh
$env:CARGO_TARGET_DIR = "C:\dev\ene-target"
cargo run --profile ci -p ene-host
```

`C:\dev\ene-target` はラベルなし（Medium）なので動作する。実測で
`ene-host` と `ene-desktop` の両方が起動し、`%LOCALAPPDATA%` に正しい
DACL の状態ディレクトリが作られることを確認済み。
