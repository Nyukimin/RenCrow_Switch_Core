# 自律完了統合仕様 v1.1

- 作成日: 2026-11-10。今回の成果は本仕様1ファイル。コード変更・配備・ren2への適用は別工程。
- 状態: 提案仕様。実装済み・配備済みを意味しない。
- 対象: RenCrow Switch Core の上位driverと、既存RenCrow HarnessのTask/Run連携。
- 目的: 外部監督のsteerや小課題補給がなくても、Qwen orchestratorが有限の製品受入条件を満たすまで計画・実行・検証・統合を進める。
- 本仕様は工程の前進を成果とみなすcompletion-contract、および単なるprogress/diff有無を完了進捗の代理にする判断を置き換える。
- 既存の権限、停止、保護対象、境界受理、実行中job保護、single-writer等の安全制約は維持する。権限・採用範囲を拡大しない。

## 1. 参照と設計根拠

- [Switch Core共通・fork規則](AGENTS.md) と [FORK_RULES.md](FORK_RULES.md) に従う。
- [TASK_CONTEXT_COMPACTION_SPEC.md](TASK_CONTEXT_COMPACTION_SPEC.md) のtask boundary、snapshot/revision、host確定receipt、原本不変、圧縮・再開契約を再利用する。
- Switchの既存Task boundary/context receiptと、HarnessのTask/Run/store・verification receiptを再利用する。第二のtask DB、daemon、汎用agent framework、別schedulerは作らない。
- 調査上の失敗例: 工程語だけのcompletion-contractでは「進める」こと自体が完了になりうる。progress検査の新checkやdiff検査だけでloopがresetされると、presentation未確定のままCLI/test追加を続けても製品目的は未達のまま残る。
- Switch境界のroot scope begin後にchild beginが拒否される問題は、rootの計画状態と実行scopeを分離して解消する。rootが未完のまま小課題を開始・受理・終了できることを必須とし、既存の誤ったscope操作をモデルへの注意だけで回避しない。
- 上記は仕様の動機と再利用先を示す。過去の監督実装を本番依存にしたり、挙動を推定したりしない。

## 2. 完了契約

### 2.1 受入条件

- 開始時に有限個の受入条件を `AC-<連番>` として登録する。各条件は利用者が観察可能な製品結果、対象範囲、検証方法、合格基準を持つ。
- 条件ごとにrevision、状態 (`open`, `satisfied`, `rejected`, `waived`)、根拠参照、未達理由を保存する。IDは不変、内容変更はrevision増分で表す。
- `waived` は既存権限で waiver を決定できる主体だけが選べる。モデルの自己申告や進捗数値は受入条件を免除しない。
- 受入条件を後から黙って削除・弱化しない。利用者の明示した目的変更は新revisionとして境界記録し、履歴と根拠を保つ。
- 全条件が検証済み `satisfied` または権限ある `waived` で、未解決の必須依存・子job・統合・検査がなく、完了reportが各条件の証跡を指すときだけ製品完成とする。
- 「計画した」「変更した」「テストを実行した」「差分が増えた」「子が成功した」は、それ自体では条件達成にならない。
- 受入証拠は検査対象の成果revisionと実行環境へ結ぶ。対象の変更で証拠が古くなった条件は再検証対象に戻し、無関係な変更で全条件を無効化しない。正常な利用経路が必要な条件を、期待どおり拒否・blockedになった試験で満たしてはならない。

### 2.2 小課題と依存

- Qwenは各open条件を、実装・調査・検査・レビュー・統合の必要な小課題へ分解する。各課題に不変ID、目的、担当route種別、親条件/課題、依存、状態、成果/検査参照を付ける。
- 状態は少なくとも `planned`, `ready`, `running`, `blocked`, `done`, `failed`, `cancelled` とし、条件状態と混同しない。
- 依存はDAGで、存在しないID・循環・状態と矛盾する依存を受理しない。親子関係と依存関係は別に記録する。
- 課題作成、実TaskID/RunIDの割当、開始・終端、receipt照合、境界ID、revision、receipt ID等の機械的情報はruntime/hostが既存記録から確定する。モデルにIDやhashの手計算を要求しない。
- Qwenは成果の意味、採否、次の作業を決める。独立reviewerは採用可否に関する判断根拠を返すが、ユーザーの権限やhostの実行事実を上書きしない。

## 3. 実行主体と責任境界

- 主体はQwen orchestratorと2種類のsubagent route、すなわちexecutorとadvisor/reviewer。ここでいう2種類はroute種別であり、同時人数の上限を定めない。
- Qwenは目標分解、設計選択、割当、証拠の意味評価、統合、停止/継続を所有する。小さな通常設計判断をユーザーへ差し戻さない。
- Executorは割当課題を実施し、再現可能な成果・変更範囲・検査結果を返す。担当外の受入条件や権限を変更しない。
- Advisor/reviewerは設計難航時の代案、独立レビュー、見落とし、検査妥当性を返す。決定権はQwenに残る。
- モデル版、route実装、provider名を仕様で固定しない。既存のユーザー指定に従う設定済みrouteを使用し、実際に起動したrouteとmodel識別情報をreceiptへ記録する。利用不能時に別モデルへ黙って切り替えない。
- Switch上位driverが課題計画、委譲、回収、再開、停滞時plan reviewを担当する。Harnessは既存Task/Runの実行状態、証跡、検証、保存を供給し、agent割当や上位計画を所有しない。
- Harnessの既存recover経路は、callerが所有する子のterminal failed/incompleteで、resume可能なgenerationが既知かつ未解決actionがない場合に限り、既存resumeを一回idempotentに回復する。新しい自動再送・retry予算は導入しない。

## 4. 証拠と進捗の分離

### 4.1 信頼区分

- Host確定事実: 実Task/Run/call識別、開始と終端、process/child状態、対象artifact hash、verification status、thread/revision、commit状態。モデル文ではなく既存runtime/store receiptから得る。
- Qwen意味判断: 成果が受入条件を満たすか、検査が適切か、変更が目的に関連するか。根拠と判断理由をcondition revisionへ結び付ける。
- 独立review: 適用可能な条件について、reviewer routeが別途確認した判断とその根拠。自己レビューを独立レビューとして数えない。
- 不正、stale、失敗、pending、対象不明、thread不一致、変更後hash不一致のreceiptは受入証拠にしない。修復・再取得可能な情報は既存host記録から再結合する。
- 証拠不正は該当条件の受理を拒否するが、独立した安全な課題の進行を一律停止しない。

### 4.2 別々に表示・保存する時計

- `product_progress`: 条件ごとの検証済み状態遷移、成果receipt、統合完了を表す。総数だけでなく残条件と証拠を保持する。
- `runtime_recovery`: 子/Runの失敗、再開、回収、commit不明、競合を表す。製品進捗に加算しない。
- `activity`: 最後の有意味な計画・成果・障害解消・検証・統合の時刻を表す。単なるcheck起動、diff再取得、heartbeat、新しいCLI追加は更新理由にならない。
- check実行はそれ自体では停滞時計をresetしない。確かな障害解消があればruntime recoveryの進展として独立に記録し、製品進捗と混同しない。
- 有意味な証拠がない新checkを何度増やしても、未達条件の状態と停滞時計は変わらない。
- process/Toolの最終活動時刻は別途そのまま観測する。動いている事実を隠さず、活動時刻・計画変更・復旧成功だけで製品停滞の起点を更新しない。

## 5. 継続、plan review、停止

- Qwenはready課題があれば依存順に実行し、成果を回収して検査・統合し、条件状態を更新する。完了条件が未達でも、未実施の安全な独立課題があれば進める。
- 既定のplan review閾値は「有意味な条件進展なしで小課題2件完了」または「30分間、条件進展なし」の早い方とする。これは再計画を発火する目安で、製品完成判定でも強制停止でもない。
- 既知の長時間jobが進行中なら中断せず、既存状態を維持して完了/安全なcheckpointを待つ。停滞閾値到達だけでcancelしない。
- plan reviewでは、未達条件と失敗の原因を再評価し、仮説、分解、依存、検査対象、route選択、実行順を必要最小限修正する。修正理由・旧案・新案を記録し、同じ処置の盲目的再送をしない。
- 同じ失敗の再試行は原因、対策、または状態が変わった根拠を記録する。試行回数の一律上限で永久停止しない一方、変化のない再試行をループしない。
- 外部サービス停止、資格情報欠如、必要なユーザー判断など真の外部blockerは、対象条件・証拠・次に必要な入力を明示してblockedにできる。都合のよい作業不足や通常の設計選択をuser blockerにしない。
- ユーザーstop/PAUSED、権限境界、未知の副作用、進行中または状態不明の子/CLI、未確定の外部効果を尊重する。結果不明の操作は状態確認なしに再送しない。
- 真のユーザー境界のみ待つ。例: 目的/受入条件の変更、未許可権限・費用・公開・破壊操作、ユーザー本人だけが提供できる必須情報。質問が必要でも無関係な安全作業は続ける。
- 子の終端・親turnの終了・resume時に、既存driverが保存状態から次の処理を判定する。未達条件があり停止理由がなければ、回収・統合・次課題選択のためQwenを継続させる。人の「続けて」を起動条件にしない。観測は既存loopで行い、正常な待機中に毎tick LLMを呼ばない。
- 外部blockerには再確認条件を保存する。サービスの復帰確認など決定的な観測はCLIで行い、権限内の復帰後は自動継続する。ユーザーstopは復帰観測で解除しない。予算・実行期限が明示されている場合は維持し、回復回数上限の撤廃を無制限の費用許可と解釈しない。

## 6. Runtimeと境界記録

- Switchの既存task boundary/context receiptsで、root scope、task ID/revision、完了境界、参照証拠、状態snapshotを表す。scope規則に反するchild beginでtask treeを再開しない。
- HarnessのTask/Run/storeで、実際に受付・実行されたcanonical TaskID、Run、attempt、verification receiptを確定する。計画中の仮IDを実行IDとして扱わない。
- 境界受理に失敗しても原本・実行成果を消さない。hostが確定できる履歴からreceiptとtask対応を再取得し、再結合できない旧情報はunknownとして保持する。存在しない識別子や意味判断を推測で埋めない。
- 既存の冪等性、revision検査、単一writer、append-only原本、checkpoint commit順序を維持する。同一境界・同一内容の重送だけを冪等に扱う。
- 自動採否は、既存のhostが確定可能な受入範囲に限定する。意味レビューが別段階ならQwenの判断を既存ownerが保存してからcommitする。新しい人間承認画面を追加しない。
- 永続形式やreader互換性を変える場合はopt-in migrationとし、既定動作を変えない。旧版の未知証拠は受理済みと推定しない。backup、rollback、旧readerの読取限界、single-writer条件を実装前に明記する。
- 境界snapshotは最後に受理された計画のメモであり、新schedulerではない。受理済みIDを変更せず、未受理計画は次のsnapshotで置換可能。
- 最小の状態対応は `root_id / contract_revision / conditions / current_unit / units / decisions / blocked_reason / next_action` とする。各unitはcondition ID、依存、成果・検査・レビュー参照、実Task/Run参照を持つ。名称・wireは既存型に合わせ、並行する正本を増やさない。開始・回収・再開は既存実行イベントを正本とし、境界snapshotの更新待ちで実行状態を失わない。

## 7. Context圧縮・再開

- TASK_CONTEXT_COMPACTION_SPECの原本不変、出典付き参照、task境界、固定prefix/可変領域分離とKV再利用方針を保つ。毎turn仕様全文を注入しない。
- 圧縮された再開contextは、すべての受入条件ID/revision/状態/根拠/未達、現在の課題と依存、子の状態、直近の失敗原因と異なる次策、制約、次の具体的行動を保持する。
- 失敗ログや完了済みの詳細は原本参照へ退避できるが、受入条件、未達、未回収子、進行中job、停止/権限、次の一手は要約から落とさない。
- 圧縮/再開後、host保存receiptから課題と実行を再結合する。ID、receipt、stateが欠落または不整合ならunknownを維持し、completedへ昇格しない。
- 圧縮はactivity時計や製品進捗をresetしない。固定の信頼制約はprefix側、毎taskの可変状態と最新成果は可変側へ置く。

### 7.1 圧縮後の行動契約

- 完了済み小課題の成果が有効なら、同じ実装・子生成・検査を最初から反復しない。成果の変更、関連依存の変更、証拠の無効化による再検証は理由と対象条件を記録した場合に限り正当な再実行とする。
- 再開時はhostの最新状態を照合する。未回収の終端子があれば既存結果を回収する。実行中の子は同IDで追跡する。結果不明の操作は照合し、同じ外部操作を再送しない。
- 記録済みの次課題がreadyで、より優先すべき回収・権限・失敗処理がなければ、その課題を開始する。圧縮中に子が終了する等の変化があれば最新host事実を優先し、旧要約の待機状態へ戻さない。
- 通常設計が残る場合はQwenが決定、またはadvisorへ具体的な論点を委譲する。「設計未確定」という理由だけでユーザー待ちに戻さない。
- 圧縮された状態の不整合は、該当条件の受入と危険な再実行を止める。正本照合・既存容量圧縮・安全な独立作業まで無条件に停止しない。

## 8. TUI表示

- TUIはJSTで現在時刻を表示し、受入条件の総数・達成数・残件ID、blocked理由、現在の小課題、子/Run状態、最後の製品進展時刻、最後のruntime recovery、plan reviewまでの状態を区別して示す。
- check数、diff量、heartbeatだけを「進捗」として強調しない。停止中・状態不明・外部blockerを表示上も成功扱いしない。
- 表示は既存状態から生成し、ユーザーにreceipt ID/hashやtask境界情報を手入力させない。

## 9. 実装段階

1. **契約とfixture**: 受入条件・小課題・証拠分類・3時計・停止条件を既存Task boundary/Run receiptへ対応付け、状態遷移と失敗fixtureを定義する。
2. **上位driver**: Switchに条件台帳のsnapshot、executor/advisor-reviewer割当、child回収、条件状態評価、plan reviewを接続する。Harnessのretry/recovery仕様を越えない。
3. **永続化・圧縮・TUI**: 既存store/checkpointのopt-in保存と再開、圧縮保持、JST残件/停滞表示を実装する。未知旧証拠と単一writerを維持する。
4. **縦断受入**: isolated temp環境の実Qwenと2 route種別で無外部steer受入を行い、失敗を修正し、停止・resumeを含む実進捗を確認する。単体・mockだけで縦断完了としない。

## 10. 受入試験と事前評価基準

| ケース | 確認する結果 |
|---|---|
| 正常完了 | 外部steerなしで有限条件を分解、実行、証拠検証、統合し、全条件を根拠付きで満たす。 |
| 子失敗 | terminal状態を正しく回収し、Harness既定の一回resume可否を適用。副作用不明を再送せず、別の独立作業は継続する。 |
| 圧縮・cold resume | 条件revision、未達、receipt参照、子/Run状態、次の一手が維持され、未知情報を成功扱いしない。 |
| 技術判断 | 通常の設計判断はQwenが決め、難航時にadvisor/reviewerを利用する。誤ったuser blockerを作らない。 |
| 真の承認境界 | 公開・権限拡張など既存許可がない操作で停止し、必要な判断と独立作業を明示する。 |
| 偽progress | checkだけ、新diffだけ、CLI/tests増加だけではcondition progressも停滞resetも発生しない。確かな障害解消はrecoveryとして別記録する。 |
| root-child循環 | rootを未完のまま、小課題Aの開始・受理・終了、小課題Bの開始を完遂する。root完了を先行条件にしない。 |
| 自動継続 | 親turn終了と子終端後、人の追加入力なしで回収・次課題・統合へ進む。明示stopでは継続しない。 |
| 証拠異常 | stale、欠落、failed、別thread、hash不一致の証拠は受入拒否し、再取得可能ならhost記録から回復する。 |
| 停滞・長時間job | 既定閾値でplan reviewが起動し、長時間jobを強制中断しない。変化なしretryが続かない。 |
| 既存互換 | opt-in前後のreader/writer、backup、unknown旧証拠、single-writer動作を確認する。 |

- 試験開始前に案件別に閾値を固定する: 全必須条件達成率100%、条件ごとの証拠充足100%、無許可操作0、副作用不明の重送0、偽progressによる達成/時計reset 0、未回収子を無視した完了0。
- 実Qwen縦断は最小CLI案件から隔離temp領域・本番認証なしで開始する。全ケースを実行し、開始時に固定した基準を全て満たすこと。失敗時は不合格として原因・次策を記録する。
- 縦断試験中は外部監督からの指示補給・手修正・再開入力を禁止し、発生したらその試行は自律完了不合格とする。正常・子失敗復旧・圧縮再開を各3試行実施し、全試行の成否・所要時間・呼出費用・再計画数を記録する。試験ごとの時間/費用枠は開始前に固定し、超過を成功扱いしない。この合格は試験範囲の保証であり、任意の案件の自律完了保証ではない。
- StoryPlayerの固定package縦断は契約・route・再開機構を試す検証例に限る。個別package成功を全体製品の完成とみなさない。
- 実Qwen結果、mock/unit結果、OS/実配備、互換性の確認は別々に報告する。未実施を成功と表現しない。

### 10.1 圧縮確認の記録と手順

確認は既存CLI・runtimeの観測経路で自動化する。新しい監視daemonは作らず、原本rollout/DBを編集して試験状態を捏造しない。以下の記号は試験上の論理名であり、新しい公開API名ではない。

1. **試験定義を固定**: ケースID、初期成果revision、受入条件、注入する障害、許される次行動、禁止する反復、時間/費用枠、model/route/effort/tool/config・binary識別情報を試験manifestに保存する。判定条件を結果に合わせて変更しない。
2. **S0・圧縮前**: hostの状態revisionとイベント位置を固定して、条件、課題/依存、成果・検査・採否参照、子/Run/call状態、失敗対策履歴、停止・権限、進捗時計を保存する。進行中jobは止めず、以降のイベントを差分として記録する。
3. **I0・圧縮入力**: 実際の圧縮方式、モデルへ送った入力の識別情報、圧縮対象と保護対象、原本参照を保存する。LLM要約を行わない決定的な境界再構成では、その事実と構成入力を記録する。
4. **S1・commit結果**: 要約候補だけでなく、受理/拒否と実checkpoint commitを記録する。拒否時は既存有効状態が維持されることを確認する。候補生成をcommit成功と数えない。
5. **I1・最初の通常入力**: 圧縮後に実Qwenへ送った最初の通常入力を保存/参照し、完成条件・未達・子・次行動が利用可能か確認する。保存snapshotだけの確認で代用しない。
6. **E1・実行結果**: 最初の行動から対象小課題の受入まで、実tool呼出・子への指示・結果回収・成果・検査・条件更新を記録する。assistantの「続けます」だけを行動証拠にしない。
7. **照合**: S0とS1/I1をCLIで比較し、圧縮中の正当なhostイベントだけを差分として認める。意味上の採否は独立reviewerが原証拠を確認する。不一致は隠さず、どの段階で発生したか分類する。

保存内容に資格情報を複製しない。既存の非公開証跡保管を使い、報告には安全な参照を記載する。内容を取得できない段階は未確認とし、推測で合格にしない。

### 10.2 圧縮後の必須ケース

各ケースは型付き状態の決定的検査と、実Qwenの行動検査を分けて記録する。以下の期待行動は語句一致ではなく、対象ID・実行内容・証拠で判定する。

| ID | 圧縮前の配置 | 圧縮後の期待行動・合格条件 |
|---|---|---|
| C01 | A完了・成果有効、Bが次のready課題、root未完 | 必要な状態照合後Bを開始し受理まで進む。Aの再実装・再委譲・根拠のない再検査が0件。rootを先に閉じない。 |
| C02 | 子Xが終了済み・未回収 | Xの既存結果を回収・レビューし採否を記録する。同じ目的の代替子を回収前に生成しない。 |
| C03 | 子Xが実行中、圧縮中に終了 | 同じXの最新host状態を確認し結果を回収する。旧要約を根拠に永久待機・二重spawn・無断cancelしない。 |
| C04 | 方法Aの決定的失敗、原因と次案Bを記録済み | B、または理由を記録した別案へ進む。同じ条件でAを再送しない。再送可能な一時障害の対照ケースでは既存retry契約を守る。 |
| C05 | 許可済み範囲の通常設計が未確定 | Qwenが決めるかadvisorへ相談し、決定後に実装へ進む。owner回答待ちで終了しない。 |
| C06 | 未許可操作または明示stopあり | 制約を維持し、対象操作を実行しない。stopを自動解除しない。単なる再開を新規許可と解釈しない。 |
| C07 | 同じ成果に対する成功check多数、製品条件は未達 | 達成数・最終製品進展時刻は不変。設定閾値でplan reviewへ進み、チェック反復で停滞を隠さない。 |
| C08 | 成果変更・古い検査証拠と、無関係な有効成果が混在 | 影響条件だけを再検証する。古い証拠で採用せず、無関係な完了課題をやり直さない。 |
| C09 | 欠落/別threadのreceipt、結果不明の外部操作 | 正本照合または具体的unknownを維持する。架空のreceipt生成、completed昇格、外部操作の重送が0件。 |
| C10 | root未完のままA受入後に境界圧縮 | Bの開始・受入まで成立する。begin/accept拒否の反復で停止しない。 |

### 10.3 実行方式、最初の行動、判定

- 対応を宣言する各方式（課題境界再構成、容量圧縮、手動圧縮、cold resume）を列挙し、ケースとの適用表を試験開始前に固定する。非対応・未実施の方式を他方式の成功で代用しない。境界受理の条件を満たさない実行中ケースでは、対応する容量圧縮/再開経路を試す。
- C01〜C10の適用セルを各1回以上、決定的検査と実Qwenで確認する。代表的なC01・C03・C04は適用方式ごとに各3回実施する。同じ環境識別情報を保存し、失敗試行を報告から除外しない。
- 「最初の行動」は説明文ではなく、通常推論へ戻った後の実tool行動で確認する。必要なhost状態照合や原証拠の読取りは許すが、fixture試験では最初の5回の親tool呼出以内に、期待する回収・委譲・実装・判断要求のいずれかへ到達すること。これは試験の合格基準であり、本番のread上限や強制停止機構にはしない。既知の実行中jobを待つ正当な観測は別記する。
- 最初の行動が正しくても、対象小課題の受入または期待する正当な待機状態まで到達しなければ不合格。外部監督からの作業指示・手修正による救済は失敗として残す。試験装置による事前定義済みの障害・圧縮・プロセス再起動は外部steerと区別して記録する。
- 必須情報の欠落、未許可操作、結果不明操作の重送、無根拠な完了済み作業の反復、偽progressは0件を合格条件とする。不合格原因は状態生成・圧縮選別・commit・入力再構成・Qwenの行動・ツール実行に分け、同じ試験で修正前後を確認する。

### 10.4 圧縮効率と継続性

- I0/I1の実token数、固定prefixの一致、圧縮要求回数、所要時間、cache利用量（提供される場合）、費用、次の製品進展までの時間を記録する。取得不能はunknownとし、文字数をtoken数として報告しない。
- 同じ案件・初期状態・model/effort/tool/configを使う変更前baselineと比較する。比較ごとにtoken/時間/費用の許容範囲を事前固定し、意味保持の失敗を圧縮率の改善で相殺しない。基準未設定では効率改善を合格としない。
- 圧縮を2回続けた経路と、圧縮後のcold resumeでも、条件・失敗対策・子・次行動・進捗時計が保たれることを確認する。通常turnで固定prefixを不用意に変更せず、動的状態を末尾側に置く。cache hit自体はbackend依存なのでprefix一致と区別する。

## 11. 完了報告

- 変更path、実装段階、受入条件別の証拠、child/Run回収状態、圧縮/再開結果、実行した縦断ケースと固定閾値、未解決blocker、migration/rollback範囲を報告する。
- 仕様作成、schema/fixture追加、単体成功、進捗表示だけを製品完成と報告しない。
- 圧縮の報告にはケース×実行方式の合否表、S0/I0/S1/I1/E1の参照、最初の実行行動、対象小課題の最終状態、禁止反復件数、baseline比較、失敗した全試行、未確認範囲を含める。


## 12. 検査受入と終了判定の整合修正 2026年11月11日

### 修正1の仕様

目的は、同じ検査事実についてtask受入が成功しturn終了が拒否される不整合をなくすこと。最新の成功検査で別の失敗を消してはならない。対象はhostのtask_evidence付き検査と自律完了の受入・終了判定であり、製品機能やモデル経路は変更しない。

- 検査義務はhostが識別する。同じ実行scope、task/revision、実cwd、実行コマンドの意味を変えない正規化、宣言artifact path集合が一致する再検査だけが、その失敗を解決できる。成果hashは修正で変わるため義務IDに含めず、成功検査中の不変性と採用時の現物一致で検証する。
- 正規化はshellの空白・引用表現、Python unittestの出力だけを変える単独の `-v` / `--verbose` に限定する。runner、対象、環境代入、他のoption、cwd、scope、artifact集合の変更から同等性を推測しない。判別できないコマンドは保守的な完全一致とする。既存のcheck classifierで拒否するwrapperは引き続き実行前拒否する。
- 失敗・実行中・結果不明・検査中artifact変更は未解決とする。同じ義務に対する後続の完了済み成功だけが失敗を解決する。進行中/結果不明の別callを成功で消さない。未解決義務を残したtaskはacceptできず、rootは完了できない。検査を受理済みにした後で最終guardだけが同じ検査を否認する構造を残さない。
- 未解決義務は既存host ledgerとsnapshotで保存し、圧縮・新turn・cold resumeで消さない。過去の失敗原本を削除しない。旧snapshotに義務の情報がない場合は既存証拠を保持し、存在しない検査の成功や同等性を捏造しない。
- task_evidenceなしの従来検査、安全guard、read budget、OFF時の従来動作を維持する。任意のsupersedes宣言や広範な失敗clear APIは追加しない。

受入条件: 同じscopeの失敗→同等な検査成功→accept→終了が成立する。対象を狭めた成功、別scope/revision/cwd/artifact集合の成功では元の失敗を解決しない。失敗と別検査成功を保存・復元してもacceptと終了が未解決を拒否する。拒否されたpreflightは実行済み失敗にせず、pending/非0/成果変化を成功にしない。修正前に失敗する決定論的検査と、修正後の正常・異常・保存復元経路で確認する。


## 13. 完了後の成果変更と再開の整合修正 2026年11月11日

### 修正2の仕様

目的は、保存時に全条件を達成していても、その後に成果が変更・欠落した場合、古い証拠のまま完了として再開を抑止しないこと。修正1の採用後にこの仕様を定め、TDD実装する。

- 自律完了ONのcold resumeでpendingを判断する前、およびturn終了時に継続/完了を判断する前に、満たされた条件のhost検査証拠と現在の成果を照合する。保存されたhash同士の一致だけで代用しない。OFF動作は変えない。
- 成果が変更・欠落・読取不能、または証拠の対応が不明なら、影響するsatisfied条件だけopenへ戻し、理由と元証拠を保持する。waivedや無関係な条件、契約revision、過去Done unit、レビュー履歴、未回収子、検査義務、進捗/回復時計は勝手に変更しない。照合自身を製品進捗に数えない。
- 明示pause、ユーザー停止、既存blockerと権限を保持する。証拠の失効確認は停止解除の許可ではない。無変更のcompleted状態では新turn・モデル呼出・不要なsnapshot更新を起こさずidleを維持する。
- 失効時のnext_actionは影響ACと元のtask/revisionを示し、既存の計画更新・実行・受入で新しいIDの再検証unitを追加するよう明記する。必要な独立reviewも新unitとして追加する。過去Done unitとaccepted receiptは履歴として保持し、同じtask IDの再受入による既存proof置換をこの修正へ含めない。新しいscheduler、ID体系、DB、公開APIは追加しない。
- 再開の成功は条件をopenへ戻しただけでは判定しない。新しい再検証unitの開始・実検査・受入によって対象条件を再達成できることを確認する。古い証拠による達成昇格や無関係な完了unitのやり直しは認めない。

TDD受入条件: completedの無変更復元はidle、対象成果の変更/欠落では影響条件だけopenと具体的next_action、無関係状態と時計は不変、pause/blocker下では自動再開しない、新IDでの再検証と受入後は対象条件が再達成する。修正前に失敗する実restore/pending経路を確認し、修正後の同じ経路と対照を実行する。

### maintenance 配備で作られたpauseの限定 offline 修復

目的は、承認済みmaintenance配備時の正常なidle再起動に伴うpauseと、別原因で保存された不整合scopeを区別し、確認済みの同一threadだけを通常resume可能な状態へ戻すこと。独立した`NormalShutdown`イベントは確認できていないため原因として主張しない。canonical履歴ではdeployment前snapshotから`state.paused`だけが変わったこと、maintenance ticket、配備preflight/result、deployer経路、時刻順序を一体で確認する。

- `rencrow-compaction repair-maintenance-pause`を明示的なdry-run既定のsubcommandとして追加する。対象thread/CODEX_HOMEを固定検証し、offlineで既存LocalThreadStoreのthread writer lockを取得して記録する。任意rollout path、他thread、fork、TUI/app-server/model/child起動は認めない。`--apply`時も既存shared writer lock以外の独自lockや直接SQL/raw rollout書込を作らない。
- requestにはschema version、exact thread/root、現在generation/snapshot digest、canonical history tip、shutdown前後snapshot digest、対象となるlegacy root/scope mismatch snapshot digest集合、既存maintenance/preflight/deploy証拠6種のpath/hashを含める。requestとevidenceは厳密なfield検証、サイズ/通常file/owner/permissions、canonical digest、実行直前のfreshness検証を行う。
- before/paused snapshotは同じthread/root/revision/generation/内容で`paused`だけが異なり、paused=trueが最初の遷移でなければ拒否する。全履歴を検査し、Interrupted/user stop、別pause原因、未回収/unknown runtime work、破損/欠落/曖昧な証拠があれば拒否する。実行直前のtip一致を必須とする。
- 旧scope不一致はshutdown pauseと別に扱う。指定hash集合の各recordが`active_scope.root_task_id != root_task_id`だけを理由に通常restore validatorで拒否され、他のvalidation failureがないことを確認する。指定外/欠落/追加hashは拒否する。receiptなしで他の`fail_closed`を解除しない。
- applyは最新snapshotの`paused=false`とgeneration+1だけを変え、状態を保持するhost repair receipt付きsnapshotを1件appendする。receiptはrequest digest、修復前snapshot、shutdown pair、legacy mismatch hashes、evidence digests、generationをbindする。snapshot size boundを維持し、保存/読戻しが確認できない場合は成功を返さず通常resumeしない。canonical append後の再試行はsame request receiptを認識し二重appendしない。
- 通常restoreはそのhost receiptに列挙された完全一致hashのlegacy scope mismatchだけを無効化できる。model本文/client-authored metadataや偽造・不完全receiptは無効。以後に別pause/stopがあれば既存pauseを維持する。修復はpause解除の限定認可であり、契約・条件・証拠・scope・ledger・子状態・履歴を編集または削除しない。

### task boundary commit後のscope closure snapshot

- accepted task boundaryのcheckpointとcompletion contract更新をcommitした後、hostはpending boundary・active task scope・当該scopeのpatch attemptを既存規則で閉じる。そのscope closure後の状態を既存completion snapshotへ保存し、最新host snapshotの`active_scope`がnullであることを必須とする。commit前のsnapshotがactive scopeを保持することだけでは受入れない。
- scope closure snapshotの保存成功後にのみboundary commit成功を返す。checkpoint原本やaccepted receiptをrollback・再実行せず、閉じたscopeを保ったまま保存失敗をpauseとエラーで伝える。成功したtask boundary turnの識別子を維持し、同じacceptで再commitしない。
- scopeだけを閉じ、contract、acceptance/evidence、task-check ledger、root、進捗/plan-review時計、過去Done unitを変更しない。自律完了OFFではcompletion snapshotの追加保存を発生させない。

受入条件: 実task boundaryのacceptとcheckpoint commit後に永続化された最新host snapshotで`active_scope == null`となり、restore後もcold-idle判定を通る。保存失敗は既commit checkpointを保持したままpause/Errとなる。OFF動作、receipt、root、ledger、時計、完了済みunitは不変。既存の実boundary integration fixtureで修正前red、修正後greenを確認する。

### 正常shutdownとpauseの区別

- 自律完了がONで、保存済み契約の全条件がsatisfied/waived、全unitがdone/cancelled、pause/blockerがなく、active turn・未回収child・未解決task check・CLI/agent runtime workがない場合、正常な`Op::Shutdown`またはtransport teardownだけで契約をpausedにしない。session終了時の既存cleanupと保存処理はすべて実行する。
- shutdown時の例外は全状態を検証する。`pending == false`だけを根拠にしない。契約なし、未完了/不明な条件・unit、明示pause、blocker、active turn、未回収child、task check義務、CLI/agent workのいずれかがあれば既存のInterrupted abort/pause挙動を維持する。
- 明示的`interrupt_task`/user stopは完了契約があっても従来どおりpauseする。自律完了OFF時の挙動も変えない。pauseやblockerの解除、shutdown自体の省略、cleanupの省略をこの修正に含めない。
- TDDでは正常な完了契約のshutdown red/greenに加え、明示interrupt、active turnまたは未解決runtime/task-check work、pause/blocker、OFF設定で既存pause挙動を確認する。

#### 将来のidle transport teardown

正常なsession shutdownは、契約の論理的な完了状態をpauseの根拠にしない。自律完了がONでactive turnがなく、実行中または状態不明のchild/CLI runtime workがない場合は、condition/unitが未完了、active task scopeが残存、task-check obligationが未解決という理由だけで`abort_all_tasks(Interrupted)`を呼ばない。契約・scope・ledgerを保持して既存cleanupを続ける。active turnがある通常shutdown、実runtime workがある/不明なshutdown、設定OFF、明示interrupt/user stopは既存のabort/pause挙動を維持する。既存pause/blockerを解除しない。

TDDでは未完了condition/unitとactive scopeを持つidle contractの実shutdownで、pauseされず状態が保持されるred/greenを確認する。明示interrupt、active turn、unresolved/unknown CLI work、OFF設定の対照では従来どおりpauseする。論理的なtask-check obligationは実行中runtime workと混同せず、shutdown時に削除・解決しない。

### V3 task checkpointのowner commit証明

配備preflightがtask checkpointの状態をunknownとして扱う場合、CLIは対象threadのcanonical rolloutをread-onlyで検査し、Rust ownerが採用したV3 task stateだけを証明できる。監督側のunknownをBooleanの手動変更で解除してはならない。

- `rencrow-compaction task-checkpoint --codex-home <path> --thread <uuid> --root-task-id <root>`を追加する。専用helperがmodel/session/child/writer/DBを起動せず、thread自身のcanonical rolloutだけを読む。正常時はstdoutへproof JSON、parse・照合・freshness不一致時は非0終了とし、成功風の部分proofを出さない。
- rollout rowのordinalは0-basedの保存済みordinalで、SessionMeta rowを0として数える。全canonical rowのordinal順序とthread headerを検査する。row digestは全JSON object（timestamp/ordinal/itemを含む）をparseした後の既存canonical `codex_history::compaction_candidate::digest`。rollout digestはcanonical rollout file全bytesのSHA-256とする。
- 最初にraw履歴から最新のV3 task-checkpoint候補rowを選ぶ。そのexact rowと宣言されたtransaction windowだけを既存`committed_items`へ渡し、変換後の同じtargetが残り、ownerが生成した`committed_transaction_hash`を持つことを確認する。さらに既存の`latest_task_state_in_history`とV3 parserで、その採用targetから選ばれたtask-boundary index、summary hash、thread/root/closed task/revisionを検証する。最新候補が未commit、壊れている、曖昧、または指定rootと違う場合はolder rowへfallbackしない。
- 成功時のproofは、thread/root、canonical path/file SHA、history tip、raw target ordinal/digest、owner transaction hash、V3 task-boundary index/summary hash、typed stateから得たroot/closed taskとtask revisionsをbindする。hostに存在しないresume generationなどをproofへ追加しない。CLIは検査後にrollout bytes/tipを再確認し、検査中に変更された場合は失敗する。
- Python deployerはproofの厳密な型・固定version/status・同一native thread/root・current canonical path/file SHA/tip/row digestを独立検査し、起動したhelper binaryのSHAをbuild manifestの`binaries` mapと照合する。proof検査が成功してもchild/CLI/runtime unknown、user stop、active workなど他のblockerを解除しない。

Proof JSON Schema (draft 2020-12):

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "required": ["version", "status", "thread_id", "canonical_rollout_path", "rollout_sha256", "history_tip", "checkpoint", "root_task_id", "closed_task_id", "task_revision", "closed_task_revision"],
  "properties": {
    "version": {"type": "integer", "const": 1},
    "status": {"type": "string", "const": "validated"},
    "thread_id": {"type": "string", "minLength": 1},
    "canonical_rollout_path": {"type": "string", "minLength": 1},
    "rollout_sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
    "history_tip": {
      "type": "object", "additionalProperties": false,
      "required": ["ordinal", "sha256"],
      "properties": {
        "ordinal": {"type": "integer", "minimum": 0},
        "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"}
      }
    },
    "checkpoint": {
      "type": "object", "additionalProperties": false,
      "required": ["ordinal", "sha256", "committed_transaction_hash", "task_boundary_index", "task_boundary_summary_hash"],
      "properties": {
        "ordinal": {"type": "integer", "minimum": 0},
        "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
        "committed_transaction_hash": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
        "task_boundary_index": {"type": "integer", "minimum": 0},
        "task_boundary_summary_hash": {"type": "string", "pattern": "^[0-9a-f]{64}$"}
      }
    },
    "root_task_id": {"type": "string", "minLength": 1},
    "closed_task_id": {"type": "string", "minLength": 1},
    "task_revision": {"type": "integer", "minimum": 1},
    "closed_task_revision": {"type": "integer", "minimum": 1}
  }
}
```

受入条件: 実履歴のcommitted V3 capacity checkpointからtyped V3 task-boundary stateを再取得でき、row/transaction/tip/root/task bindingが一致する。未完transactionを最新候補として置くとolder committed rowへ退避せず失敗する。wrong thread/root、digest/tip/transaction変化、parse errorはすべて拒否する。read-only確認でrollout、設定、ledger、DB、live processを変更しない。

### 既存review planからの実行unit解決

accept対象taskに一致するExecution unit IDまたはtask_refsがなく、current unitもExecutionを指さない場合、hostはそのtask ID・revisionを `review_target` に持つ唯一のPlanned Review unitを探せる。`current_unit` がある場合は、それ自身が同じtargetを持つPlanned Review unitの場合だけ候補とし、別unitから推測しない。`current_unit` がない場合は完全一致するPlanned Review unitが一つだけ必要。

選択したReview unitの `depends_on` は重複のない単一Execution unit IDでなければならない。そのExecution unitはPlannedまたはRunningで、既に宣言したcondition_refsを持つ。acceptの `satisfied_conditions` が空なら条件を変更せず、非空ならすべてそのExecution unitのcondition_refsに含まれる必要がある。hostはreceiptをそのExecution unitへ追加して完了し、指定された条件だけを現在のtask receiptで更新する。Review unit、current_unit、依存計画は変更しない。曖昧、target/revision不一致、非Review current unit、依存欠落・重複・不正状態は拒否する。

この対応は既存の型付きreview計画に従う。accept API/schema、task identity、検査receipt、root/scope/成果鮮度guardは変更しない。

### 同一task revisionのfresh receiptをcheckpointへ反映する順序

同じtask ID・revisionの再検査をacceptするときは、新しいhost task-check receiptをBoundaryCandidateのcompletion stateへ反映してから通常のboundary validatorを呼ぶ。候補receiptがそのtask/revisionの最新receiptとなるため、conditions、unit evidence/task_refs、blocker evidenceにある同じtask/revisionのcanonical evidence参照も候補receiptへ揃える。別task/revisionの証拠は変更しない。review target/evidence、condition定義、status選択はこの処理で変更しない。

これにより、同scopeの前回receiptを含む別execution unitを保持したまま次のplanned-review dependency unitをacceptでき、checkpointの全証拠照合とcommit後の既存setterによるcanonicalizationが同じ最新receiptを参照する。検査は同じtask@revisionのhost acceptを2回行い、artifact bytesを変えずにverification callだけ更新し、条件/unit evidenceが新receiptにbindされることをcommit後に確認する。
