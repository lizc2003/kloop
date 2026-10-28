# Plan 267 — 工具契约单一来源

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：待实现。**先做 plan 257**(它修掉 `max_rounds` 这个具体 bug,这里消除它的成因)。
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

- 要不要引入 schemars(新增直接依赖，按仓库惯例要在 workspace `Cargo.toml` 写明理由)?
  (a) 引入：每个工具一个 `#[derive(Deserialize, JsonSchema)] #[serde(deny_unknown_fields)]` 输入
  结构，schema 由它生成;生成的 schema 与现有手写 schema 逐字对比(措辞、`description`、
  `pattern`、上下限),不一致的逐条决定。(b) 不引入：保留手写 schema,但每个工具加一条测试，
  断言 schema 的 `properties` 键集合 == 解析结构体接受的键集合。推荐 (b) 起步，代价小、立刻
  堵住漂移;(a) 留作后续。
- 显式 `null` 统一当省略还是统一报错?推荐当省略(模型常发 `null` 表示"不填")。

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
