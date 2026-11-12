# ForkのローカルbuildとCodex-switch接続

2026-09-22。Linuxでの初回試行用。非公式Forkであり、公開release・三OSの受入完了を意味しない。
「再現条件」は2026-09-22の初回build（独自Compaction処理なし）の記録である。当時の配備binary SHA-256は`3f6d6d61bd176ee08e65c0a486a4dfc3a9aceb7c6cbbcfc7024305ea87ad03b9`で、source記録は私有の`target/fork-bootstrap/compaction-runtime-deploy/candidate.json`にある。現行sourceのV2実装状態は[Compaction仕様](COMPACTION_SPEC.md#第3部-現在の実装状態2026-09-25)を参照。利用中binaryの版と接続は起動時に実測し、過去のSHAから推定しない。

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
   現行のlauncherは全Codex targetで`rencrow-switch-core`を要求し、公式`codex`へfallbackしない。
   Windowsでも同じ起動先契約を使い、native確認は別途行う。
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

`/compact`とlocalの自動CompactionがV2のruntime経路を利用する。V2が失敗した場合に上流Compactionへ暗黙fallbackしない。実装状態、失敗status、受入範囲は[Compaction仕様](COMPACTION_SPEC.md#第3部-現在の実装状態2026-09-25)を参照する。
`--rencrow-input-author human`は利用者による直接投稿の受付を申告する設定。
Astra等による代理操作は`automation`を指定する。旧記録・照合不一致を本人入力と推定しない。

各段階の表示は出力token数 / request全体のwall秒（prefill・待ち時間込み）であり、
Backendの純粋なdecode速度ではない。実行したplan提案・plan review・要約要求のusageはcheckpointに残り、summary reviewが通過したと偽装しない。

保存結果が不確定になったruntimeは次の通常turnを拒否する。再起動時はownerのappend-only
replayがhash一致の完了印まで確認できる最後のcheckpointを利用する。不完全な新checkpointは読み取りviewで除外する。電源断・書込み失敗時に複数JSON行が
filesystem transactionになるとは主張しない。原ログやDBを手動で切り詰めない。

配備前に隔離sessionで現在要求の維持、撤回部分の非復活、通常toolの利用、
再起動と2回目のCompactionを確認する。共有sessionのwriterを二重に起動しない。

<a id="macos-host-207"></a>
## macOS host 207への配備 — 2026-10-25

利用者の配置指示により、source `e8417aaea4352e3e77f529adaa13a41c7a653afb`を
Rust 1.95.0、lockfile維持、`fork-deploy` profileでdarwin/arm64へnative buildした。
`codex`と`rencrow-compaction`の両binaryをbuildし、前者を`rencrow-switch-core`として配置した。
公式`codex`の実行fileは置換していない。公開release・Windows受入の記録ではない。

- `--no-daemon --version`／`--help`、Mach-O ARM64、試験binaryと配置binaryのSHA-256一致を確認。
- RenCrow_LLM所有のlauncherを配備し、207のGatewayを通して205のStrata model routeを使用した。
  Gatewayがaliveと報告しないGinはcatalogから除外され、別modelへの暗黙fallbackは行わない。
- 隔離CODEX_HOME／workspaceのTUIから実際の`exec_command`でfixtureを読み、応答内容を確認した。
  操作主体は`--rencrow-input-author automation`と申告し、workspace-writeの境界とhook trustを維持した。
- 同じthreadでNormal V2 checkpoint 2件、Emergency 0件、未commit 0件をowner診断CLIで確認した。
  指示の撤回・置換後に2回目のCompactionを行い、旧合言葉ではなく最新の合言葉を回答した。
  TUIを終了して同じthreadをresumeした後も、最新の合言葉を回答した。
- 通常のSSH起動先は`<HOME>/.local/bin/codex-switch`。会話・DBは専用の
  `<HOME>/.codex-switch`へ分離する。207上の既存認証は参照し、認証情報を複製しない。
  認証の認識は確認済みだが、この試験はOpenAI modeの実inferenceや既存Linux threadの移植受入を含まない。
- 配備templateはadvisor MCPの実pathを解決した。sourceから削除済みの旧delegate MCP参照は
  配備側で無効とし、advisorのinitialize／tools/listとTUIからの起動を確認した。

利用者が作業directoryから新しいsessionを開く場合は`codex-switch rencrow-safe --new`を使用する。
CLIはSSH terminalから必要時に起動する。GatewayとBackendの自動起動は各service managerが所有する。
privateな会話・ログ・認証・binary・build provenanceはGitへ取り込まない。

## 2026-10-27 Compaction selection wire v2

Instruction selection now transmits host-owned passage IDs with a JSON Schema.
Invalid selection retains the affected original instructions while Work summarization
continues; conservative checkpoints retry selection after resume. See COMPACTION_SPEC.md.
The native CLI and rencrow-compaction helper must be deployed together: older readers
cannot read the new conservative_selection checkpoint mode.
Validation on macOS: history 149/149; focused compaction and native execution guard
109/109; diagnosis Python tests 6/6; targeted clippy/fmt and both deployment builds passed.
An isolated live Qwen xhigh run produced a normal checkpoint with two ID-selected
source passages and answered the corrected label after compaction. Selection took
139 seconds in this fixture; no general latency improvement is claimed.
Whole-workspace acceptance was not run for this change. Private traces and deployment
hashes remain outside the repository under Tmp/ren2-supervision.

## 2026-10-28 子結果未回収時の継続修正

`require_child_results`が有効で未回収子が残る場合、従来の即時ターンエラーの前に、
親へ未回収IDと回収・明示close手順をdeveloper messageで返し、同じターンを最大2回継続する。
待ちtimeoutを子の失敗とせず、子の再生成やtool再送、権限変更は自動実行しない。
2回でも解消しない場合は従来の契約エラーを維持する。通常の編集・検査guardは省略しない。
回帰は`rencrow_execution_guard_tests`の有界継続・timeout/terminal回収を対象にする。
当初の環境不足は解消し、Rust 1.95.0 / just / nextestでbuildしローカル配備済み。
対象guardテスト15件、compaction・子設定を含む関連132件が合格。全workspace試験は
無関係なrusty_v8 150.4.0のmacOS prebuilt archiveがHTTP404のためbuild不可。
実モデルを新launcherから起動し、workspace内GOCACHEへの書込成功を確認した。
既存実行中processは旧binaryのまま継続し、更新は次回起動から適用する。

### 複合readコマンドの回数制限

`cat ...; echo ...; sed ...`等の表示用コマンドを含むread batchもreadとして計数する。
表示専用のecho/printf/head/tail/cutを、shell parserが分離した既知commandに限り認識する。
未知commandや実行・置換構文をreadと仮定せず、shellの検査成功判定とsandbox制約は維持する。
同じ回数制限が実tool loopでも働くことを回帰テストで確認した。
