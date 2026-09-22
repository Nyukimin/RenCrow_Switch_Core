# 別ラインCompaction候補CLI

`rencrow-compaction`は稼働session・rollout・DBを更新しない。Forkの既存CLIとは別binaryで、候補を作り、最後に使用データを選択する。
実装正本は[Compaction仕様](COMPACTION_SPEC.md)。通常Compactionへの切替はまだ行わない。

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

prepareは、①整理案、②別要求での意味検証、③整理後の作業要約、④要約検証を行う。
モデルは対象IDと意味判断だけを返す。hashとbyte参照はCLIが固定snapshotから生成し、未知IDを拒否する。意味reviewは実際に送ったplanへ、要約reviewは送った要約へCLIがhashを結び付ける。モデルにhashの計算・転記をさせない。
CLIの整理操作は現在record全体単位。現行指示と撤回対象が同居するrecordは保持する。基礎ライブラリの部分範囲機能は自動公開していない。
各要求のresponse ID、usage、壁時計秒、output tok/secをstderrへJSONで表示し、成功candidateにも含める。
reasoningをoutputへ二重加算せず、cacheはusageの元の区分を保持する。4要求の費用を含むため、削減効果は別途比較が必要。
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

入力受付から本人の由来を自動記録するadapterは未接続。`human`指定をLLMに生成させて自動由来確認の代替にしない。

旧rolloutの安全な取込み:

```sh
rencrow-compaction capture --rollout frozen-rollout.jsonl --output legacy-input.json
```

旧ログのassistant text messageは`work`として要約対象にする。その他のresponse_itemは`unknown`として、元のpayloadをopaqueに保存する。
`role=user`や文字列markerから人間を推測しない。従ってこの取込みだけでは本人指示の自動削減はできない。
原本は変更しない。不完全なJSONL行はエラーとし、欠落を黙って無視しない。

## 最後の選択と保護

selectは元入力hash、整理案とreviewの対応、要約view hash・review・作業source coverageを再検証する。
出力は`current_context`、`human_input`、`work_summary`、`completed_results`、`protected`を分離したデータ。
旧user群を追加せず、新旧経路の一部を混合しない。これは選択済みの候補データであり、APIへ送信済みの履歴ではない。

生成物は原本から再生成する派生物であり、別の指示正本ではない。入力変更後のcandidateはstaleとして拒否する。
出力は一時ファイルへ書込み・sync後に非上書きで確定する。既存出力・入力を上書きしない。
Unixではtempfileのprivate modeを引き継ぐ。Windowsでは配置先のACLを適切に設定する。
stdoutは`ready`、失敗はstderrの`rejected`とexit 2。`ready`は意味的完全性や稼働配備の証明ではない。

未実装: intake由来の自動取得、merge/reference等の追加削減操作、通常Compactionの最終接続、checkpointの原子的確定・resume、remote経路、実作業による総token比較。


## この環境の配置と確認

2026-09-22、Ubuntuで`~/.local/bin/rencrow-compaction`へ独立binaryを配置し、`--help`の4subcommandを確認した。
SHA-256: `ae3d3b2a38f545483fe0b59cff61c440d1de2bafe34066230f1847093a18478c`。
稼働中の`rencrow-switch-core`は変更していない（SHA-256 `04159684a80c3751fa0903b81a04fa3d9b4b20550a4b86e927bf73b5b0e53c1d`を確認）。
CLI配置の取消しはこの独立binaryを除くことで行える。候補ファイルの削除は元sessionの復旧操作にはならず、元sessionは本経路で更新していない。
試験結果・観測した不具合と修正・未完了境界は[仕様の実装記録](COMPACTION_SPEC.md)を参照。
Windows/macOSの実機動作は未確認。通常Compactionへの切替は未実施。
