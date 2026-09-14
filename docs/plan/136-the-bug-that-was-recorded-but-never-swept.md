# Plan 136 — 记进教训里的那个 bug,只修了撞见的那一处

> 来源:2026-09-11,用户「全面仔细扫描一遍代码,是否有错漏,是否能优化,是否有重复代码,
> 以及架构是否合理」。本 plan 是那次通读(12.5 万行,非测试 5.1 万行)的落地收口。

## 一、教训 111 的尾巴,自己就是一个未清的账

教训 111(c) 结尾记着:

> 某次 `s.replace(anchor, new_test+anchor)` 里 `new_test` 末尾**手抄了一遍 anchor**,于是
> 文件里出现了 `...rides response.completed./// Deltas stream for display...` 这样两段拼在
> 一行的 doc comment,语法合法所以编译测试全绿、没有任何信号,直到这次读文件才看见。

当时修的是**撞见的那一处**(`provider/src/responses.rs`),没有问"同一把脚本还在别处留下过
几处"。这次全仓通读给出了答案:**还有四处**,每一处的形状完全一样——一段 doc 注释停在
它本该属于的项**之前**,被下面那个后插入的项顶着:

| 注释所在 | 它描述的其实是 | 结果 |
|---|---|---|
| `core/src/agent.rs:988` | `STREAM_RESUME_LIMIT`(1006 行) | 顶在 `struct Ending` 头上 |
| `core/src/history.rs:458` | `estimated_tokens`(478 行) | 顶在 `effective_window` 头上 |
| `core/src/permissions.rs:1949` | `path_is_sensitive`(1969 行) | 顶在 `path_tail_is_spill` 头上 |
| `tui/src/app.rs:1787` | `cells_from_history`(1813 行) | 顶在 `injected_label` 头上 |

四个被抢走注释的项现在都**没有文档**,而抢到注释的项顶着一段说别人的话。`cargo doc` 会把
错的那段原样发布到错的项上。

这条教训因此要补一句:**一个"编译器不会说话"的 bug 被记进教训时,同时要扫一遍全仓的同类,
否则教训只覆盖了撞见的那一个坐标。**

## 二、同一族:编译器不会管的其余几处

通读里另外几处"语法合法、测试全绿、但确实是错的":

1. ~~**`core/src/tools/mod.rs:1170` 同一次调用抄了两遍。**~~ **通读判错,实现时被测试推翻,
   见第七节。** 原判断:参数相同、紧邻、中间无 `await`,命名 before/after 像是重构残留。
   实际:那是**刻意的双采样**。改动落地后
   `catalog_appearance_during_classification_still_requires_discovery` 立刻红——它的
   fixture `AppearingSrv` 前两次 `defs()` 返回空目录、第三次才返回工具,模拟的正是"工具在
   分类过程中才出现在目录里"。本轮只给这两行补上说明它为什么是两次的注释。

2. **`core/src/shell.rs:123` 死分支。** 含 `&;|` 的 kind 一定不是空白,所以第一个 if 命中的
   第二个必然也命中。白名单遍历是安全判定的核心(教训 8),多一个看起来在做事、实际什么都
   不决定的分支,是读者的负担。

3. **`core/src/permissions.rs:1436` `rules_hit` 的 `strip_for_match` 是死参数。** 两个调用点
   (deny/ask)都传 `true`,没有 `false` 调用者。删掉正好也消掉一个违反本项目风格的 bool
   位置参数。

4. **`core/src/skills.rs:298` `map_tool_name` 漏了 `NotebookEdit`。** 教训 28 立的规矩是"开放
   映射:命中改写、未命中透传",但这条只在**映射表本身完整**时成立。一个下载来的 skill 写
   `allowed-tools: [Read, NotebookEdit]`,`NotebookEdit` 透传进 allowlist,于是 fork 子 agent
   实际**拿不到** `notebook_edit`——透传在这里不是兜底,是静默丢能力。同理补
   `AskUserQuestion` / `ExitPlanMode`。

5. **`core/src/skills.rs` `$ARGUMENTS` 是前缀匹配。** body 里的 `$ARGUMENTS_EXTRA` 会被换成
   `<args>_EXTRA`。

6. **`core/src/inbox.rs:325` `remove_local_pending` 只有 `debug_assert` 防下溢。** release 下
   多减一次就 wrap 成 `usize::MAX`,`is_empty()` 从此永远 false,TUI 的空闲自动唤醒会持续
   触发投递轮——一个只在 release 出现、且表现为"agent 自己不停说话"的故障。

7. **`core/src/permissions.rs` 模块头的管线列表少了一层。** 头部 `//!` 列了 12 层,实现里在
   "5. ask rules"和"6. sandbox auto-allow"之间还有一层 scheduler 控制直接 `Ok(None)`。权限
   管线的模块头就是这个文件的规格说明,漏一层等于规格与实现不符。

## 三、重复:改一边会忘另一边的那几处

按"分叉代价"排序,只收本轮能安全收的:

1. **provider provenance 校验写了两遍**——`rollout.rs:validate_provider_message` 与
   `history.rs:provider_request_view`,六段判断(route 定位、interval_end、origin_boundary 区间、
   model_matches、identity 四字段、api_family/redacted 形状)逐条对应、措辞一致但各一份。这是
   最危险的一处:任何一侧改规则都会**静默**分叉,而两边都是 reasoning 重放的安全判定。抽到
   `provider_route.rs` 一个共享函数,两处各自保留自己那一半特有逻辑(rollout 的
   `original_line` 边界比对、history 的 chat_target / sanctioned_switch 授权)。

2. **`rollout.rs:995/1012` 把 `ContentBlock::has_reasoning` 抄了一遍**——与
   `protocol/src/lib.rs:288/305` 逐字相同(只是 `Self` → `ContentBlock`)。

3. **`permissions.rs` 内部五处**:`describe`/`describe_parts` 的 detail 抽取 match(还一个用
   `.chars().take(200)` 一个用 `clip()`);`Rule::matches_argv` / `matches_escalation_argv` 的
   前缀比较;`parse_rule` 里 `bash` / `sandbox_escalate` 两个分支的 token 切分;`ask_user` 与
   `escalate_sandbox` 的会话缓存写入;**`remember_payload` 与 `escalation_remember_payload`
   的 bash 前缀记忆**——`take(2)` 前缀、坏 token 校验、rule/signature 去重三段逐字相同,只差
   规则词汇和是否跳过只读段(这一条初稿漏列,见第八节)。

4. **`rollout.rs:1359 legal_cut_seqs` 与 `:1392 fork_points` 同一套遍历**——注释自己都写着
   "seqs 恰好是 legal_cut_seqs 的非末尾项"。

5. **`subagent.rs:333/611/812` 三处 spawn 工作体逐字相同**,只差最后调哪个
   `run_*_in_execution` 和用哪个 cancel token。

## 四、便宜的性能账

只收"改动局部、收益确定"的三条:

1. **`tools/mod.rs:493 reserved_builtin_names()` 每次调用重建全部内置 `ToolDef`**(十几个带
   大段 schema 的 `json!`,只为取 name),而 `resolve_source` 在**每次工具调用解析**时都走它。
   改成 `LazyLock<HashSet<&'static str>>`。名字集合从此与 def 构造解耦——这也是教训 75 的
   形状(公开 schema / 执行器解析 / registry 是三层独立边界)。
2. **`config.rs` 五个 `effective_*` 便捷函数各克隆一整个 `EffectiveWorkspace`**,含**整个
   system prompt String**(数 KB),只为取一个字段;`effective_cwd()` 在工具层高频调用。
3. **`all_tool_defs` 重复合并**:自己算一遍 `merged_source_defs`,`deferred_tool_defs` 内部
   再算一遍。

## 五、非目标(留后续 plan)

- ~~**工具中心注册表。** 一个工具的事实散在 5+ 处并行 match(`builtin_defs` / `is_concurrency_safe`
  / `CallFacts::is_readonly` / `execute_tool` / 两张保留名表,外加 `toolrow.rs` 显示名与
  `tool_title`),漏改**不会编译报错**。这是本轮通读里最值得做的架构改动,但它要动工具层的
  形状,单独一条 plan。~~ ✅ **plan 138 已做**(`tools/builtin.rs` 的 `Builtin` 枚举;顺带
  查出 `reserved_names` 漏了 `run_program`/`stop_program`,整表改从枚举派生)。
- **拆超长函数**(`turn_rounds` 548 / `responses::stream` 425 / `App::apply_core` 367 /
  `main` 366 / `run_one` 349 / `ui_loop` 339)。
- **SSE 驱动循环抽取**(三个 provider 适配器各写一遍 `send_checked → SseParser → GuardedBody`
  骨架)、**`fs.rs:atomic_replace` unix/windows 两份 80 行合并**、**测试 Config fixture 统一**
  (5 份手抄 30+ 字段)、**Config 的两个克隆构造改 `Clone` + `..base`**。
- **scheduler 两条性能账**:`CronSpec::next_after` 逐分钟扫 52 万次(一次 `cron_create` 最多
  跑三遍),以及有 durable store 时 worker **每秒**一次 flock+读盘+解析。两者都要动调度语义
  (跳跃式扫描的正确性、轮询间隔与跨进程可见性的权衡),不该混在清账里。
- **`argv_is_dangerous` 只认 `rm` / `sudo`。** 模块头写明 safety checks 免疫 bypass,所以这是
  bypass 下唯一还会弹确认的一层;但 `dd of=/dev/...`、`mkfs`、`shred`、`git clean -fdx`、
  `git reset --hard`、`chmod -R 777 /` 全是 word-only,会静默直跑。**扩不扩表是安全策略决定
  (扩了 bypass 下确认变多),开工时问用户**,不在本轮自行决定。

## 六、验收

每一处改动都要有一个**会因为改回去而变红**的测试,而不是"跑一遍没炸"。doc 错位这类编译器
不管的,测试就断言那段文字挂在正确的项上(用 `include_str!` 读自身源码做结构断言,这是唯一
能锁住它的手段)。

## 七、实现时被推翻的一条

第二节第 1 条(两次 `is_deferred`)判错了,而且是**通读两遍都判错**。删掉一次之后
`catalog_appearance_during_classification_still_requires_discovery` 立刻红:它的 fixture
`AppearingSrv` 前两次 `defs()` 返回空目录、第三次才返回工具——模拟的正是"一个工具在分类
过程中才出现在目录里"。动态源会在两次观察之间发布新目录,而**任何时刻被看到是 deferred 的
工具都还没走过发现**,所以两次采样取 `||` 是正确的,不是残留。

判错的原因很具体:`before`/`after` 这两个名字在没有中间操作时读起来像"重构后忘了删",而
真正的意图(两次采样)只活在一个测试的名字里。修法因此不是删,是把这段理由写进注释——一段
连作者本人两遍都读错的代码,缺的就是那句话。

## 八、初稿漏列的一条

第三节第 3 条初稿写的是"四处",而通读报告里 permissions 内部本来就是四条:
`describe`/`describe_parts`、`remember_payload` 那对、会话缓存写入、`Rule` 前缀比较 +
`parse_rule` 两分支。搬进 plan 时我把最后一条**拆成了两条**,凑够四条就往下走——于是
`remember_payload` 那对被挤出清单,既没做,也没进第五节的非目标。用户回头问"其他的都改了
吗",逐条核对才发现。

教训是清单转录的:**把一份清单搬进另一份文档时,按原清单的条目逐条勾掉,不要按数量对**。
数量相同是最容易骗过自己的一致性检查——尤其当搬运过程中还顺手拆分或合并了条目。

## ✅ 已完成(2026-09-11;提交 SHA 以本条所在提交为准)

**一、doc 归位。** `core/src/agent.rs`(`STREAM_RESUME_LIMIT`)、`core/src/history.rs`
(`estimated_tokens`)、`core/src/permissions.rs`(`path_is_sensitive`)、`tui/src/app.rs`
(`cells_from_history`)各自拿回自己的注释,四个原本顶着别人文档的项换回自己的。

**二、死代码与错漏。** `shell.rs` 删掉被下一行完全覆盖的 punct 分支;`permissions.rs`
`rules_hit` 去掉两个调用点都传 `true` 的 `strip_for_match`;模块头与 README 的权限管线补上
`session scheduler controls` 那一层;`skills.rs` 的 `map_tool_name` 补 `NotebookEdit` /
`AskUserQuestion` / `ExitPlanMode`,并把"开放映射的前提是表本身完整"写进注释;`$ARGUMENTS`
加标识符边界;`inbox.rs` 的 `remove_local_pending` 改饱和减,**并删掉那个 `debug_assert`**
——它只在 wrap 不可能发生的构建里生效,而饱和路径因此也无法被测试覆盖;`tools/mod.rs` 的
双采样按第七节保留并补注释。

**三、重复收口。** `provider_route.rs` 新增 `ReasoningShape` / `ProvenanceMismatch` /
`validate_provenance`,rollout 的落盘校验与 history 的请求投影从此共用同一份五条规则,各自
只留错误措辞、rollout 的行边界比对(`origin_line_boundary`)和 history 的
chat_target / sanctioned_switch 授权;`rollout.rs` 删掉手抄的两个 `has_reasoning`,改用
`kloop_protocol` 的;`cut_points` 收口 `legal_cut_seqs` 与 `fork_points`;`permissions.rs`
抽出 `argv_has_prefix` / `parse_prefix_pattern` / `remember_in_session` / `call_detail`;
`subagent.rs` 抽出 `child_history`,三处 15 行的 spawn 开场变成 4 行;`bash_prefix_memory`
+ `ReadOnlySegments` 收口 `remember_payload` 与 `escalation_remember_payload`(第八节)。

**四、性能。** `reserved_names()` 合并原来的 `reserved_builtin_names` +
`reserve_surface_names` 并改 `LazyLock`——`resolve_source` 每次工具调用都查它,之前每查一次
就重建十几个带完整 schema 的 `ToolDef` 只为读 `name`;`partition_source_defs` 让
`all_tool_defs` 合并一次目录而不是合并两遍再相减;`config.rs` 的 `effective_field` 让五个
`effective_*` 只克隆需要的那一项,不再为取一个 `PathBuf` 复制整个 system prompt。

### 测试

- 新增 `crates/cli/tests/doc_placement.rs` 四条:按"关键短语必须落在正确的项上"钉住四处
  归位(注释可改写,位置不能)。放在 cli 的集成测试里是因为它跨 core 与 tui 两个 crate,
  是仓库级不变量。**反向验证过**:把 `agent.rs` 改回错位,该测试报
  `` `const STREAM_RESUME_LIMIT` should carry the documentation about "resuming a turn", but has: `` 并失败。
- `skills.rs` 两条:`every_cc_tool_with_a_kloop_counterpart_is_mapped`(三个新映射 + 一个
  真没有对应物的名字仍然透传)、`arguments_placeholder_stops_at_the_identifier_boundary`
  (`$ARGUMENTS_EXTRA` / `$ARGUMENTS2` 不再被吃掉,且因为没用到占位符而走追加分支)。
- `inbox.rs` 一条:`releasing_more_local_pending_than_was_added_saturates_at_empty`,过度
  释放后计数归零、`is_empty()` 为真、之后仍可正常加减。**反向验证过**:改回 `fetch_sub`
  即红。
- `tools/mod.rs` 一条:`the_reserved_set_covers_the_catalog_the_surface_and_the_retired_names`
  ——把两张手写列表合成一张缓存集合,只有"什么都没掉"才安全,而掉出去的名字会被 MCP 服务器
  占用。
- `permissions.rs` 一条:`the_two_bash_memories_differ_only_in_vocabulary_and_read_only_segments`
  ——同一条 `ls -la && git commit -m x`,普通 gate 只记 `bash(git commit *)`(只读段跳过、
  不交出 argvs),升级门记两条 `sandbox_escalate(...)`;全只读的命令前者无可记、后者照记;
  前缀含空白的可解析命令两边都拒。**反向验证过**:把升级门的 `ReadOnlySegments::Remember`
  改成 `Skip`,立刻丢掉 `sandbox_escalate(ls -la *)` 并变红。
- 其余(去参数、去重、分区、effective_*)是行为等价改写,由既有测试保证:`cargo test
  --workspace` 全绿,其中 `kloop-core --lib` 809 条。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace` 三条**各自单独跑、当场取退出码**(HANDOFF 111(b2)),依次 0 / 0 / 0。
