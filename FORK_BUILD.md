# ForkのローカルbuildとCodex-switch接続

2026-09-22。Linuxでの初回試行用。非公式Forkであり、公開release・三OSの受入完了を意味しない。
「再現条件」は2026-09-22の初回build（独自Compaction処理なし）の記録である。現在の稼働binaryは新方式の通常Compactionを含む（SHA-256 `3f6d6d61bd176ee08e65c0a486a4dfc3a9aceb7c6cbbcfc7024305ea87ad03b9`、source記録は私有の`target/fork-bootstrap/compaction-runtime-deploy/candidate.json`）。最新sourceの実装状況はCOMPACTION_SPEC.mdの「実装状態」を参照。build完了・接続の実測結果はLLM ownerの監督検証記録で管理する。

## 再現条件

- source: `f96093ad9d3cd0d31a713789e99d7397a5bc0790`（上流基点はFORK_RULES.md）。
- Rust: `codex-rs/rust-toolchain.toml`の1.95.0。lockfileを維持する。
- C compiler、pkg-config、OpenSSL開発header/library、CMakeを用意する。
- 初回試行は上流定義の`dev-small` profile、2並列。release性能の測定値として扱わない。

```sh
cd codex-rs
cargo +1.95.0 build --locked --profile dev-small -j 2 -p codex-cli --bin codex
```

このホストでは管理者権限を使わず、`target/fork-bootstrap`へRustとUbuntuの依存debを展開する。
`CARGO_HOME`、`RUSTUP_HOME`、`CARGO_TARGET_DIR`を専用領域へ設定する。
展開したOpenSSLには`OPENSSL_DIR=<sysroot>/usr`、
`OPENSSL_LIB_DIR=<sysroot>/usr/lib/x86_64-linux-gnu`、`OPENSSL_STATIC=1`、
`CPATH=<sysroot>/usr/include/x86_64-linux-gnu`を設定する。
CMake等の`usr/bin`をPATH、`usr/lib/x86_64-linux-gnu`をbuild時のLD_LIBRARY_PATHへ追加する。
これらはホスト固有のbuild依存解決であり、配布binaryの実行時設定ではない。
bootstrap、binary、会話・認証情報はcommitしない。

## 配備用build（2026-09-26から）

配備する`rencrow-switch-core`（`codex`）と`rencrow-compaction`は`fork-deploy` profileでbuildする。
`dev-small`と同じく最適化なし・debug情報なしで、`debug-assertions`だけを切る。
`dev-small`の配備binaryでは、上流の本番buildならerrorとして記録するだけの`error_or_panic`
（`core/src`内12箇所）や`debug_assert!`がpanicし、sessionが応答しなくなった
（2026-09-25 23:45、`94523a2f6`の原因）。試験は従来どおり`dev-small`（debug-assertions有効）で行い、
不整合は試験で検出する。

```sh
cd codex-rs
cargo +1.95.0 build --locked --profile fork-deploy -j 2 -p codex-cli --bin codex --bin rencrow-compaction
```

成果物は`target/fork-build/fork-deploy/`。未梱包binaryとしての起動条件（`--no-daemon`等）は`dev-small`と同じ。

## 接続と復旧

1. build成果物の`--version`、`--help`、動的依存関係を確認し、source SHAとbinary SHA256を記録する。
   開発版のversionが`0.0.0`の場合、version表示だけで出自を判定しない。
2. `~/.local/bin/rencrow-switch-core`という別名へ導入する。公式`codex`は置換しない。
3. 新規環境はRenCrow_LLMの`examples/codex/install.sh`でlauncherとassetsを配備する。
   既存profile/catalogに運用設定がある場合は、それを保全してwrapperだけを更新する。
   RenCrow targetは`RENCROW_CODEX_BIN`の明示指定、PATHの`rencrow-switch-core`、公式`codex`の順に選択する。
   `openai` targetは公式版を使う。Windowsではlauncher選択契約を共有するが、native確認は別途必要。
4. 旧Codexのwriterを停止し、専用CODEX_HOMEのsession・設定・SQLiteを私有領域へbackupする。
   認証情報を複製しない。同じsessionを新旧同時に開かない。
5. 同じsessionをresumeし、実processの実行ファイル、model、Gateway経路、sandboxとapproval、実tool結果を確認する。
   Qwenへのrole指示は実行担当として与え、監督Astraの役割を混ぜない。
6. 復旧時は新writerを止める。新旧state schemaと外部効果を照合したうえで、
   `RENCROW_CODEX_BIN=codex codex-switch rencrow-safe <session-id>`により公式版を選択する。
   DBを無条件に巻き戻して新しい作業・外部効果を失わせない。

本基点には公式0.155.1より新しいSQLite migrationが含まれる。旧版のmissing migration許容だけでは
完全なrollback保証にならないため、backupと実resume確認を省略しない。


## 別系統の入力記録を試験する場合

`codex-cli`の`codex`と`rencrow-compaction`を同じrevisionからbuildする。
TUIの`--rencrow-input-author human|automation`は必須（未指定では起動しない）で、送信本文・添付の別記録を有効にする。
通常Compactionの切替フラグではない。利用と制限はCOMPACTION_CLI.mdを参照。
稼働中の旧binaryはこのフラグに対応しない。既存writerの差替えや再起動を伴わない試験は、
新binary・独立CODEX_HOME・独立sessionを使用し、同じ既存Gateway/model契約を維持する。

試験済みの未梱包dev-small binaryには`--no-daemon`が必要。起動例:

```sh
CODEX_HOME=/private/isolated-home ./target/fork-build/dev-small/codex --no-daemon --rencrow-input-author human
```

既存TUI回帰をこのホストで確認する際は、短い一時path、私有umask、通常の端末色設定を用いる。
Git 2.34.1では上流worktreeの`list -z`に対応できず、関連検査は未受入。今回の機能のためにGitや製品の検査を弱めない。

## 新方式の通常Compaction

新方式対応binaryをbuild後、対象CODEX_HOMEの有効な設定に次を設定する。
launcherによるprofile再生成後も維持する運用設定は`CODEX_HOME/config.toml`のtop levelに置く。
独立試験ではprofile v2の`<profile>.config.toml`のtop levelにも設定できる。

```toml
rencrow_compaction = true
```

`/compact`とlocalの自動Compactionが同じ検証済みpipelineを利用する。
要約生成は1要求で、モデルにWork ID全件の転記を求めない。host-bound `summary_hash`、coverage、invalidation検査と保存barrier成功後だけ通常履歴へ採用する。人間由来入力がある場合のplan提案・意味reviewは維持する。summary reviewは自動実行せず、bundleには`summary_review: null`を未実施receiptとして保存する。
remote V2 / TokenBudgetとの併用は拒否し、旧方式へ暗黙fallbackしない。
sourceにあるV2（8工程）の部品はこの経路にまだ接続されていない。この設定で動くのは上記の現行経路である。
`--rencrow-input-author human`は利用者による直接投稿の受付を申告する設定。
Astra等による代理操作は`automation`を指定する。旧記録・照合不一致を本人入力と推定しない。

各段階の表示は出力token数 / request全体のwall秒（prefill・待ち時間込み）であり、
Backendの純粋なdecode速度ではない。実行したplan提案・plan review・要約要求のusageはcheckpointに残り、summary reviewが通過したと偽装しない。

保存結果が不確定になったruntimeは次の通常turnを拒否する。再起動時はownerのappend-only
replayがhash一致の完了印まで確認できる最後のcheckpointを利用する。不完全な新checkpointは読み取りviewで除外する。電源断・書込み失敗時に複数JSON行が
filesystem transactionになるとは主張しない。原ログやDBを手動で切り詰めない。

配備前に隔離sessionで現在要求の維持、撤回部分の非復活、通常toolの利用、
再起動と2回目のCompactionを確認する。共有sessionのwriterを二重に起動しない。
