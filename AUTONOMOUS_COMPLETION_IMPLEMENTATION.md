# 自律完了の実装・検証記録

対象仕様: [AUTONOMOUS_COMPLETION_SPEC.md](AUTONOMOUS_COMPLETION_SPEC.md) v1.1。

2026年11月11日現在、実装・実試験中。本ファイルはnative配備完了や全受入試験合格を示さない。

Gatewayのツール契約adapterだけは11月11日に反映済み。実行中・待機中requestが0件であることを確認し、旧binaryを退避してSIGTERMで正常終了。既存 `system/com.rencrow.llm-gateway` が新binaryを起動し、PID 69478・実inode・SHA-256・health・3モデルrouteのaliveを確認した。正本設定は変更していない。証跡は `Tmp/autonomous-completion-20261111/gateway-deploy/deployment.json`。native本体/helper/ren2の切替とは区別する。

## 互換性と切り替え

- `rencrow_autonomous_completion` は既定false。明示的に有効化した親だけが新しい継続制御を利用する。
- 条件・小課題・失敗対策・停止・進捗時計は既存task boundaryに追加するoptionalな `completion_state` へ保存する。別DBや監視daemonは追加しない。
- 境界未確定の最新状態は既存rollout envelopeのhost metadataへ保存する。モデルには短い参照案内を渡し、`task_context_boundary status` で現在の正本を取得できる。
- cold resumeは同一threadのhost metadataだけを復元する。モデルやユーザーが書いた同名のJSONをhost証拠と認めない。
- host検査台帳を保存・復元する。結果不明の検査はpendingのまま保持し、古い成功へ戻さない。証拠採用前に実ファイルを再ハッシュする。
- 旧データの新フィールド欠落は互換読取する。ただし、旧データに存在しない受入証拠は推定しない。
- 新しい状態を旧binaryへ戻した場合、新metadataを理解できないreaderがあるため、自律継続を無効化して原本を保全する。旧readerによる新checkpointへの追記はしない。同じthreadを新旧プロセスから同時に開かない。
- 配備前に本体とcompaction helperを対で退避する。今回は `Tmp/autonomous-completion-20261111/baseline/` に保存し、SHA-256を `baseline-binaries.json` に記録した。
- rollbackはまず対象プロセスを安全な境界で終了し、退避したbinary対と設定を復元する。rolloutやDBを巻き戻さない。新形式threadの継続可否を旧readerで確認できなければ、新binaryで復帰する。

## 検証の区別

| 検証 | 現在の証拠 |
|---|---|
| history型・既存互換 | `just test -p codex-history` 170件成功（11月11日、typed review追加後） |
| 実Qwen試験用CLI | `python3 -m unittest test_autonomous_completion_probe.py` 22件成功。`python3 -m py_compile autonomous_completion_probe.py test_autonomous_completion_probe.py` も成功。`completed_idle_resume` はapp-server stdio復元を別transportとして検査し、実cold resumeは未実施 |
| candidate-5 paired build | `candidate-5/manifest.json` はpaired build成功、7999 source files、build中のsource manifest不変を記録。core binary SHA-256 `2d752bb548a619d2615c2334c881e6617a609ba0d2cec0f44b62b96f83c41040`、compaction helper SHA-256 `7596ad31ade3ba40920e258bd5608410a3a8ec81e6ad3363aab558e90f78362f`。source manifest `candidate-5-source.json` SHA-256 `30633bea52bd0584f14911635204e9e606855543a2287940f5d3b3eaaad11ff3` |
| core型検査 | 中間版の `cargo check` 成功記録はある。candidate-5のpaired release buildも成功している。最終版workspace全体の独立した `cargo check` は未記録であり、paired buildの成功とは区別する |
| cold resume、停止、偽progress、親子制御、TUI初期入力 | 関連188件中187件成功。残るShutdown通知の回帰は修正後、Rust 1.95で該当1件成功。圧縮scopeの安定ID照合と保存・復元回帰も成功。全workspace検査ではない |
| Human入力による再開判定 | Human一致receiptのみ許可し、Automation・receiptなし・別thread・通常発言を拒否する追加1件成功 |
| host board・typed review・世代照合・偽progress回帰 | 関連176件中174件成功。C07結合fixtureの旧accept形式と欠落execution unitを修正後、残る本件1件も成功（計175件）。Code Mode補助binary不足の1件は下記の環境blockerで未検証 |
| 操作説明と診断 | 空の条件resultの項目別エラー、schema説明、Add/Updateを含むpatch例の実parser検証の3件成功 |
| Harness既存契約との互換 | `go test ./internal/cli ./internal/taskcontext ./internal/compaction ./pkg/protocol -run 'Recover\|Recovery\|Boundary\|Receipt' -count=1` の4package成功。実サービス連携の証拠ではない |
| 実Qwen・executor・advisorによる縦断 | 初回baseline、candidate-1とも900秒枠で未完了。合格試行なし。下記参照 |
| C01〜C10・圧縮2回・cold resumeの適用表 | 未完了 |
| 変更前baselineとのtoken・時間比較 | 未実施 |
| 配備・ren2新binaryで継続 | 未実施 |

試験fixtureと設定は `Tmp/autonomous-completion-20261111/` に置く。実Qwen試験は本番認証やDBを複製せず、新規CODEX_HOMEとworkspaceを使用する。rootはQwen3.8、executorとadvisorは設定済みKAT/Qwen3.5 routeを明示し、試験失敗時に別モデルへ黙って切り替えない。ren2の既存worker overrideとは区別する。

実Qwen試験装置の `needs_review` は合格を意味しない。実checkpoint commit、次の実tool行動、成果と検査、禁止反復を原本に照合する。単体試験やcompaction候補だけで仕様完了を宣言しない。

## 実モデル試行の失敗と次策

原本は `Tmp/autonomous-completion-20261111/` 配下。試行中の外部steer・手修正は行っていない。

| 試行 | 観測 | 原因・次策 |
|---|---|---|
| `baseline-C01-1` | KATはAC-1のファイルを作成し、裸のPythonスクリプト検査はexit 0。ただしnative guardは検査receiptに採用せず、境界未確定。親input 97,767 / cached 85,618 / output 3,971 | 検査分類器は標準runnerを要求する。裸スクリプトを無条件採用せず、非採用理由と `python3 -m unittest` の案内を追加。次のfixtureは実unittestに変更 |
| `candidate-1-C01-v2-1` | KATのapply_patch呼出0回。shell heredoc編集が拒否され、未作成ファイルへの検査・読取を反復。900.263秒でtimeout。親input 85,346 / cached 60,673 / output 4,650 | Gatewayがapply_patchを匿名aliasへ変換し、JSON wrapperを要求しつつnativeの「JSONで包まない」説明を残す不整合を確認。原名保持と説明整合をadapterで修正。実モデル改善は未確認 |
| `candidate-1-gatewayfix-C01-v2-1` | 46.566秒で接続失敗。tool呼出0回 | 隔離Gatewayが途中終了。adapter効果の判定対象にしない。次試行ではrootがGatewayプロセスを所有して実行 |
| `candidate-1-gatewayfix-C01-v2-2` | KATがapply_patchで2ファイル作成、unittest成功、親の回収・直接検査・advisor独立レビューまで進行。900.65秒でtimeout、境界commitは0 | 親がbegin/accept/updateの引数を混同。schemaが全action共通のキーを表示する一方、parserはaction別に拒否する不一致を確認。opt-in schemaに操作別oneOfと短い進行契約を追加。全体受入は不合格のまま |

上記tokenは親が報告した累計値で、子の消費を含む総費用ではない。fixtureとbinaryが異なるため効率改善の比較値として扱わない。失敗試行を削除・成功へ置換しない。

candidate-2の試行定義は `candidate-2-trial-plan.json`。同じ2条件・role/effort/guardを保持し、試験枠のみ事前に30分とした。過去の15分試行は失敗として保持し、本番の読取上限や実行制限を緩和しない。

### candidate-2で確認した設計上の問題（修正中）

`candidate-2-C01-v3-1` は1552.411秒で既知の反復を確認し、試験装置側から終了した不合格試行。境界commitは2回だが、A受理後にAC-1の不要な再検査と重複レビューが発生したためC01合格とはしない。`reviewed-result.json` に失敗を保存した。

同試行の各commit直前のhost状態と、commit済みcheckpoint内の `completion_state` は2回とも完全一致した（`compaction-state-comparison.json`）。方式は `task_boundary` の決定論的再構成であり、LLM要約呼出はない。この証拠は状態保持の確認に限り、状態の意味的な正しさや圧縮後行動の成功を示さない。

`candidate-2-C01-v3-1` は実worker実装、親check、実advisor回収、Aの境界commitまで到達した。ただし、計画unitと実行scopeのboardを両方モデルへ入力させる設計により、review unitを過去の受理済みscopeとして扱い `UnverifiedCompletedTask` となった。review結果にはscope用検査receiptと別のhost証拠が必要であり、同じ検査をreview名義で再実行して代用する設計にはしない。

また、`begin(a2, completion_state)` が条件変更で拒否された後、a2名義の検査が受理された。開始scope変更とledger検証の原子性を確認・修正する。呼出拒否なのに内部scopeだけ変わる状態を正常扱いしない。

次版では、opt-inの受入boardをhostの実行事実から生成し、モデルに二重管理を要求しない。実reviewの対象・route・回収結果・親採否を型付きで結び付ける。旧off契約と、未知・失敗・未回収・古い証拠の拒否は維持する。

### candidate-4の実測（不合格・修正して次候補へ）

paired buildは成功し、7999ファイルのsource manifestがbuild前後で一致。binary識別は `candidate-4/manifest.json`。実Qwen試行は `candidate-4-C01-v3-1` に保持している。

- 実送信schemaの全体propertiesに旧 `goal/decisions/tasks/current_task_id/constraints` が残り、action別oneOfでは拒否する不整合を確認。初期登録で旧項目が2回誤送信されたため、ON時の非受理項目をschemaから除去する修正を準備中。OFF時の旧契約は維持する。
- `call_0500cef9f4394d258dcb5643` のplanned review target (`task-a@1`) が、検査済みの証拠と誤認され拒否された。汎用再帰の `canonicalize_completion_evidence` がtask ID/revisionを持つ全objectを証拠扱いしたことが根因。計画参照と実績証拠を分離し、spawn/adoption時の証拠照合は維持する修正を準備中。
- 04:27 JST頃から、現行ren2の製品作業を別途継続。試験へのsteerはしていないが、同じbackendを共有するため、この試行の時間を速度改善の比較値にはしない。製品継続指示は `ren2-product-continue-during-validation.json` に保存。

同じ `review_target` の誤変換が受入後にも発生し、`artifact_ref` を計画参照へ注入してtyped schemaが拒否した。モデルが修正を試みてもhostが同じ変換をするため、1116.045秒で試験装置から終了し、不合格として保存した。実境界commitは1回。`reviewed-result.json` に原因と未確認部分を記録し、raw resultは変更していない。

修正版では、`conditions[].evidence`、`units[].evidence/task_refs`、`blocker.evidence` の型付き証拠だけを正規化する。`review_target` はその対象にしない。planned reviewの未来参照を許可しつつ、偽の実績証拠と受理済みreceiptのないspawnを拒否する回帰、およびON/OFF schema互換の計4件が成功。静的検査も完了（既存の警告あり）。candidate-4の誤canonicalizationとschema不整合はcandidate-5で修正した。candidate-4の失敗・原因・raw結果はそのまま保持する。

### candidate-5のpaired buildとC01試験結果

`candidate-5/manifest.json` によると、core本体とcompaction helperのpaired buildは成功し、7999ファイルのsource manifestがbuild前後で一致した。binary SHA-256は上表の値。source manifest実体は `candidate-5-source.json`（SHA-256 `30633bea52bd0584f14911635204e9e606855543a2287940f5d3b3eaaad11ff3`）。この事実は最終workspace全体の独立`cargo check`実行記録とは別である。

candidate-5には、ON時schemaからparser非対応のlegacy fieldsを外す修正と、型付き証拠配列だけをcanonicalizeしplanned `review_target` を参照のまま保持する修正を含む。candidate-5 paired buildは成功したが、実試験 `candidate-5-C01-v3-1` は不合格として記録した。原本 `result.json` は変更せず、`needs_review` / `interrupted` / native exit `-15` のまま保持し、wrapper PID 80130 へのSIGINT、session 73055 のexit 1を別途照合した。詳細は同試験の `reviewed-result.json` に保存した。

失敗理由は、C01-v3のworkspace限定に対する ordinal 99 の `/tmp/a1.out`・`/tmp/a1.err` redirect、worker/parent間で標準テスト実行者の指示とhost guardが衝突したこと、通常analystレビューがtyped review/adoptionに結び付かなかったこと、task-bのbegin後にB実装・受入がなく試験全体が未完了だったこと。前進として、認識済み `python3 -m unittest -v tests` はtask-a rev1のhost evidence付きで成功し、task-aのacceptとcompaction checkpointがcommitされ、task-bのbeginもhostに記録された。これらは違反・不足と相殺せず、C01全条件の合格を示さない。

次候補C01-v4のfixtureと `candidate-7-trial-plan.json` を準備した。既存の2つのAC、4つのrequired units（task-a/review-a/task-b/review-b）、workspace限定、従来のorchestrator/executor/reviewer model routes、effortと各execution guardを維持する。review unitは初期計画にfuture `review_target` として宣言し、evidenceとは扱わない。driver担当は `review(target_task_id)` が既存review unitをhost bindし、同じV1 reviewer childをwait後 `adopt_review(unit_id,reason)` で採用する正常系integration testの成功を報告した。Astraはその後、既存 `spawn_agent(completion_review)` 互換回帰を指摘したためcandidate-7をreview-blocked/not-readyとした。candidate-7 paired buildはRust/Cargo 1.95.0、locked `fork-deploy`、`-j 2`でexit 0。source 7999件のmanifest SHA-256 `5ee97a91daf193affd037ee6bb588db3be9c3838373abe366ac312a729b74e6c` はbuild前後不変。core SHA-256 `83741b52f07e36746b7dd35d7a7745efb3e4ca8eb217902f5d2bae71d84285bd`、helper SHA-256 `74181cfd99c0b5ba6ad0fb447f41ca59e94a6ed7f6e06d4bd3aec2ef7271abfd`。coreの `--version`/`--help` とhelperの `--help` はexit 0。helperの `--version` は未対応optionでexit 2（helperはversion optionを公開していない）。artifactは `Tmp/autonomous-completion-20261111/candidate-7/` に隔離保存した。build後にrootが許可したlegacy互換修正が `codex-rs/core/src/session/completion_children.rs` に入った。candidate-7のbuild中sourceは不変だが、これはbuild時snapshotであり現在sourceと異なるため試験に使わない。修正後sourceをfreezeしcandidate-8をRust/Cargo 1.95.0のlocked `fork-deploy -j 2`でpaired buildしてexit 0。7999 source filesのmanifest SHA-256 `30e8da578bcb495a40707fd3e84732c33cfa68a8550da1bd553403387b22deec` はbuild中不変。candidate-7との差分は `codex-rs/core/src/session/completion_children.rs` のみ（candidate-7 SHA `c2532a7e7719c152dfc7623e95a00b3315a8f3769c6b1dc3c2669e4366a53314` → candidate-8 source SHA `5286209dd901dbecec7f3c3bdfa1da26e06f0e5c3db11c27b0fc60eb73a151f0`）。candidate-8 core SHA `ae1111f039eb8bc281419fccae1ea0f3c032eb538d0e7b80e024e5bd6ea1457c`、helper SHA `852e6e9378312f12e8d7353eb1d5a6bff0ccdfb05a19b36d8430c1a7d5cf8794`。coreのversion/help、helperのhelpはexit 0（helper version optionなし、exit 2）。artifactは `Tmp/autonomous-completion-20261111/candidate-8/`。candidate-8はroot/Astraの限定レビューとGateway readiness確認後、fresh workspace/homeでC01-v4を実行した。30分の時間枠は事前固定し、backend負荷を記録して速度比較には使っていない。

### candidate-8の実測（不合格）

`candidate-8-C01-v4-1/result.json` は `timeout / -15 / 1800.494秒`、境界commitは1回。raw結果は不変（SHA-256 `faa00ee7ffe00974b5bb444409abdc75e09ba6f3e7515761c9e26c9f6ee0ad3c`）。独立レビューと `reviewed-result.json` は全体をfailedと判定した。配備はしていない。

Aの親検査・受入・圧縮、typed reviewerの起動と同一generationのwait回収までは成立した。しかし `close_agent` が回収済みCompletedの証拠まで無条件に失効させたため、続く `adopt_review` が拒否された。親rolloutのordinal153にはwait証拠があり、close後の162で消え、adopt call `call_1a1405fe90b043a6b401cb5f` が171で拒否される。最終状態はA done/AC-1 satisfied、review-A running、B/review-B planned、AC-2 open。修正対象はcloseの証拠失効であり、adoptの検証を弱めない。

外部steerは0。子によるAC-2先行実装は親が受入前に修正し、commit後のA再実装・再検査は確認されなかった。workspace外への書込みは確認されず、親によるmanifestとreviewerによるログのworkspace外参照はあった。圧縮要約と最初の親通常入力 `provider-inputs/00026.json` の一致を確認したが、S0全項目照合・cold resume・全case matrixの合格には代用しない。

次候補では、回収済Completedのclose後採用を可能にし、send_input/resume後の旧generation採用拒否を維持する回帰を追加する。併せて、子作成後の初回送信失敗でIDを失う経路と、真の不明起動のhost保存・重送抑止を修正中。未解消checkの識別情報をエラーへ表示する修正、plain-text compaction summaryが通常turnの出力schemaを継承しない修正も限定検査中。本番HTTP 400との因果が未確定な点は保持する。


## 現時点の実装上の制限

- 利用者による目的変更を受けた条件revisionの更新経路は未実装。現在はモデルによる条件内容の変更を拒否する。既存の停止・再開許可を条件弱化の許可へ転用しない。
- typed reviewのhost証拠採用はmulti-agent V1のみ。V2はこの追加オプションを明示拒否し、通常のV2子制御とは区別する。
- 外部blockerの自動復帰確認は、設定済みRenCrow providerのモデルreadinessに限定。他サービスや資格情報の回復は未対応。
- app-server全schema生成は既存のByteRange/ByteRange2衝突で未完了。実験的resumeフィールドの局所検査と全schema生成の成否を区別する。
- 関連filterに含まれた既存 `code_mode_records_mcp_completion_even_when_result_is_not_printed` はCode Mode hostが未配置で失敗。補助binary `test_stdio_server` はbuild済みだが、`codex-code-mode-host` のbuildは `rusty_v8 v150.4.0` のmacOS aarch64配布archiveがHTTP 404で失敗した。この試験は未検証として残し、自律完了の合格件数に含めない。推測した依存版への変更やV8全体のsource buildは行っていない。
- 実モデル縦断・反復試験・cold resume・配備が未完了のため、外部監督なしで製品完成できることはまだ確認できていない。


## 検査受入と終了判定の整合修正 実装仕様1

2026年11月11日。上位仕様12節の修正1を先に実装し、その検査・レビュー後に修正2へ進む。以下は実装前に固定した契約であり、成功記録ではない。

1. `task_check.rs` の既存TaskCheckLedgerにhost生成の検査義務identityと未解決attemptを保持する。scopeは既存ActiveTaskScopeのroot/task/revision/begin識別に合わせ、cwdは解決済みnative path、artifact path集合は順序に依存しない。check identityは既存保守classifierを通過したcommandだけから生成し、意味を証明できない表記は元commandを保持する。任意のモデル指定IDで義務を解除しない。
2. `unified_exec/exec_command.rs` のpreflightでscope・identityを確定し、実行開始の既存ledger登録へ渡す。preflight拒否は従来cleanupで開始予約を取消し、実行済み義務を作らない。既存process completionから成功/失敗/変更/未確定を更新する。同じ義務の終端済み失敗に対する後続の安定成功だけを解決として扱い、原attemptの記録は残す。
3. `task_context_boundary` acceptは既存latest receipt・成果鮮度・原call照合に加え、そのscopeの未解決義務がないことを要求する。最終guardは同じledgerの対象rootの未解決義務を確認する。新方式のtask検査をturn-local command文字列台帳だけで二重判定しない。task_evidenceなし/OFFのguardを保持する。
4. 既存TaskCheckSnapshot/host completion snapshotへ義務を互換的に保存する。latest/taskだけで過去の未解決失敗を落とさない。旧形式は読取可能にし、新しいidentityがない証拠に等価性を推定しない。新旧混在時の未確定はfail-closedで保持する。永続化の失敗を検査成功と扱わない。snapshotは未解決義務のattempt、task/revisionの最新、現在の条件・reviewが参照するreceipt、その解決リンク先を保持し、解決済みかつ参照されない過去attemptは原本に残す。既存256件上限を増やさず、解決済み検査の反復だけでsnapshotが増え続けない。
5. 正規化は引用・空白の保守的なtoken化と、明確に識別したPython `-m unittest` のoptionとして解釈される位置の単独 `-v` / `--verbose` の除去だけを許す。`--`以降の対象名、他optionの値、判別不能な配置は除去しない。環境代入、Python executable/option、対象、他optionは保持する。曖昧なshell構文は文字列一致へ戻す。広い検査と狭い検査を同じidentityにしない。
6. TDD順序: (a)現コードでaccept/終了の不一致と保存復元の失敗を再現するテストを追加しredログを保存、(b)最小コード修正、(c)同等検査の回復、非同等検査の拒否、scope分離、pending保持、snapshot互換のgreenログを保存、(d)親が差分と実結果を確認。`just test`、Rust 1.95.0、既存dev-small targetを使う。全workspace試験と追加機能は行わない。

テスト・buildの実行者はLuna一人とし、command/cwd/toolchain/終了値を `Tmp/autonomous-completion-20261111/consistency-repair/` に保存する。最終paired build・再起動・E2Eは修正2のTDD完了後に行う。既存dirty差分、原rollout、ledger、ユーザーstop、モデル経路、権限を保持する。


### 修正1の検証結果

修正前は既存TaskCheckLedger APIで失敗attemptが後続成功によりsnapshotから落ちることを1件の挙動失敗として確認した。記録は `consistency-repair/issue1-red-raw-junit.xml`。修正後はRust 1.95.0、`just test -p codex-core --cargo-profile dev-small` の限定filterで20/20成功、exit 0。実registryの非同等検査拒否・同等検査回復・受入、別の完了契約fixtureの `Completed(Some(_))` まで確認した。コードはAstraレビューと親の差分/原ログ確認を経て採用した。証跡は `Tmp/autonomous-completion-20261111/consistency-repair/issue1-adoption.json` と `issue1-green-final20.*`。

途中のコンパイルミスはTDDのredとして数えない。fixtureのcat検査、集合順序比較、意図的open状態でCompletedを期待した誤りも訂正し、失敗原本を保持した。今回差分は9ファイル、+1286/-47で、ledger/互換保存・guard/受入配線・回帰と実統合の一まとまりとして確認した。無関係なformatter差分は含めない。paired build・配置・再起動・実モデルE2Eはまだ実施していない。


## 完了後の成果変更と再開の整合修正 実装仕様2

修正1を20/20の実検査とコードレビューで採用した後に確定。これは実装前の仕様であり成功記録ではない。

1. `Session::set_rencrow_completion_state`内の既存receipt/task/revision/fingerprint/現物hash検査を、副作用なしの共通処理へ抽出する。既存setterとhost freshness reconcileで同じ判定を使う。setter全体を再呼出ししない。review_evidenceのモデル入力補正、受入、進捗計測を照合の副作用として走らせない。
2. Sessionのhost reconcileは契約と参照証拠を取得し、SessionState lock外で現物hashを照合する。lock再取得時に対象契約/証拠が照合開始時と同じことを確認し、並行更新があれば古い照合結果で上書きせず最新状態で再評価する。satisfiedの影響条件だけopen、元証拠保持、具体的unmet_reasonと新ID再検証のnext_actionを設定する。他のstateと時計は維持する。
3. `agent/autonomous_completion.rs` cold-resume入口のpending判定前と、`session/turn.rs`の自律継続/完了判定前でreconcileを呼ぶ。configがONのときだけ有効。無変更ならpersistしない。変更時は既存のhost snapshotに保存し、保存失敗をidle/完了成功にしない。既存pause/blocker/未知の子とCLIの保護を維持する。read-only predicateへ隠れた書込みを追加せず、reconcileとpending照会を区別する。正常な完了契約でのsession shutdownは、条件/unitの完了、pause/blockerなし、active turnなし、未回収child・task-check義務・CLI/agent workなしを明示確認した場合だけ`Interrupted` abort/pauseを省略し、残りのcleanupは続ける。`pending == false`単独では判定しない。`abort_all_tasks`と明示interrupt APIのpause意味は変更しない。
4. 新しい保存schemaは追加しない。修正1のtask ledgerとtyped参照保持をそのまま使う。過去Done unitの巻戻し、旧review launch削除、同taskの再revision受入処理は追加しない。next_actionを通じて既存updateで新task IDの検証unitと必要reviewを作り、begin/check/acceptへ進む。hostが架空のreceiptやモデルの判断を生成しない。
5. TDDは既存completion_persistence/cold-resume fixtureを使い、completed保存→実ファイル変更→restore→再開判断のredを先に実行・生ログ保存する。型や構文エラーをredとしない。実ThreadManager再開fixtureの準備が過大になる場合は、privateな照合入口を無挙動stubで用意し、実restore→同入口→pending/stateの同一テストでred/greenを確認する。これは単体挙動の証拠であり、実再開配線と最終E2Eの代用にしない。その後、共通照合と配線の最小実装、無変更idle・影響条件のみopen・停止保持・新ID再検証受入のgreenを確認する。未達を意図するfixtureと全達成の正常終了fixtureを混同しない。
6. `just test`、Rust 1.95.0、既存`codex-rs/target/dev-small`を使用。1番の20件も最後の関連回帰として一度確認する。テストプロセス自身のreturncodeとstdout/stderrをattempt別保存する。編集前backupは`consistency-repair/issue2-before/`、検証記録は同directory親へ保存する。宣言/型/呼出側/所有権/fixture応答順を編集前と検査前に確認する。

### shutdown pause lifecycle補足

修正2の実E2Eで、完了契約を持つ通常CLIが正常終了した際、CLI teardownの`Op::Shutdown`から`shutdown_session_runtime`が`abort_all_tasks(Interrupted)`を呼び、active turnの有無を見る前に契約をpauseする経路を確認した。これはtransport/session teardownをユーザー停止と混同する。

1. 最小変更は`shutdown_session_runtime`だけで、正常idle完了契約の厳密な条件が満たされる場合に限り`abort_all_tasks(Interrupted)`を呼ばないこと。公開abort API/`TurnAbortReason`、explicit `interrupt_task`、active shutdownのpause保護、他cleanupは変更しない。
2. 条件判定は既存Session状態を使い、自律完了ON、契約あり・全condition satisfied/waived・全unit done/cancelled、pause/blockerなし、active turnなし、未回収child/未解決task-check/CLI/agent runtime workなしを確認する。`autonomous_completion_pending() == false`だけでは完了契約と停止理由を区別できないため不十分。
3. TDD redは、host側にcompleted contractを設定したmodel-free sessionで実`shutdown_session_runtime`/`Op::Shutdown`後にpausedがfalseである期待を置く。修正前のpause=trueをruntime assertion failureとして保存する。未完了条件/unit、未回収child/CLI/agent/task-check、active turn、既存pause/blocker、config OFF、およびexplicit interruptが引き続きpauseする対照をgreenで確認する。source edit後に`just fmt`、core限定`just fix`、Rust 1.95.0 `just test -p codex-core --cargo-profile dev-small`の変更影響filterを行い、検査前後のsource/型/API/所有権を確認する。paired build/配置はrootレビュー後とし、ren2/childは停止しない。

### 将来のidle transport teardown

修正2の実E2E後に加えた将来再発防止仕様。正常なidle shutdownの例外条件を「完了契約」に限定せず、shutdown時のpause判定と実runtime作業の有無を分ける。

1. `shutdown_session_runtime`だけを変更する。自律完了ON、`active_turn == None`、`autonomous_completion_has_unresolved_runtime_work() == false`なら、`abort_all_tasks(Interrupted)`を呼ばず既存contract、active task scope、task-check ledgerを保持して通常cleanupを実行する。未完condition/unit、active scope、未解決task-check obligationは、それだけでは実行中runtime workを意味しない。
2. ON/OFFやactive turn、unresolved/unknown CLI/child runtime workの分岐はfail-closedとする。既存pause/blockerを解除しない。active turnありの通常shutdown、runtime workあり/状態不明、設定OFFは従来のInterrupted abort/pauseを維持する。公開API、`TurnAbortReason`、`interrupt_task`、明示interruptの意味は変更しない。
3. 変更前に未完condition/unitとactive scopeを持つidle contractで実`shutdown_session_runtime`のpause=trueをruntime redとして記録する。greenではpause=falseに加え、契約・scope・ledgerが保持されることを検査する。explicit interrupt、active turn、unresolved/unknown runtime CLI、OFF設定はpauseする対照とする。Rust 1.95.0のdev-smallで変更影響filterを実行し、raw stdout/stderr/return codeとsource hashesを保存する。build/deployは親採用後。

### maintenance 配備 pause の限定 offline 修復実装仕様

詳細なTDD/受入条件と根拠は`Tmp/autonomous-completion-20261111/consistency-repair/maintenance-resume-design.md`と`maintenance-resume-evidence.json`。実装・applyは未実施であり、実行時にはcanonical tipと証拠を再検証する。

1. 既存`rencrow-compaction` CLIへ`repair-maintenance-pause --codex-home ... --thread ... --request ... [--apply]`を追加する。defaultはread-only dry-run。1つの採用済みlocal CODEX_HOME/thread/rootに限定し、model/TUI/app-server/session/childを起動しない。requestは64KiB以下、`deny_unknown_fields`、regular file・owner/permissions・exact UUID・thread/rootを検証する。証拠kindは既存deployment result/preflight/ticket/pre-deploy snapshot/facts/deployer sourceの固定6種だけ許可する。
2. helperはLocalThreadStoreのexisting `resume_thread` でcanonical writer lockを取得し、読み直した履歴tip/hash・generation・current snapshot hash・request evidenceをapply直前に再照合する。active writer、stale tip、unknown/missing history、別thread、active/unknown runtimeのいずれかを検知したら変更なしで拒否する。既存`append_items`→`flush_thread`→`shutdown_thread`を使い、raw append/direct SQLは禁止。canonical rolloutと通常LocalThreadStore projectionからsnapshot/receiptを読戻して一致を確認する。
3. coreに小さな共有pure validation surfaceを置き、通常restoreとCLI repairが同じsnapshot schema、accepted task-boundary receipts、task-check ledger、scope/patch/childのvalidationを使う。snapshotへoptional host-only maintenance receiptを加え、request digest、thread/root、generation before/after、修復前/current snapshot digest、shutdown before/paused pair、許容scope mismatch digest集合、evidence digestを保存する。
4. restore時はhost-only receiptを事前に収集・検証し、receiptが明記した旧snapshot digestに一致し、active scope/root不一致だけが失敗理由のrecordだけをskipする。指定scope以外の失敗は元どおりfail-closed。client-authored/model本文のmarker、Receipt欠落/不正、別thread/root、generation不整合、同じgenerationでのpause解除は無効。
   repair receiptの受理時は、shutdown前後digestを同じcanonical履歴のhost-authored snapshotへ解決する。両snapshotはthread/root/revision/generationと全payloadが一致し、`state.paused`だけがfalseからtrueになった組でなければならない。receiptの修復元snapshotもcanonical履歴から解決し、生成snapshotが既存builderの出力と完全一致することを確認する。pause後からrepair receiptまでに`Interrupted` eventがあればreceiptを拒否してpauseを維持する。shared inspect/restore validatorはaccepted task boundaryのrootもsnapshot rootと照合する。
5. applyはlatest stateの`paused=false`とgeneration+1だけを行い、root、completion contract、AC/evidence、scope、check ledger、children、historyと進捗時計を保つ。failure/結果不明はnonzeroとし通常resumeへ進めない。apply済みrequest digestのreceiptが最新で、その後のpause/stopがなければidempotent `already_applied`、二重append/generation増分なし。
6. TDDはvalid maintenance fixtureでdry-run read-only、apply後restore、canonical+SQLite readback一致を検査し、拒否fixtureはactive writer、stale snapshot/tip/evidence、別thread/root、user stop/Interrupted、別pause reason、scope hash余剰/欠落、偽host/client receipt、snapshot size bound、append/flush failureを対象にする。追加API/daemon/config/TUI/input-intake経路は作らない。idle shutdown変更と合流した関連filter/fmt/fixは一度行い、Rust 1.95.0/dev-small raw resultを保存する。paired build/deployは親のレビューと採用後。

### checkpoint commit後のscope closure snapshot補足

実cold-resumeで、accepted task boundaryのcheckpoint commit後に最新host snapshotが`active_scope`を保持し、completed契約でもcold-idle経路を拒否する原因を確認した。`commit_prepared_task_boundary`はcheckpoint commit、completion state更新、unit accept時計の保存後に`active_task_scope = None`をmemory内で反映するが、そのclosure状態を再保存していなかった。

1. 同じ既存boundary commit入口で、pending boundary/active scope/patch attemptを閉じ、`task_boundary_committed_turn`を設定した後、lockを解放して既存`persist_autonomous_completion_snapshot`を呼ぶ。ON時はこのscope-closure snapshot成功後だけ`Ok(true)`を返す。新schema/API/commit経路は追加しない。
2. snapshot保存失敗時はcheckpointやaccepted receiptを再実行・巻戻ししない。完了状態をpauseし、`checkpoint committed, scope-closure snapshot persistence failed`を含むErrで呼出側へ伝える。同acceptの二重commitを避ける既存turn markerを保持する。
3. scope closure以外のcontract、receipt、ledger、root、時計、Done unitを変更しない。OFF時は新しいcompletion snapshotを追加しない。TDDは`task_evidence_recovery_accept_completes_final_guard`の実accept/checkpoint commit後に最新host snapshotの`active_scope == null`を検査し、修正前にruntime assertion red、修正後に同一経路greenを確認する。cold-idleの実E2E確認はcandidate-12の実モデル経路で別途行う。

実装はLuna、設計と差分/検査の採用は親、難航箇所の助言はAstra。2番の採用後だけpaired buildへ進み、その後に既存の同一thread再起動、E2Eを実行する。原本・既存dirty差分・モデル経路・権限・Gatewayを保持する。


### 修正2の検証結果

有効なfingerprint・accepted V3 task境界・host検査snapshotを使い、実restore後の無変更fresh対照から成果変更でopenへ戻る挙動について、無挙動入口でred（runtime failure）、実装でgreenを確認した。初期の仮fingerprint fixtureは変更前から不正であり、成果変更の再現証拠として不採用、原ログは保持した。再現証拠は `consistency-repair/issue2-restore-red-valid.*`、greenは `issue2-restore-green-attempt2.*`。

最終限定filterはRust 1.95.0・dev-smallで21/21成功、exit 0。保存復元・無変更/no-extra-persist・pause/blocker/時計/無関係状態保持、新IDの実検査/accept/条件再達成、修正1の20件を含む。Astraコード承認と親の原ログ/実統合差分確認を経て採用した。`just fmt` と `just fix -p codex-core` は終了0、対象外のauto-fix差分は直前backupから保全した。採用記録は `consistency-repair/issue2-adoption.json`、実検査は `issue2-final-related.*`。

既存test_codexのresumeは自動wake opt-inを渡さないため、実ThreadManagerのcompletion-enabled cold resumeはこの結果に含まない。再ビルド・同一thread再起動後の実モデルE2Eで必ず別確認する。部分検証を全仕様完了としない。

scope closure snapshotのTDD検証記録（2026年11月11日）:

- 実boundary accept後の最新host snapshotで`active_scope == null`を期待する統合fixtureを追加。修正前はコンパイル後の実行時assertion failureでredとなり、snapshotにactive scopeが残ることを記録した。`scope-persist-red-attempt1.*`。
- 修正後、同じaccept経路のfocused testはgreen。関連35件filterもRust 1.95.0/dev-smallで35/35、exit 0。実checkpointを含むcold-resume fixtureでtask-boundaryの`Compacted` checkpointを確認し、scope closure後のsnapshot assertionを通過した。`scope-persist-green-attempt1.*`、`scope-persist-final-related-v4.*`。
- 関連filterの初回は旧fixtureのevidence commandがclassifierに拒否されcheckpointが生成されず失敗した。classifierを確認し、既存許可形式の`python3 -B -m unittest -v test_task_boundary_artifact`へ変更した。明示的artifact assertion、patched file内容、request text、artifact名の期待をまとめて更新した。中間runでは旧artifact filenameのassertionが残り失敗したため、fixture全体を再点検して修正。これらのfixture失敗は製品redとして数えず、ログ`scope-persist-final-related*.{stdout.log,stderr.log,exit.json}`を保持した。
- scope closure snapshotの永続化失敗分岐を故意に注入する追加testはこの検査に含めない。cold-idleの実モデルE2Eも別工程で確認する。

後処理と最終source照合:

- `just fmt` はexit 0。`just fix -p codex-core` はRust 1.95.0でexit 0。実行前にrepo rootからtracked+untracked/non-ignored 8,538 files（86,260,285 bytes）をhash付きでbackupした。`scope-persist-pre-format-files-meta.json` と全件JSONLを参照。
- fmt/fix後の全件比較でfixが今回の範囲外4ファイル（`rencrow_execution_guard.rs`, `task_check_tests.rs`, `tools/handlers/task_context_boundary.rs`, `tools/handlers/unified_exec/exec_command.rs`）へ追加した差分を検出した。4件とも同じ実行直前backupから復元し、再比較で8538件中の差分は対象integration test assertionのrustfmt整形1件だけと確認した。production boundary修正とboundary fixtureはfix前のgreen済み内容を保持する。
- 35件filterはfmt/fix前に終了0で実行済み。formatter/fix後のtest再実行はしていない。fixtureは実検査前に全体確認が足りず旧artifact filename assertionを1箇所残したため、旧command・artifact名・expected content・request textの全参照を再点検して一括修正した。中間失敗はfixture期待の更新漏れとして記録し、製品redには数えない。

## V3 task checkpoint owner proof（candidate-14 preflight blocker）

### CLI契約

追加commandは`rencrow-compaction task-checkpoint --codex-home <path> --thread <uuid> --root-task-id <expected-host-root>`。CLIはstdoutへspec §13記載のstrict version-1 proofを出し、それ以外の成功形式を持たない。rootはnative thread-bound snapshotの`thread_id`と`root_task_id`から渡す。監督台帳のtask IDをrootの代わりに使わない。PythonはproofのJSON値型（boolはintegerとして受けない）、path、thread/root、rollout full-file SHA、tip、checkpoint ordinal/digest、build manifestのhelper hashを照合する。

### 所有権検査順序

1. CODEX_HOMEをcanonicalizeし、既存thread resolverで指定thread自身のrollout pathを得る。canonical raw rowsを読み、0-based persisted ordinal（SessionMeta=0）の連続性、thread header、parse errorなし、全file SHAとhistory tipを記録する。
2. raw `Compacted` rowsからV3 task-checkpoint candidateをordinal順に選び、owner transaction処理前にlatest raw candidateを固定する。選択対象をolder valid rowへ差し替えない。
3. selected raw rowのordinal/digestとrootをexactly bindし、既存`codex_history::compaction_transaction::committed_items`へbounded transaction windowを渡す。戻りの同じtargetにsynthetic `committed_transaction_hash`が存在し、対象windowが完全に採用されたことを確認する。full rolloutも同じ既存ownerで走査し、transaction integrity errorを拒否する。
4. targetの正規化済み`replacement_history`へ既存`codex_core::compact::latest_task_state_in_history`を適用する薄いcore wrapperで、V3 parserが選択した`task_boundary_index`/`task_state`を返す。typed `StoredTaskState`のthread/root/closed-task/revisionとV3 summary hashをproofへ移す。新しいtransaction validatorやtask-state parserを実装しない。
5. 成功直前に同じcanonical rolloutの全bytes、tip ordinal/digest、target ordinal/digestを読み直して初期値と一致させる。変化、target消失、transaction未commit、latest candidate parse失敗、root不一致なら失敗する。

### 最小TDD検査

- valid committed V3 capacity checkpointから埋め込まれたV3 task boundaryをproofへ出す。
- latest raw candidateのcommit marker欠落/不正では直前のolder committed targetへfallbackせず拒否する。
- wrong thread/root、target raw row digest mismatch、history tip mismatch、outer transaction hash mismatch、embedded V3 summary/root/task mismatchを拒否する。
- CLI入口がown-thread rolloutだけを読むこと、実行中にraw bytesが変化した場合にsuccess proofを出さないことを隔離fixtureで確認する。
- repair-maintenance-pauseのsnapshot inspectionは通常restoreと同様に既存`committed_items`で正規化してから既存task-state照合を行う。raw履歴からのexact tip/time/pair証明は元rowを保持し別途確認する。

## Planned review dependencyによるaccept unit解決

### 原因と限定動作

実host projectionでは `current_unit` がnullでも、実行unitはRunning、同じtask/revisionを `review_target` に持つreview unitはPlannedで、その `depends_on` が実行unitを示す状態がある。既存のaccept対応はtask ID・current Execution unit・task_refsだけで実行unitを探すため、この正規計画を解決できずacceptを拒否する。

既存lookupが失敗した場合だけ、current unitがあればそれがexact task/revisionのPlanned Reviewであることを要求する。current unitがnullならexact targetのPlanned Reviewが一つだけであることを要求する。Reviewの依存は単一かつ一意のExecution unit IDに限り、対象Execution unitはPlannedまたはRunningとする。選択conditionはExecution unitの既存condition_refsの部分集合に限定する。空の選択ではconditionを変更しない。

accept receiptと選択conditionだけをExecution unitへ反映する。Review/current unit/planは書き換えず、複数候補・target/revision不一致・非Review current unit・依存の欠落/重複/異種/完了済みを拒否する。既存のreceipt、artifact freshness、task-check、root、active scope、unresolved work guardは維持し、tool引数/schemaは増やさない。

### 検査

現projectionと同形の `execAC1` Running + `reviewAC1` Planned (`review_target=sp-newAC1@1`, `depends_on=[execAC1]`) fixtureで、現行acceptが「matching execution unitなし」と拒否するbehavioral redを先に記録する。修正後はexecution receiptと選択ACだけが更新され、review/current unitと他条件が不変であることを確認する。曖昧target、異なるrevision/current unit、複数/不正dependency、invalid execution statusは限定negativeとして拒否を確認する。Rust 1.95.0/dev-smallの対象handler testを使い、構文/型/fixture不備をredとして数えない。

### 同一task revisionのfresh receipt canonicalization

autonomous child acceptでは、既存のunit選択と `apply_host_acceptance` の後、BoundaryCandidate validation前に新host `AcceptedTaskReceipt` を最新task receipt mapへ反映し、Session setterと共通の `canonicalize_completion_state_evidence` を適用する。対象receiptはactive task ID/revision、accept result、検証済みartifact fingerprintから作る。既存のaccepted receipt mapは他taskのcanonical refsを解決するため保持する。

共通canonicalizerの対象はcondition evidence、unit evidence/task_refs、blocker evidence。review-specific evidence/target、task/condition定義、status、他task revisionは触れない。新旧receiptを含むsame-revision integration testで、同じtask@revision・同じartifactの再accept後にcheckpointがcommitされ、persisted completion evidenceが最新receiptと一致することを検証する。

## 人間入力による再開とレビュー期限（2026-11-12）

通常TUIのアイドル時 `StartOrSteer` も、`StartIfIdle` と共通の明示再開判定を使う。有効なHuman intake receipt、本文・client ID・thread照合、対象source、機能ONを要求する。active turnへのsteerとautomation入力では解除しない。再開snapshotの保存失敗時はpauseを維持し、新規taskを開始しない。

新規のhost管理レビューは、spawn予約時に10分後の絶対期限 `deadline_unix_ms` を保存し、同じspawn call・child・turn generationの証拠へ引き継ぐ。復元・計画更新で期限を延長せず、未回収の期限付きレビューのstatusもhostが保持する。旧snapshotで期限が無いレビューには期限を後付けせず、従来のstatus更新を維持する。

期限は既存120秒observerと `wait_agent` の待機境界で検査する。親のpause・自律継続OFFは解除せず、保存済み期限だけを観測する。取消しは親のregistry、read-only設定、childの親・role・正確なgenerationを照合し、tool待機・background terminal・code cellが無いと確認できた場合に限る。tool futureの登録からdropまでRAIIで追跡し、inventory確認中の新規dispatchはepochの差で検出する。

取消し後も、同じgenerationの `ReviewEnded` statusと永続化済み終了イベントを確認するまで失敗確定にしない。安全性・終了が不明なら、host証拠にescalationと固定長の観測fingerprintを保存し、同条件の再送・連続waitを止める。実際のterminal結果は回収でき、別generation・通常worker・hostの結果通知配送は妨げない。汎用followupの拒否は入力依頼の入口に置き、共通の通知transportには置かない。

Rust 1.95.0の変更Core機能とhistory crateの限定検証は217/217成功。Human再開の永続化・失敗時停止、期限とhost所有権の保持、実checkpointとdisk snapshotからの復元、親に属する実childの取消しと終了証拠、worker保護、世代別waitを含む。全workspace検査とC01-v6総合E2Eの合格を意味しない。実機配置・同一thread再開の証拠は私有の `Tmp/ren2-supervision/periodic/human-resume-20261112/` に分けて保持する。
