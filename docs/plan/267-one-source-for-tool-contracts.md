# Plan 267 — 工具契约单一来源

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成(2026-11-17)。plan 257 已完成,依赖满足。
> 行号是 2026-10-28 的读数，开工先自己量。

## 问题

工具的输入契约分散在三处，彼此不受约束：

1. **schema 手写**:`core/src/tools/builtin.rs` 里 15 处 `json!`;**解析另写**:6 个 serde 结构体
   加一批手写 helper;**TS 声明又手写**:`tools/codemode.rs:789` 的 `type AgentOptions`。
   plan 257 第 1 条(`max_rounds` 已从 `RunAgentInput` 删掉，TS 声明与两个 bridge 仍在用)就是
   三处分头演化的结果。
2. **参数 helper 各写一份，语义已分叉**:
   - `strict_object`:`tools/agent_message.rs:186`、`todo.rs:277`、`scheduler.rs:321`、
     `worktree_tool.rs:116`(加 `:122` `reject_unknown`),`mod.rs:1916` `strict_str_arg`;
   - `optional_string`:`worktree_tool.rs:129`、`search.rs:117`、`notebook.rs:603`;
   - `required_string`:`agent_message.rs:204`、`scheduler.rs:335`、`mod.rs:1909` `str_arg`;
   - `integer_arg`:`fs.rs:489-516` 与 `search.rs:139-166` 逐字相同;
   - `bounded_diagnostic`:`agent_message.rs:225` 按字节、`todo.rs:296` 按字符;
   - 分歧：未知字段报错措辞三种;显式 `null` 在 worktree_tool 报 "must be a string"、在
     search/notebook 当省略;agent_message/todo 回显未知字段名时限长并去控制字符，mod.rs、
     worktree_tool、scheduler 原样回显任意长的 key。
3. **两个 `call_agent` bridge 几乎逐行相同**。`codemode.rs:658-719` 与 `workflow.rs:456-517`:
   journal claim → 计数 → 带取消的信号量 → 拷贝选项 → 调用 → 记录;要拷贝的选项键各维护一份。
4. **structured agent 的输入手工解析**。`subagent.rs:228-394` 用 `input["isolation"].as_str()`,
   `isolation: 1` 被静默当成 shared;`run_agent` 路径用 serde 会报错。注册 → spawn → panic 处理 →
   worktree finish → emit_end 的流程在 `:228-394` 与 `:570-670`、`:782-799` 重复。
5. **send_message 校验两层各写一份**。`agent_message.rs:110-160`(`parse_send_message`)与
   `agent_mailbox.rs:629-686`(`validate_payload`、`bounded_roster_text`)规则一模一样;
   `is_unicode_line_separator` 定义两次，`todo.rs:213`、`:300` 又内联一份。
6. **过期注释**:`mod.rs:1870-1883` `provided_keys` 说 "kloop translates no synonyms",与 `:858`
   `ARGUMENT_SYNONYMS` 矛盾;`builtin.rs:439-443` 原属 `concurrency_safe` 的文档挂在了 `timeout` 上。

## 开工先问用户

- ~~要不要引入 schemars?~~ 用户定:**不引入**,按 (b) 起步:保留手写 schema,用一致性测试堵漂移;schemars 留作后续。
- ~~显式 `null` 统一当省略还是统一报错?~~ 用户定:**统一当省略**;必填字段仍报缺失。

## 做法

1. 按问答建立 schema 与解析的一致性约束;`AgentOptions` 的 TS 声明从 `run_agent` 的 schema 生成
   (`OrchestrationScope` 等编排类型仍手写)。
2. `tools/args.rs`:`object`、`reject_unknown`、`required_str`、`optional_str`、`optional_bool`、
   `integer`,统一 null 语义与措辞，回显一律限长去控制字符;各工具改用它。
3. 抽 `AgentCallGate { journal, count, max, slots }`,两个 bridge 只保留各自的选项映射与 admit 函数。
4. structured 路径改用 `deny_unknown_fields` 的 serde 结构体;运行与收尾抽成泛型 helper,用闭包传入
   `run_turn` 与 `run_structured_turn` 两种变体。
5. 工具层调 mailbox 的 `validate_payload` 再加自己的前缀;`is_line_separator` 只留一份。
6. 修两处注释。

## 验收

- 每个工具：schema/解析一致性测试;未知字段、`null`、超长 key 的报错形状统一(整对象断言)。
- 措辞变化会影响模型看到的错误文本：逐条列进完成记录，现有 parity 语料若钉了旧措辞要说明。
- `isolation: 1` 在 structured 路径报错;两个 bridge 的现有测试不变。
- 外层执行沙箱之外 `make check`;DESIGN 的工具参数校验一节先读再改;HANDOFF 记进展与教训。

## 完成记录（2026-11-17）

六条全部成立并按 (b) 落地。

1. **一致性测试**：`builtin.rs` 新增 `every_schema_matches_its_parsers_accepted_keys`，
   走 `Builtin::ALL`，逐工具断言 `schema.properties` 键集合 == 解析接受的键集合；
   `workflow` 的 `name` 是解析器会读、schema 漏掉的字段，补进 schema 并说明
   “named workflow registry 未实现”。`read_file`/`write_file`/`edit_file`/`grep`/`glob`/
   `exit_plan_mode`/`skill` 的 schema 补 `additionalProperties: false`。
2. **`tools/args.rs`**：`object`/`strict_object`/`required_str`/`optional_str`/
   `optional_bool`/`bool_or`/`integer_arg`/`bounded_diagnostic` 一份；显式 `null`
   统一当省略，回显按字符限长去控制字符；缺失参数仍列出调用实际携带的其它 key。
   各工具删掉本地 helper 并改用它；原先未设防线的工具（bash_output、powershell、
   grep/glob、read/write/edit、plan mode、skill、tool_search）改为拒绝未知字段。
3. **`tools/agent_gate.rs`**：`AgentCallGate` 统一 journal claim、总数上限、
   live 槽位与取消复查；两个 bridge 只保留各自的选项映射和错误措辞。取消检查仍在
   journal hit 之前，保持“停止后不再起新子 agent”。
4. **structured 路径**：`StructuredAgentInput` 用 `deny_unknown_fields` serde；
   `isolation: 1`、拼错字段、`max_rounds: 0` 均报错。注册→spawn→supervisor→
   worktree finish→emit_end 抽成 `run_supervised_child`，`ChildTurn::{Plain,Structured}`
   选择 turn 变体，`finish_plain`/`finish_structured` 映射结果。
5. **`send_message`**：`parse_send_message` 只派生 summary 和目标，payload 规则调用
   `agent_mailbox::validate_payload` 后再加 `send_message:` 前缀；mailbox 内错误文案
   去掉 “local Agent message” 前缀。`is_unicode_line_separator` 只在 `args.rs` 定义。
6. **注释**：`provided_keys` 的 “kloop translates no synonyms” 改成只承认
   `ARGUMENT_SYNONYMS` 的精确无歧义重命名；`concurrency_safe` 的文档从 `timeout` 移回。

**AgentOptions**：`codemode::agent_options_type` 从 `builtins` 里的 `run_agent` schema
投影 `agent_type`/`model` 生成；`ts_type` 支持 `["string","null"]`。编排类型仍手写。

**模型可见措辞变化**（原有断言同步更新）：
- 未知字段：`unexpected parameter 'x'` / `unexpected input field "x"; accepted: …` /
  `unsupported field 'x'` / `unknown field 'x' in each todo` → 统一 `<tool>: unknown field 'x'`。
- 类型错误：`'key' must be a string` / `when provided` → `<tool>: key must be a string` 等。
- `send_message` 的 payload 错误去掉了 “local Agent message” 前缀，由工具加
  `send_message:`；`summary: null` 改为派生 summary。
- powershell 未知字段不再附 “foreground-only” 解释。
- `tool_search.max_results: null` 与数字字符串改为省略/接受；负数/小数报
  “whole number of 0 or more”，0 仍是 “positive integer”。
- plan 58/56/53 的 parity 断言按新措辞更新；本机未跑外部 parity 语料。

**验证**：`cargo test -p kloop-core --lib` 1451 通过、1 ignored（含新增
schema/parser 一致性、`args` null/限长、structured 严格输入）；`cargo clippy -p
kloop-core --all-targets -- -D warnings`；`cargo fmt --all -- --check`；
`cargo check --workspace`。未跑 `make check`、外部 parity 语料、真实 API。
