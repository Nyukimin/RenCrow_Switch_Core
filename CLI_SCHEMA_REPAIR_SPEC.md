# RenCrow Switch Core CLI/Schema 修正仕様（2026-11-15）

## 文書情報

- 対象: 今回許可された限定修正のみ。旧WORLD/StoryPlayer作業、native task境界、監督PAUSEDは変更しない。
- 範囲: 下記S1（自動テストの起動側）・S2（Schema の型識別名）と、S2に付随する生成物のみ。全体テスト・CI・Backend 操作・サービス再起動・配備・commit/push は行わない。
- 既存の作業成果（`docs/CURRENT_FIX_IMPLEMENTATION_SPEC.md` の差分、`.serena/`、`*.snap.new`、`.pending-snap`）は他作業の成果であり上書き・削除・採用をしない。
- 検査結果・ログ・差分の証拠は私有の `Tmp/ren2-supervision/periodic/private/residuals-1-4-20261114/r5-repair-20261115/` に保存し、本書は契約のみを定める。

## S1 CLI の呼び出し契約

### 不変条件

1. 製品の必須指定 `--rencrow-input-author` と human/automation の識別は維持する（`codex-rs/tui/src/input_intake.rs` の `require_author` / `MISSING_AUTHOR_MESSAGE`、`codex-rs/tui/src/cli.rs` の `value_parser = ["human", "automation"]`）。
2. 入力元の指定が必要な既存 TUI 自動テストの起動側は、明示的な `automation` を渡す。自動テストを human と偽装しない（既存の `codex-rs/tui/tests/suite/resize_reflow.rs`・`focus_palette.rs` が同じ方針で作られている）。
3. 本当に human 経路が受入対象の専用 fixture は既存契約を守る（例: `codex-rs/cli/src/main.rs` の `resume_and_fork_preserve_declared_input_author` は `human` を渡す）。
4. 非 TUI コマンド（`exec`・`cloud`・`features` 等）、daemon 管理コマンド、exec、既存の欠落／不正 author を検査する negative case（`startup_requires_a_declared_input_author` を含む）は無差別に変えない。エラー検査の削除・期待値の緩和はしない。

### 原因

`codex-rs/tui/src/lib.rs` の `run_main` が `require_author` を最初に実行するため、TUI 起動経路へ入るテストは作者指定なしではこの gate で終了する。CLI 側の `stdin is not a terminal`（`codex-rs/cli/src/main.rs`）や `--no-daemon` 拒否は gate より前の経路にあり、それらを検査するテストは現状の期待値のままで正しい。`--add-dir`・`sandbox_workspace_write.writable_roots` の拒否は `codex-rs/tui/src/startup_orchestration.rs` 側（gate より後）で発生するため、その経路のテストは作者指定の追加が必要になる。

### 修正対象（テスト起動側のみ・4ファイル）

| 対象 | 起動経路 |
| --- | --- |
| `codex-rs/cli/tests/features.rs` | 共通 `codex_command` は非 TUI にも使われるため変更せず、TUI 起動経路へ到達する 5 テスト専用の helper を追加して適用する |
| `codex-rs/cli/tests/daemon_startup.rs` | 共通 `daemon_startup()` の PTY 起動引数 |
| `codex-rs/cli/tests/worktree.rs` | `worktree_start_and_fork()` の共通 `worktree_args` と、共通引数を経由しない3か所の `rejected_start` 起動引数 |
| `codex-rs/cli/tests/doctor_path_safety.rs` | `interactive_tmux_startup_does_not_execute_workspace_helpers` の PTY 起動引数 |

対象テスト（13件）: `auto_daemon_start_attaches_to_shared_server`、`bedrock_onboarding_leaves_a_running_daemon_untouched`、`daemon_auto_start_preserves_bedrock_onboarding`、`daemon_exclusion_survives_resume_picker`、`daemon_exclusion_survives_fork_picker`、`interactive_tmux_startup_does_not_execute_workspace_helpers`、`interactive_remote_default_preserves_remote_working_directory_before_requiring_terminal`、`interactive_validates_config_before_requiring_terminal`、`remote_start_allows_network_access_overrides_before_requiring_terminal`、`remote_start_rejects_writable_root_overrides_before_connecting`、`remote_start_rejects_add_dir_before_connecting`、`interactive_worktree_start_and_fork_bind_owner_before_turn`、`daemon_worktree_start_and_fork_bind_owner_before_turn`。

## S2 Schema の型識別契約

### 衝突する型

衝突は `codex-rs/protocol/src/user_input.rs` の `ByteRange` と `codex-rs/history/src/compaction_plan.rs` の `ByteRange` の間である。`RolloutLine` は history 所有であり、history は公開 V2 に依存しない。公開 V2 の同名型（`codex-rs/app-server-protocol/src/protocol/v2/turn.rs`）は今回の衝突相手ではない。

### 修正仕様

`codex-rs/history/src/compaction_plan.rs` の `ByteRange` へ `#[schemars(rename = "CompactionByteRange")]` を追加する。次は維持する。

- Rust の型名、serde 上の `start` / `end`、`deny_unknown_fields`
- 保存済み rollout JSON の形（`schema_version`・`applied_refs` 等の roundtrip と厳格性）
- 公開 core / V2 / TypeScript の `ByteRange`
- `codex-rs/app-server-protocol/src/export.rs` の `detect_numbered_definition_collisions`（削除・緩和しない）

history 型を core 型へ共有化すると未知 field 拒否の契約が変わるため採用しない。

### 到達経路と生成物

Compaction 側 schema は `ObservationPartCoverage.presented_ranges` / `unpresented_ranges` から `RolloutLine` へ到達する。実際の生成 Schema で両型が正しく区別され参照できること、Compaction 側の `additionalProperties: false` が維持されること、既存 JSON roundtrip／厳格性が維持されることを確認する。

生成物は既存の正規 generator（`just write-app-server-schema`）でまず新規 scratch へ生成し、差分を精査する。公開 TypeScript / JSON Schema が不変であることを確認した後、管理対象の生成物へ反映し、scratch と内容を照合する。既存の precomputed stable blob には新しい coverage schema がまだ未収録のため、単純 rename 以外の既存差分も内容を確認して反映可否を判断する。巨大な無関係生成物更新はしない。

## 受入

検査はプロジェクト指定の Rust 1.95.0 で `just test` を使い、full suite・CI・all-features・Backend 操作・サービス再起動・配備・commit/push は対象外。Rust の compile 失敗を挙動の red と数えない。実行プロセス自身の終了コードと、期待した件数のテストが選択・成功したことを記録する。選択0件や、`tee` / 後続コマンドの終了コードでは成功と判定しない。

- A（変更前）: CLI features の `interactive_validates_config_before_requiring_terminal` と protocol の `stable_precomputed_exports_match_schema_fixtures` を各々絞り、argv・exit・log を保存する。
- B（変更後 CLI）: 上記13件を codex-cli の4つの integration test target へ絞り、exact test filter で実施する。無関係な全 CLI/TUI テストは走らせない。既存 `require_author` negative test の名前も調べ、最小の対照として実行結果を残す。
- C（変更後 Schema）: `just test -p codex-app-server-protocol --lib -E 'test(schema_fixtures_tests::) | test(precomputed_exports_tests::)' --retries 0`。history は変更した型の既存 serde／compaction validation を必要範囲で絞る。実 RolloutLine スキーマで名前・参照・strictness を確認する回帰検査が必要なら、既存 schema test module へ最小追加する。
- D（generator 入口）: `CODEX_APP_SERVER_SCHEMA_ROOT=<今回の新規scratch> CODEX_APP_SERVER_SCHEMA_EXPERIMENTAL=0 just test -p codex-app-server-protocol --lib -E 'test(=schema_fixtures_tests::write_schema_fixtures_from_env)' --run-ignored only --retries 0`。generator は typescript/json ディレクトリを再生成するため、差分確認は先に scratch で行う。管理対象への再生成後は、公開生成物の差分がなく、stable blob が確認済み scratch と一致することを検査する。
- E（format）: repo の `just fmt` を適用する際、今回触らない既存ファイルの整形差分や他者差分を混ぜない。対象ファイルの format は最後に確認する。今回関係しない既知 lint・format 残件は別残件であり、受入を成功に見せるための検査緩和はしない。

受入の判定は、変更前ログと変更後ログが同じ条件で比較できていることをもって行う。production binary は変更しない。
