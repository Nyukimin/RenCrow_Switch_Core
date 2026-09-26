# 別ラインCompaction候補CLI

`rencrow-compaction`は稼働session・rollout・DBを更新しない。Forkの既存CLIとは別binaryで、候補を作り、最後に使用データを選択する。
実装正本は[Compaction仕様](COMPACTION_SPEC.md)。本CLI自体はlive履歴を置換しない。Fork本体の通常Compaction接続・配備状態は同仕様で管理する。

## 実行

```sh
cargo build --manifest-path codex-rs/Cargo.toml --locked --profile dev-small -p codex-cli --bin rencrow-compaction
# 対象ファイルは入力として固定し、出力先は既存ファイルと別にする。
rencrow-compaction inspect --input input.json --output references.json
rencrow-compaction prepare --input input.json --output candidate.json --model worker --effort high
rencrow-compaction select --input input.json --bundle candidate.json --output selected.json
```

この初期実装は既存RenCrow Gateway `http://127.0.0.1:8090/v1/responses`と、現在のQwen実行契約`worker/high`に限定する。
別route・modelへのfallbackはない。既存認証が必要なら`--api-key-env <環境変数名>`を指定する。認証情報をファイルや引数へ直接書かない。
1要求のtimeoutは既定300秒。モデル/effortは明示必須で、生成上限を下げて節約する処理はない。
自動retryは行わない。ツール呼出し・未完了応答・不正JSON・不正参照・保護対象削除は拒否する。

prepareは、必要な場合だけ①整理案と②意味検証を行い、③整理後の作業要約を一要求で生成する。summary reviewは自動要求しない。新candidateは`version: 2`とhost-bound `summary_hash`、明示的な`summary_review: null`を保存し、未実施をacceptedと偽装しない。version 1 candidateは既存summary reviewのaccepted状態とsummary hash bindingを必須とする。
モデルは整理案の対象IDと意味判断、要約本文だけを返す。hashとbyte参照、全Work ID inventoryはCLI/history ownerが固定snapshotから決定的に生成する。未知IDを拒否し、要約の完全ID転記は要求しない。plan reviewは実際に送ったplanへhashを結び付け、v2の任意summary reviewはあれば同じsummary hashで検査する。モデルにhash計算・転記をさせない。
CLIの整理案は任意の`source_text`でrecord内の削減対象原文を指定できる。省略時はrecord全体。原文に一意に一致するUTF-8範囲をCLIが確定し、空文字、不一致、重複一致（重なる一致も含む）は拒否する。混在recordは有効部分を残す。保護範囲・根拠の保持検査は意味review前にも実行する。reviewへは実際に選択した原文を渡し、モデルにbyte範囲を計算させない。
実際に実行した各要求のresponse ID、usage、壁時計秒、output tok/secをstderrへJSONで表示し、成功candidateにも含める。
reasoningをoutputへ二重加算せず、cacheはusageの元の区分を保持する。summaryは常に1要求で、plan提案とreviewはhuman入力がある時だけ実行するため、その実行分を含めて削減効果を別途比較する。
失敗時も完了済み要求のusageはstderrに残る。`--trace-dir <未作成directory>`を指定すると、各段階のモデル応答原文を非上書きで保存し、不正JSONを診断できる。Unixではdirectoryを0700、ファイルを0600で作成する。応答には入力由来の機微情報を含み得るため、Gitに追加せず専用の非公開領域を用いる。

## 入力契約

`input.json`は信頼する取得処理が作るsnapshot。モデルの出力や、出所不明のJSONをそのままこの入力として採用してはならない。
型は`codex-history::compaction_candidate::CandidateInput`が所有する。

- `version: 1`、`binding`（session/turn/設定/checkpointの識別）、`current_context`（現行ownerから取得した正規context）。
- `records`: `id`, `origin`, `intake_ref`, `scope`, `role`, `text`, `protected`, `execution_evidence`, `opaque`。
- `origin`: `human` / `host` / `work` / `unknown`。本人かどうかと内容の有効性は別判定。
- `human`は受付元の証拠を示す`intake_ref`と`role=user`が必須。ただし文字列参照の存在だけで人間本人を認証するものではない。受付元との照合は取得ownerの責務。
- `work`の実行証跡だけを`execution_evidence=true`にできる。assistantの自己申告を実行証跡に昇格させない。
- `protected`: 削除してはならないUTF-8 byte範囲。`opaque`: 添付・tool call/result・process情報等、そのまま残す構造。
- `unknown`と`opaque`を持つ本人入力の本文は全体保護する。由来や添付が不明なまま容量だけを理由に削除しない。
- `host`を含む入力は現行`current_context`が必須。古いhost本文を本人入力として再保持しない。保護指定/opaqueがあれば別途残す。

入力受付の別記録は、下記の明示された入力元を持つTUI投稿に対応する。`human`指定をLLMに推測させて由来確認の代替にしない。

旧rolloutの安全な取込み:

```sh
rencrow-compaction capture --rollout frozen-rollout.jsonl --output legacy-input.json
```

旧ログのassistant text messageは`work`として要約対象にする。その他のresponse_itemは`unknown`として、metadataを含む元の行をopaqueに保存する。assistant textでもmetadataがあれば元の行を保護する。
`role=user`や文字列markerから人間を推測しない。従ってこの取込みだけでは本人指示の自動削減はできない。
原本は変更しない。不完全なJSONL行はエラーとし、欠落を黙って無視しない。
このcaptureは直線的なresponse履歴の取込みであり、live checkpointの復元ではない。初回の完全なworld_stateはopaque保護し、差分更新・再設定は拒否する。圧縮済み・巻き戻し・retained context・agent通信等の再構築が必要な項目は明示的に拒否し、ownerで復元したsnapshotを要求する。表示用eventやsession/turn metadata、usageはモデルのresponse履歴へ追加しない。

## 最後の選択と保護

selectは元入力hash、整理案とreviewの対応、要約本文のhost-bound hashとview hash、host-generated work source coverage、失効本文を再検証する。v1はaccepted hash-bound summary reviewを要求し、v2はsummary hash必須・reviewなしを許すが、任意reviewがあればacceptedとhash一致を検査する。
出力は`current_context`、`human_input`、`work_summary`、`completed_results`、`protected`を分離したデータ。
旧user群を追加せず、新旧経路の一部を混合しない。これは選択済みの候補データであり、APIへ送信済みの履歴ではない。

生成物は原本から再生成する派生物であり、別の指示正本ではない。入力変更後のcandidateはstaleとして拒否する。
出力は一時ファイルへ書込み・sync後に非上書きで確定する。既存出力・入力を上書きしない。
Unixではtempfileのprivate modeを引き継ぐ。Windowsでは配置先のACLを適切に設定する。
stdoutは`ready`、失敗はstderrの`rejected`とexit 2。`ready`は意味的完全性や稼働配備の証明ではない。

CLI captureは圧縮・巻き戻し済み履歴の復元を行わない。runtime側ではownerが復元したsnapshotとintakeを照合し、checkpoint確定・resumeへ接続する。本人入力0件なら空の選別案をCLIで確定し、Qwenへ送るのは要約要求1件だけ。未対応: merge/reference等の追加削減操作、remote経路、実作業による総token比較。


## この環境の配置と確認

2026-09-22、Ubuntuで`~/.local/bin/rencrow-compaction`へ独立binaryを配置し、`--help`の4subcommandを確認した。
SHA-256: `b9fee2400128deec357c6648ef0047f772c8d112403b039ec618eccb3457ae77`。
稼働中の`rencrow-switch-core`は変更していない（SHA-256 `04159684a80c3751fa0903b81a04fa3d9b4b20550a4b86e927bf73b5b0e53c1d`を確認）。
上記は2026-09-22時点の記録。2026-09-23の完了履歴削減版の配備で、`~/.local/bin/rencrow-compaction`はSHA-256 `ea38850361a312927227cb90f63fe73589afe8d8b1631801f0add61e8624acb1`、`rencrow-switch-core`は`3f6d6d61bd176ee08e65c0a486a4dfc3a9aceb7c6cbbcfc7024305ea87ad03b9`に更新された（記録は私有の`target/fork-bootstrap/compaction-runtime-deploy/candidate.json`）。
CLI配置の取消しはこの独立binaryを除くことで行える。候補ファイルの削除は元sessionの復旧操作にはならず、元sessionは本経路で更新していない。
試験結果・観測した不具合と修正・未完了境界は[仕様の実装記録](COMPACTION_SPEC.md)を参照。
Windows/macOSの実機動作は未確認。local Compactionの実装・配備と共有sessionの受入状況は[仕様](COMPACTION_SPEC.md)を参照。


## 送信本文・添付の別記録（2026-09-22）

新しいForkのTUI（通常起動・`resume`・`fork`）は、起動時に入力元の指定が必須（2026-09-26、利用者指示）。指定しないと端末を初期化する前にエラーで終わる。

```sh
rencrow-switch-core --no-daemon --rencrow-input-author human
# 自動操作の端末では automation を指定する。
rencrow-switch-core --no-daemon --rencrow-input-author automation
```

`--no-daemon`は未梱包の開発binaryを単独起動する上流のオプション。Gatewayの迂回や権限変更は行わない。
この指定は操作者による入力経路の申告であり、物理的に人間がキーを押した証明ではない。
LLM、role=user、client_id、受付順から本人性を推定しない。`automation`の入力は記録するが本人枠へ昇格しない。
起動引数のprompt、内部生成メッセージ、由来を保てない結合・本文変換は本人枠へ昇格しない。
本機能は新binaryが必要であり、既に起動中の旧binaryには有効にならない。

TUIの送信操作で得た本文と画像添付（ローカルpath/placeholder、remote URL）を、
IDE等のhost情報追加前に取得する。送信時にthread/client IDと送信本文hashを付け、
`$CODEX_HOME/rencrow/input-intake/<thread-id>/<client-id>.json`へ私有・非上書きで保存する。
受付記録は最大1 MiB。保存失敗は画面に表示し、通常送信は継続するが本人性の証拠にはしない。
原ログと添付実体を変更・削除しない。添付実体のコピー・バックアップ機能ではない。

`capture`はsessions/archived_sessions配下のrolloutから同じCODEX_HOMEの受付記録を自動取得する。
別配置の場合のみ、操作者が管理する記録のdirectoryを明示する。

```sh
rencrow-compaction capture --rollout frozen-rollout.jsonl \
  --intake-dir /trusted/codex-home/rencrow/input-intake --output input.json
```

受理済み`item_completed/UserMessage`（旧形式では`user_message`）とthread/client ID・本文hashを照合し、対応するモデル履歴に元本文が一意に存在することを確認する。
本文と添付参照を入力枠へ移し、残るhost情報・画像構造・metadataを保護枠へ残す。
派生候補の元メッセージから移した本文だけを除き、本文の二重保持を避ける。原rolloutは不変。
`automation`はwork、記録なし・未受理・継承入力はunknownのまま。不一致・破損は出力せず拒否する。
添付付き入力は現在のopaque保護に従って本文も全体保護するため、添付付き投稿の部分削減は未対応。
添付内容や引用文をユーザー命令として扱う権限は付与しない。
受付記録は信頼するローカル操作者の管理領域を前提とし、モデルや外部文書が提供した任意JSONを本人証明として受け付けない。
通常Compactionへの切替、live checkpoint、履歴復元、既存sessionの再構築はこの機能に含めない。

## 参照化したコマンド出力の取得（検証中）

モデル履歴の`archived_data`には過去の出力を取得する`retrieval_argv`を含める。引数配列を使い、原文を取得してから証拠として判断する。

```sh
rencrow-compaction evidence --thread <thread-UUID> --call-id <call-ID> --sha256 <original-content-SHA256>
```

既定の`CODEX_HOME`から対象threadを解決する。別homeを明示する場合は`--codex-home <directory>`を使う。任意のrolloutファイルを直接指定する機能ではない。読み取り専用で、同threadの完了receipt・call/output・内容hashを照合する。欠落、破損、thread/hash不一致は非zero終了となる。

成功時のJSONは`archived_data`, `version`, `thread_id`, `call_id`, `sha256`, `tool`, `status`, `exit_code`, `process_id`, `retrieval_argv`, `result`を返す。`result`は過去に保存した出力原文であり、新しいコマンド実行や現在の稼働状態の証明ではない。初期対象は完了証拠のある`exec_command`のテキスト出力、取得上限は1 MiB。上限超過や対象外の出力は圧縮時に参照へ置換せず原文を保持する。

このv1取得CLIのcontractと旧rolloutの`capture`動作は維持する。通常Compactionでは新規完了済みpairを要約入力へ直接渡し、圧縮前にv1 markerへ変換しない。既存v1 markerはcanonical raw proofと現在のcallを検証し、marker本文とcall argumentsを要約へ渡す。詳細は[完了済みexec_commandのsummary projection](COMPACTION_SPEC.md#完了済みexec_commandのsummary-projection)を参照。

現行CLIのsubcommandは`capture`、`inspect`、`prepare`、`select`、`evidence`の5つ。仕様⑧の範囲取得引数（`--part --start --end --part-sha256`）と`inventory`はrollout libraryに実装済みだが、CLIには未追加。
