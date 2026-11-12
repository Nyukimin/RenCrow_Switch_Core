# 課題境界によるコンテキスト再構成仕様 v1.0

作成: 2026-11-02 / 指示: れん / 設計・受入: ren1 Sol / 実装: Luna / 設計相談: Astra

## 1. 目的と適用先

大課題を中課題に分け、中課題の成果を受け入れた時点で、その作業の探索・相談・試行錯誤を常駐コンテキストから外す。次の中課題には、大課題の目的・制約、完了した成果、残件、次の作業に必要な情報を渡す。毎回長い会話から課題状態を推測し直すことを避ける。

対象は RenCrow_Switch_Core の RenCrow Compaction 経路、および RenCrow_Harness の共通 Compaction 経路。ren1 が本仕様の実装を担当し、ren2 の Midori/CORE 作業には割り込まない。

本書は既存 COMPACTION_SPEC および Harness の保存・権限・byte 契約に対する追加仕様である。競合する場合、本書が明示的に変更する課題境界と派生contextの構成だけを優先し、それ以外の不変条件を維持する。新しいDB、daemon、汎用課題管理サービス、長期Memory基盤は作らない。

## 2. 現行方式との差分

- 現行: 主に容量到達を契機として、必要時に指示を選別し、前回要約と作業経過から新しい要約を作る。
- 追加: 受理された中課題境界で、型付きの課題状態から決定的に次のcontextを作る。
- 既存の前回要約、原文保存、observation参照、checkpoint、CAS/commitフェンスは再利用する。既存機能を再実装しない。
- 境界再構成そのものには、履歴全体に対するLLMの指示選別・要約生成を要求しない。意味判断と成果説明はオーケストレータが当該中課題の情報から行い、検証・参照解決・保存・再構成はruntimeが行う。
- 未完了作業で容量に達した場合は既存Compactionへ進む。境界の到来を待って容量超過や停止を起こさない。

## 3. 役割と完了の意味

- オーケストレータ: 中課題の分解、エグゼキュータへの小課題の指示、結果回収、レビュー、境界候補の提出と次課題選択。
- エグゼキュータ: 当該小課題を実行し、成果物と実行した検査の参照を返す。
- アドバイザー: 難しい原因・設計判断への助言。助言だけでは成果受入や履歴整理を許可しない。
- runtime: 呼出主体、task/thread/revision、成果・検査・レビュー参照、未確定実行状態を検証し、保存とcontext公開を所有する。

`update_plan`のcompleted、子のterminal完了、成功exit code、assistantの「完了」はそれぞれ単独で境界受理の証拠にならない。単に完了した実行と、受入条件を満たした中課題を区別する。

受入には、当該課題に対応する成果と検査のhost記録、および同じ成果・revisionを対象とした親レビューを要する。hostが保証するのは主体・出典・対象・状態の一致であり、テストだけで仕様の意味上の正しさを証明したとは扱わない。レビュー判断はオーケストレータの責務として残す。

親レビューreceiptを作る既存入口がない場合、hostが識別・認可した親による境界提出操作自体をレビュー操作とし、検証した候補・課題・成果revision・検査参照・実際の呼出主体へ束縛したreceiptを同checkpoint内へ記録してよい。モデル本文のreview済みフラグとは区別し、提出だけで不正/未検証の成果が受理される経路は作らない。

通常運用で中課題ごとの人間への確認は追加しない。既存の権限を持つ親/hostが受理操作を行う。子の出力本文や、モデル入力中の `accepted: true` / `accepted_by` の自己申告は受理操作の代用にならない。

## 4. 常駐する課題状態

単一の型付きスナップショットを使う。フィールド名は各runtimeの既存命名規則に合わせてよいが、次の意味を満たす。

| 情報 | 内容 |
|---|---|
| 識別 | format version、root task ID、thread、状態revision、境界ID |
| 大課題 | 目的、完了条件、権限・継続制約への原文参照 |
| 中課題一覧 | 安定したID、短い目的、pending/in_progress/completed/blocked、依存関係 |
| 完了成果 | 結果の短い説明、成果物revision/hashまたは既存のopaque参照、検査receipt、親レビュー参照 |
| 残り・次 | 未完了中課題、現在取り組む中課題、次の具体的な一単位 |
| 決定と不確実性 | 後続に効く採用判断、既知の問題、未確認、再開条件とそれぞれの出典 |
| 保護対象 | 未回収tool/child/job、結果不明の外部効果、継続中の制約、scope不明の指示 |

完了した中課題の全文ログを常駐させない。古い完了詳細は既存archive/checkpointへ参照化し、依存する成果や未解決事項は必要な範囲で保持する。課題メモも無制限に増やさず、既存のcontextサイズ・断片上限・計数規則を適用する。参照化した詳細は既存の原本取得経路で取り出せること。

新しい境界でclosedとする課題以外のcompleted状態は、前checkpointのhost受理記録との一致を要求する。候補が他課題をcompletedに変えたり、過去の成果・根拠を黙って消すことを許さない。大課題IDと中課題間の依存関係は別概念であり、親子関係を「親の完了を待つ依存」として扱わない。全中課題完了時は残件とcurrentが空の最終状態を表現できること。

保存済みsnapshotの根拠に、LLMが作った存在しないID・hash・検査結果を採用しない。IDと証拠の参照をhostが解決し、存在・所有範囲・内容の対応を確認する。モデルにoffset、hash、request相関IDを手計算・復唱させて正本にしない。

## 5. 境界入力と実用経路

境界候補は状態スナップショット、閉じる中課題ID/revision、成果参照、検査参照、親レビュー参照を含む。レビューは当該候補/成果に束縛し、以前の別成果のレビューを流用しない。

各runtimeに、オーケストレータ/hostから実際に境界を提出できる入口を設ける。既存のtool・Service・`context/compact`・CLIと連携し、純粋関数やテスト専用コードだけの実装では完了にしない。API/schema/CLIを追加・拡張した場合、producerとconsumer、許可主体、入力例、保存・再開まで一緒に実装する。

候補の提出とhostによる受理を区別する。入口でhost記録に基づき受理できる場合は一操作でよい。根拠の取得とレビューが別段階の場合は、既存ownerの記録を使って段階を分ける。新しい承認UIや人間待ちを作らない。

中課題を受理した境界操作が、通常の作業継続前に再構成を発火する。オーケストレータが実行できる具体的な入口と例を提供し、単に「適宜compactする」というプロンプト追記だけにしない。

Harness初期版で閉じる課題は、実際に受付・実行されたcanonical Harness TaskIDで指定する。大課題IDはgroup識別でよい。計画段階の仮IDは受理済みIDではなく、親は実TaskIDが確定した後、次の境界へ提出するsnapshotで明示的に置換し、依存/current参照も同時に更新する。受理済みIDは変更しない。任意の論理IDを後付けで既存Runへ推定対応させない。

このsnapshotは「最後に受理した境界時点の計画メモ」であり、liveな実行スケジューラではない。未完了の計画スロットのIDや次の計画は親が次のsnapshotで更新できる。`turn/start`で新TaskIDが出るたびに課題メモやcontext revisionを自動更新する機構は初期実装に追加しない。整理する実行履歴の所有判定は、計画仮IDではなく常に実TaskIDを使う。

検査対象との結合が従来receiptにない場合、初期実装はlocal workspace内の明示された検査対象ファイルに限定してよい。HarnessのVerificationPlanにoptionalなartifact selector、Switchの既存exec入力にoptionalなtask-evidenceを追加し、既定空/省略では旧検査動作を維持する。対象を指定しない旧検査から、成果revisionに結合済みの強い証拠を推測生成しない。実ファイルの検査前後hashと終端statusを記録し、受理時にも同じ対象を確認する。単に同Runの不変なログEvidenceを列挙することは、ファイル検査対象の結合の代わりにならない。

同じ境界・同じ内容の再送は冪等。古いrevision、別thread、同じIDで異なる内容、循環/欠落した依存、存在しない成果・検査・レビューは不正候補として扱う。不正候補では派生contextを変更せず、型付きの理由を返す。通常の作業・既存容量圧縮まで永久停止する理由にしない。

## 6. 整理できる範囲

初期の主対象は完了した作業のassistant経過とtool履歴である。完了中課題を正しく識別できる範囲だけを整理し、別の未完了中課題の経過を巻き込まない。call/resultは対応を保つ。

現行実装は通常compactionのhost-owned `Origin::Work` / protected / opaque分類を再利用する。受理して閉じるscope内で非protected・非opaqueのWorkと分類されたassistant進捗文を整理候補にする。`apply_patch`はdispatch時にactive scopeと実environment種別をcall IDへ固定し、終端成功したlocal patchだけ記録する。patchの全変更先が今回のverification receiptのartifact stampに含まれ、commit時点のartifact hashも一致する場合に限り、その完全なcall/result pairをWorkとして整理候補にする。失敗/部分適用/remote patch/未検査targetは保持する。その他の`exec_command`、未知tool、失敗/未回収call、境界tool自身のcall/result、scope外のassistant発言は保持する。ユーザー/開発者/system原文、外部効果の有無が不明な実行、継続制約は整理対象にしない。

履歴と課題の対応は、既存のtask/run/call所有情報、または当該作業を始める時にhostが保存した開始境界で確定する。対応を遡ってモデルに推定させない。開始情報がない旧履歴は保持する。必要なら同じ境界入力に開始/継続操作を設け、開始以降の実行をruntimeが当該中課題へ結び付ける。利用者にログoffsetを手入力させない。

ユーザー/システム/開発者の継続制約、権限、最新の訂正を、課題メモにあるからという理由で削除しない。指示を成果へ置き換える場合は既存のscope・訂正・完了検証を満たすこと。scopeが確定しない、同じ発言に継続制約が混在する、指示対応を十分に証明できない場合は原文保持を優先する。初期実装でその置換に対応しないことは許すが、対応済みと報告しない。

未回収の子/tool、未確定の書込み・生成・送信、失敗した最終検査がある課題をcompletedとして閉じない。他の独立課題の安全な進行は妨げない。境界commitのために実行中の子・CLIを強制終了しない。

課題メモ・引用・成果物内容は派生データであり、ユーザー指示やtrusted instructionに昇格させない。元の監査ログ、会話原文、成果物は削除・直接編集しない。

## 7. 再構成と保存の順序

1. 現thread/revisionと必要な成果・検査・レビュー・未確定実行状態をhost snapshotとして取得。
2. 候補と根拠を検証し、既存のサイズ・計数契約内でcontext案を決定的に生成。
3. 既存commitフェンス内で最新入力、revision、cancel、実行状態を再検査。
4. 受理状態、整理元の参照、成果/検査/レビュー参照、再構成contextを、既存の一つのcheckpoint commitへ結合。
5. commitの保存成功を確認してからactive contextを公開し、次の中課題を実行。

課題状態だけ保存してcontextが旧版のままになる別書込みを避ける。commit前の取消/競合では旧状態を維持し、commit後の取消では保存済み境界を取り消さない。保存結果不明は既存の復旧契約に従い、勝手に再送しない。

cold resume、連続する境界再構成、容量Compactionの後でも、最後にcommitした課題状態と証拠対応を復元する。従来checkpointから受理済み課題を推測補完しない。

既存保存形式への未知field追加を互換と偽らない。必要なら明示的な版を設け、新readerは旧版を読めるようにする。旧binaryが新checkpointを読めない場合の復旧限界と、有効化前のbackupを文書化する。旧wire/goldenを新形式に合わせて書き換えて試験を通さない。

モデル応答を伴わない課題境界は専用のmode/版として表現し、通常のLLM要約やEmergency成功に偽装しない。モデルresponse/usage receiptを捏造しない。新modeのreader/writerに加え、通常の容量Compactionとresumeが課題状態を維持するconsumerも同じ変更に含める。

## 8. 会話・創作への適用

共通のsnapshot/出典/保存機構を使い、用途は明示的に選ぶ。初期導入で曖昧な話題終了を自動認定しない。

- task: 受入済み中課題を区切りに、成果・検査・残件を保持する。
- conversation: 話題のまとまりを区切りとし、直近の会話原文を保持する。背景、明示された好み・気持ち、合意、未解決の話題を出典付きで引き継ぐ。
- creative: 設定検討、シーン、章の区切り。採用設定、人物関係、現在位置、文体の参照、伏線・未解決を保持する。

会話・創作へ偽のコード検査成功を要求しない。配信済み成果物・発言の原文参照と、権限あるレビューを根拠として扱う。「疲れたと発言した」を「現在疲れている」と断定する等の変換をしない。最近の何往復を保持するかは設定/明示入力とし、最低限最新の未解決なやり取りを保持する。モードを自動推測して変更しない。

実装はtask経路を先に縦に完成させ、その後conversation/creativeの保持差分を同じ機構へ接続する。未対応モードは明示的に拒否/従来経路へ案内し、対応済みと表示しない。

## 9. 実装・配置方針

- Switch: 既存history/compaction、session owner、checkpoint、再開処理を使う。純粋な組立て・検証は既存history層に置き、coreへ巨大な並行実装を増やさない。
- Harness: 既存Service/Kernel/Compaction/storeを使う。ContextBlock、Evidence、CJ1等の契約を維持し、公開変更が必要なら対応schema・producer/consumer・契約vectorも更新する。
- まず明示的に選べる入口で検証する。既存のtoken閾値、model/effort、認証・権限・Policy・GPU資源設定を変更して高速化としない。
- 変更は小さい実用単位に分け、両repoの独立実装を並行化する。既存dirtyをbaselineとして保存し、他作業の変更を消さない。
- ren1の公式Codexを置換せず、ren2や既存サービスを停止しない。build/testと隔離実行を先に完了する。稼働切替が必要な場合は安全な切替点と成果物・復旧手順を示す。

## 10. 受入試験

最低限、各runtimeで次を確認する。

1. 実入口から中課題Aを受理・再構成し、次の通常モデル要求が大課題/成果A/残件B/次Bを含み、Aの探索ログ全文を含まない。
2. 原本の探索ログ・証拠を参照経由で再取得できる。
3. completed自己申告、検査だけ、レビューだけ、欠落/失敗/stale/cross-threadの根拠では境界が受理されない。
4. 同じ発言内の継続制約、scope不明指示、未完了B、未回収結果と未確定外部効果を保持する。
5. 同じ境界の重送、競合する入力/取消、commit前後の中断とcold resumeで状態とcontextがずれない。
6. 容量圧縮へのfallbackと、その後の次境界でも課題状態が失われない。
7. 旧checkpointと新checkpointの読み戻し、および未知版拒否が明示どおり動く。
8. 対応するconversation/creativeでは直近原文と未解決事項が保持され、引用や感情が新しい指示/断定へ変換されない。
9. 同一fixtureの旧方式と新方式について、通常入力/要約入力/出力/モデル呼出回数と所要時間を分けて比較する。文字数だけで速度や品質改善を断定しない。境界再構成にLLM呼出しがないことを検証する。

Rustはjust fmt/対象just test、Goは関連packageと統合試験を使う。実モデルでの継続はmock要求検査と別に記録し、未実施を成功にしない。OS・稼働配備の未検証も区別する。

## 11. 完了報告

仕様と公開入口、変更repo/file、hostによる受理条件、実際の再構成前後の要求、保存・再取得・再開、実行した検査、旧形式互換、未対応モード、実モデル/稼働配備の確認範囲を報告する。基盤だけ・schemaだけ・要約文だけを作業完了にしない。

## 12. Switch parent tool quick use

The parent session must have RenCrow compaction enabled and be an authorized CLI, VSCode, or Exec session. The parent profile allowlist must include `task_context_boundary`; child role allowlists remain unchanged. The boundary tool is shown only to the parent.

At the beginning of a concrete subtask, call:

```json
{"action":"begin","root_task_id":"root-42","task_id":"task-a","task_revision":3}
```

Run the ordinary verification command with an optional `task_evidence` object. Paths are local workspace-relative regular files:

```json
{"cmd":"just test -p codex-core task_check","task_evidence":{"task_id":"task-a","task_revision":3,"artifact_paths":["core/src/task_check.rs"],"check_kind":"verification"}}
```

After successful terminal completion, submit the task state and parent review in one `accept` call:

```json
{"action":"accept","goal":"Implement and verify the task boundary","tasks":[{"id":"task-a","purpose":"Implement the feature","status":"completed"},{"id":"task-b","purpose":"Review integration","status":"in_progress"}],"current_task_id":"task-b","decisions":["Use the existing checkpoint owner"],"constraints":["Keep original user constraints"],"result":"Task A passed its declared verification."}
```

The host resolves the latest started verification attempt for the active task/revision and binds its receipt to the actual exec call and artifact hashes. The caller supplies no receipt ID, hash, thread ID, nonce, or call ID. A missing, failed, pending, stale, changed-file, unresolved-process, or unresolved-child check blocks acceptance. The tool result says `prepared`; the owner commits only after its result is recorded and before another model request. A successful commit writes typed V3 task-boundary metadata with the state and exact source fence in the existing checkpoint. Original history remains available in the append-only source log. Retrieve referenced historical details from that source using checkpoint `item:<index>` refs and hashes; refs are integrity selectors, not a new database or standalone API.

Cold resume reads V3 state from the checkpoint. Existing V2 capacity checkpoints remain readable and their capacity metadata can be carried forward, but a V2 checkpoint does not imply that a task boundary was accepted. Older binaries that do not understand V3 may reject or fail to resume a thread containing it. Before enabling a V3-writing binary/profile, preserve a copy of the thread store and raw rollout source; a backup is required for rollback to an older binary. This does not change existing model, provider, or reasoning-effort settings.
