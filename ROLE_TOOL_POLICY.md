# 役割ごとの許可toolとsandboxの縮小

2026-09-28制定。状態: 2026-09-29にLinuxへ配備し、「検査」の実経路を確認済み（f0728b1bf、記録はRenCrow_LLMの`artifacts/tools-schema-fixed-20260929.md`）。本書はForkの仕組みだけを所有する。どの役割に何を許すかという製品の方針は
RenCrow_LLMの`docs/07_運用・セキュリティ方針.md`「統一Codex profileの役割ごとの許可tool」が所有する。

## 目的

LLMは、読んだfile、コマンドの出力、処理を頼まれた文章の中に書かれた指示に従ってしまうことがある
（2026-09-28の害の無い埋め込み指示の試験で、GPT-OSS-120B、Qwen3.6、GLM4.7がtool呼出しを試みた）。
LLMの自制に頼らず、役割に要らないtoolをmodelへ見せず、呼ばれても実行しないことで、従った場合の被害を狭める。

## 設定

### `rencrow_tool_allowlist`

- 置き場所: top level（profileまたは`CODEX_HOME/config.toml`）と、`agents.<role>.config_file`の役割file。
- 値: 文字列の配列。既定namespaceのtoolは名前だけ（例 `exec_command`）、それ以外は`namespace::name`
  （例 `multi_agent_v1::spawn_agent`、`mcp__rencrow_advisor::delegate_llm`）で1つずつ明示する。namespace単位の一括指定はしない
  （上流の`ToolPolicy`が完全一致で照合するため。一覧が固定revisionとしてそのまま読める）。
- 未指定: 上流と同じ（許可一覧による絞り込みをしない）。空の配列: toolを一つも見せない。
- 役割fileの値は、親から引き継いだ許可一覧との共通部分になる。役割でtoolを増やすことはできない。
- どのtoolにも一致しない要素は何もしない（許可を広げる方向には倒れない）。

### `rencrow_read_only`

- 置き場所: 役割fileだけ。`true`のとき、その役割の子agentのsandboxを読み取り専用にする。
- 親から引き継いだ権限より狭めることだけができる。`false`や未指定は親の権限をそのまま引き継ぐ。
  読み取り専用では、workspaceと`/tmp`への書込み、localhostを含む通信ができない。

## 強制点

新しい仕組みは作らず、上流の`ToolPolicy.allowed_tools`（guardian reviewerも使うsessionのtool制限）へ流し込む。

1. session開始時: 許可一覧を`ToolPolicy.allowed_tools`へ変換する。拡張機能が既にpolicyを決めている場合は共通部分にする
   （既存の制限を緩めない）。
2. 見せない・実行しない: 上流のregistryは許可外のtool（組込み、MCP、hosted `web_search`を含む）を登録しない。
   そのためmodelの入力に現れず、modelが名前を推測して呼んでも未知のtoolとして実行されない。
   turn途中の要約（COMPACTION_SPEC.md 第3部 提案A）も同じ登録済みtoolを使うため、prefix cacheの前提は変わらない。
3. 内部要求: Compactionの各段階と要約の応答に含まれるtool呼出しは実行しない（既存の挙動を回帰testで固定する）。

## 互換・失敗挙動

- 設定を切れば上流と同じ挙動に戻る（FORK_RULES.md 改変原則8）。codex-switchは常に設定する。
- 許可一覧の外のtoolは、model入力にも実行経路にも現れない。一覧の誤りで必要なtoolが見えなくなることはあるが、実行を広げる方向には倒れない。配備時にprompt全文debug記録で各役割の一覧を確認する。

## 検査

- 許可一覧から作ったpolicyが、一覧のtoolだけを許し、namespace付きのtoolを名前だけで呼べないこと。既存policyの制限を緩めないこと。
- 役割fileの一覧が親の一覧との共通部分になり、役割でtoolを増やせないこと。
- `rencrow_read_only`の子agentのsandboxが読み取り専用になり、親のsandboxは変わらないこと。
- 未指定時のtool一覧が上流と同じこと。
- 実経路: codex-switchで主・worker・explorerを起動し、prompt全文debug記録で各役割に届いたtool一覧を確認する。

## 戻し方

設定を外すか、本機能のcommitをrevertする。上流で役割ごとのtool制限とsandbox縮小が提供された場合は、受入条件を比べて本機能を撤去する。
