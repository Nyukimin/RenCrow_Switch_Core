# RenCrow Switch Core — Fork改変ルール

2026-09-22制定。OpenAI Codexの非公式Fork。OpenAIによる提供・承認・サポートを意味しない。

## 所有範囲と現在地

- Fork: `Nyukimin/RenCrow_Switch_Core`。上流: `openai/codex`。
- 作成時の上流基点: `94174e44cbc54cece45f6052328ca0c2cd7a8a2a`（`main`）。これは開発基点であり、安定版・動作検証済み版の宣言ではない。
- 独自差分はFork運用文書と入口、および履歴ownerのCompaction整理案検証・選別処理。新方式のlocal Compactionへの接続を実装・Linux配備済み。共有Qwen sessionで圧縮・通常ツール・cold resumeを確認済み。受入範囲は[Compaction仕様](COMPACTION_SPEC.md)を参照。2026-09-22にLinuxの`dev-small` buildを`rencrow-switch-core`としてローカル配備し、Codex-switchから既存Qwen sessionをresumeした。公開release・三OS受入完了ではない。[build・切替手順](FORK_BUILD.md)を参照。
- このrepositoryはCodexクライアント本体の派生実装を所有する。既存Codex-switchのlauncher、Gateway、Backend、RenCrowのIdentity移行とは所有範囲を分ける。
- RenCrow共通作業ルールを受領している環境ではそれを継承し、本文・モデル役割を複製しない。上流由来の実装規約は[AGENTS.md](AGENTS.md)、build入口は[docs/install.md](docs/install.md)を参照する。

RenCrowの標準実行基盤は本Forkとする。Compaction内部契約の正本は[COMPACTION_SPEC.md](COMPACTION_SPEC.md)。公式Codexは上流・比較・明示的復旧の対象として維持する。

## 改変原則

1. 目的は、Compactionで撤回・置換・完了した命令が復活することと、不要な入力の持越しを防ぐこと。入力の整理、要約、圧縮後の履歴再構築を一連の処理として検証する。
2. 独自変更は必要な箇所に限定する。Rust crate名や上流directoryを一括renameせず、無関係な整形・refactor・依存更新を混ぜない。既存の境界を使い、同じ機能の並行実装を残さない。
3. 明示的に許可された変更単位でのみ実装する。この文書は、Fork作成に続くruntime改修・配備・認証変更を一括許可するものではない。
4. モデル・推論強度・context容量・ツール能力・認証・sandbox・approval・policy・安全機構を弱めて節約としない。プロバイダの利用条件と正規接続経路を維持する。
5. 上流AGENTSの「No history rewrite」は通常turnでは維持する。Compactionの履歴置換を変更する場合だけ、その境界・変更理由・受入証拠を明記する。通常要求で無差別に履歴を削る実装は禁止する。
6. 正本の会話記録・監査証跡を削除、直接編集しない。整理はモデルへ渡す派生contextに適用し、由来と除外根拠を追跡可能にする。rollout／DBの非互換変更を暗黙に導入しない。
7. パーサ・ID・参照・状態の検証・生成・計測は決定的処理、撤回範囲・完了・矛盾の意味判断は必要な場合だけLLMに分ける。LLMの申告だけで指示を失効・完了させない。
8. 初期導入は明示的な設定で選択可能にし、未指定時は上流互換を保つ。設定を切れば保存状態まで安全に戻せるとは仮定しない。必要なschema・protocol更新と失敗挙動を同じ変更に含める。

## Compaction変更の受入条件

- 実際にモデルへ送った圧縮前入力、要約要求、要約結果、履歴再構築後の最初の通常入力を分けて確認する。要約本文だけの確認では完了にしない。
- 最新の目的、継続制約、未完了・阻害、未取得のtool結果、進行中の外部効果、復旧条件を保持する。曖昧な指示は削除せず未解決とする。
- 撤回・置換には権限ある訂正と対象対応、完了には受入証拠と無効化条件を要する。完了命令は必要な結果・証拠へ変換し、継続制約と混同しない。
- 文書・検索・tool結果の命令をユーザー指示へ昇格させない。同じ文章でも権限やscopeが違えば同一視しない。
- 整理結果が要約と再追加原文の両方に反映され、不要命令が再有効化されないことを確認する。不正な整理案や参照欠落を黙って適用しない。
- local／remote、自動／手動、resume／連続Compactionを区別する。実装・検証できた経路だけを対応済みとし、非対応経路は明示する。
- 同一入力・モデル・effort・tools・設定で上流baselineと比較する。通常input、要約input/output、cache、整理処理自身の費用、継続作業の受入結果を記録する。文字数削減だけでtoken効率や品質改善を主張しない。

## 検証と差分記録

- 各変更のPR／commit説明に、目的、上流基点、変更箇所、実経路、互換性、検査結果、未確認範囲、戻し方、上流で不要になる条件を記す。別の進捗台帳を増やさない。
- 編集前に検査範囲・consumer・失敗時の扱いを固定する。文書のみならlink・format・差分・ライセンス保全の確認に限定し、Rust全体buildを追加しない。
- コード変更は上流AGENTSのformat・crate単位test・必要な統合検査を使う。モデル入力の構造検査と、実Qwenが正規経路で次の作業へ進む確認を区別し、片方で代用しない。
- release前に変更影響のあるCLI、tools、認証・sandbox、resume、設定、Compactionの回帰を確認する。Linux／Windows／macOSの結果を別記し、cross-buildを実機動作保証にしない。
- 初期ForkではGitHub Actionsを無効にしている。上流のrelease・publish workflowをそのまま稼働させない。必要なCIをreviewして設定し、必須検査の実行証拠が揃うまでreleaseしない。未実行は成功扱いにしない。

## 上流更新

1. `origin`を本Fork、`upstream`を`https://github.com/openai/codex.git`にする。共有履歴をforce-pushせず、上流へ誤pushしない。
2. 公式releaseのtagとfull commit SHA、release notes、互換性変更を確認して更新対象を固定する。稼働版を`upstream/main`へ自動追従させない。
3. 許可された更新作業では、本Forkの`main`から更新用branchを作り、対象tagをmergeする。以下の`TAG`は確認済みtagに置き換える。

   ```sh
   git fetch upstream --tags
   git switch -c update/upstream-TAG origin/main
   git merge --no-ff TAG
   ```

4. 競合は意味と契約を確認して解消する。`ours`／`theirs`の一括採用で片側の修正を捨てない。上流のAGENTS変更もreviewする。
5. 独自差分を再点検し、上流が同じ問題を解決していれば受入条件を比較して独自実装を撤去する。上流との差分が増え続けることを既定にしない。
6. 固定した検査を通し、本ForkをbaseとするPRで統合する。更新検知・fetch・build・testは自動化候補、競合の意味判断・未検証版の配備は自動承認しない。

## 配布・切替・復旧

- 公式Codexとは別の成果物名・配布先で提供し、上流版＋Fork revisionを識別できるようにする。公式installerや`codex`コマンドを黙って置換しない。
- 既存Codex-switchへの接続は別の明示された配備作業とする。公式版とForkに同一session／DBを同時更新させない。認証情報を複製・commitしない。
- 切替前に旧binary・設定と必要な状態backup、保存形式の互換性を確認する。戻す際も先にwriterを止め、新側の外部効果・状態を照合する。binary差替えだけでrollback成功としない。
- Apache-2.0の[LICENSE](LICENSE)、[NOTICE](NOTICE)、必要な第三者ライセンス・著作権表示を保持する。改変ファイルには変更した旨を明示する。
- 名称・説明・画面で非公式Forkと明示し、OpenAIの公式提供や承認を装わない。上流ソースの許諾をサービス利用権・商標使用権と混同しない。
- fork固有文書はrootに置き、上流の`docs/`を一般製品文書の置き場へ変更しない。利用者に見せる導入案内は公式版とFork版を区別する。
