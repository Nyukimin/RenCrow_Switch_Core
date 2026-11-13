# 子エージェント監視

## 有効化

`rencrow_child_monitor` は開発中featureで、コード上の既定値は `false` です。標準のRenCrow runtime profileでは明示的に `true` を設定します。親のwait結果への観測追加は親の設定、子のsnapshot収集は子の設定に従います。health通知は親子とも有効な場合に使いますが、子が見つからない場合は親の設定に従って監視不能を通知します。

監視は同じプロセス内の既存agent status watcherが行います。status変更で即時確認し、変更がない場合は最大30秒間隔でsnapshotを読みます。別のdaemon、監視用LLM呼出し、子turnの追加起動は行いません。snapshotの読取り自体は観測時刻や進捗を更新しません。

## 状態と進捗

子の状態は `Unknown`、`Pending`、`Generating`、`ToolExecution`、`InputWaiting`、`Compacting`、`Completed`、`Failed` です。既存の状態APIを維持し、監視有効時は継続可能なrollback/steerエラーで子を終端扱いしないよう状態の投影を補正します。

`TurnStarted`、実データを含むtext/reasoning/tool引数の出力、toolの開始と終了、入力要求、compaction、terminal eventなどをSessionが観測してsnapshotへ反映します。空delta、request開始、retry計数、snapshot読取りは子の作業進捗として数えません。`progress_sequence` と `last_activity_at_ms` は実観測イベントで進捗が変わった場合にだけ更新します。

出力上限によりprotocol itemをクライアントへ送らない場合も、sampling streamの実データhookは上限制御より前に観測します。したがって、上限に達したこと自体を「出力なし」と扱いません。

状態不明は停止を意味しません。経過時間だけから停止・失敗を判定せず、監視は子をkillしたり自動retryしたりしません。`Completed` または `Failed` のterminal状態を決める責務は既存turn完了処理にあります。

`result_collected_at_ms` はV1のwait結果返却、またはV2の子の結果の親historyへの記録を観測した時刻です。通知のキュー投入だけでは回収としません。親モデルが内容を読んだというacknowledgementでもありません。terminal後30秒を過ぎてもこのreceiptが観測されない場合は「回収receipt未観測」を通知します。結果の紛失や未読を断定しません。

## 時間の読み分け

- **stream idle**: provider streamのchunk間無通信timeoutです。provider設定がなければCoreの既定は5分です。streamの生存待ちであり、turn全体の期限ではありません。
- **初回出力待ち**: 実際のmodel requestごとに開始時刻を記録し、そのrequestの最初のnonempty出力まで測ります。通常のtool後継続は新しいrequest seriesで、retryとは数えません。
- **work deadline**: reviewer専用です。既存のhost-owned completion review期限（通常10分）だけを同じturnへ投影します。通常のchild work deadlineは未設定です。
- **累積retry時間**: retry policyがretryを受理したsampling seriesの開始から終了までを合算します。通常のmodel継続やtool後follow-upはretryに含めません。
- **Gateway timeout**: Gatewayのrequest全体timeoutやfirst-output watchdogはCoreのstream idle値と別の時計です。Gateway overall timeoutはchild snapshotでは未知値として扱います。

## 通知

進捗疑いは実activity時刻が既知で、対象phaseが入力待ちやterminalではなく、5分間進捗が観測されない場合に通知します。これは調査が必要という信号で、停止の証明ではありません。review work deadline到達と、terminal後30秒間のcollection receipt未観測も、観測として別種類で通知します。

通知は親の次の処理文脈へhost由来fragmentとして渡します。V2の子間communicationは `trigger_turn=false` で記録し、親turnを起動しません。失敗した送達は親または子のgenerationが変わるまで同じ通知を再試行しません。通知を読んだ親が次の作業を判断します。

## request相関とプライバシー

model requestの監査ログは、thread/parent/root turn、model/provider、process-local `local_attempt_id`、upstream request ID、response ID、server-reported model、usage、所要時間、終端区分を相関します。`verified_model` はSSEの `x-rencrow-verified-model` header由来と明記します。headerがなければunknownで、WebSocketではこのheader値を推定しません。

監視ログに新たなraw request/response body保存はありません。API失敗はstatus/categoryとbounded reasonを記録します。turnイベントログもbounded reasonとID等のmetadataに限定します。

## 親dispatch重複抑止

同一parent turn内のtarget、canonical body、mode、interrupt fingerprintを使い、履歴を最大256件に制限します。結果が不明なerror後は同じdispatchを自動再送しません。payloadが変わったdispatchと新しいparent turnは別扱いです。状態はprocess-localで、再起動をまたぐdurable記録ではありません。

送信前に予約し、重複は送信しなかったことを明示するエラーで返します。成功receiptの再利用で新しい作業を起動したようには扱いません。上限に達したら古い予約を削除せず、新規送信を拒否します。このguardはQueueOnly送達、明示resume、spawnを対象にしません。

## 互換性と切戻し

featureを無効にするとmonitor snapshot、request監査ログ、health通知を出しません。既存status API、completion watcher、terminal通知、通常のturn停止処理を維持します。monitorの観測結果は既存の完了・中断記録を置き換えません。

切戻しは稼働binary/artifactを対応するpairへ戻し、runtime profileで `features.rencrow_child_monitor=false` にします。これにより監視を止めます。既存のpersisted completion/review記録をresetして切り戻す必要はありません。

## 検証範囲

unit/integration検査、ビルド、配置確認、稼働確認は別々の証拠として記録します。ローカルの検査成功だけではlive backendでの挙動を証明しません。この文書はテスト件数、PID、live backend成功を保証しません。
