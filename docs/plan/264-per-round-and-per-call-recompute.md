# Plan 264 — 每轮与每次工具调用的重复计算

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 2026-11-17 完成。行号是 2026-10-28 的读数，实际按函数定位;第 3–6 条开工复核
> 均成立。权限一侧"同一命令解析 5–7 次"在 plan 266。

## 问题

1. **每轮把整个历史深拷贝两遍**(已确认)。`core/src/history.rs:988-992` `provider_request_view`
   对每条消息 `message.clone()`;`core/src/agent/sampling.rs:117-126` 为在最前面插 injected context
   又 `Vec::with_capacity(len+1); push(ctx); extend_from_slice(messages)`。带几 MB 图片的会话每轮多拷
   MB 级数据。
2. **估算闭包写两遍，`injected_context` 每轮最多算 3 次**(已确认)。`agent.rs:1108-1115` 与
   `794-801` 两段 `request_overhead_tokens(&workspace.system, &tools, injected_context(..))`;
   `sampling.rs:117` 又算一次;深度 0 时 `estimated_tokens()` 每轮算 2 次(每次序列化锚点之后全部消息)。
3. **每次工具调用两次完整的 defer 分区计算**(待复核)。`tools/mod.rs:1124-1125` → `tool_search.rs:196-200`
   → `mod.rs:603`:`run_gated` 无条件 `classify_source`;对内置名 `current_source_route` 因
   `reserved_names` 快速返回 Missing,随后仍 `is_deferred(name)` 两次，内部
   `deferred_tool_defs(..).iter().any(..)` → `partition_source_defs` → 克隆全部 source 的 ToolDef 并
   `tool_defs(0, ..).len()` 重建约 15 个内置 `json!` schema,只为计数。挂上百个 MCP 工具时每个内置调用
   都要做。`reserved_names` 的注释(`mod.rs:622-624`)点名过这种浪费。
4. **`select:` 为判断"已加载"重建全部定义**(待复核)。`tool_search.rs:281-287` 调完整
   `all_tool_defs`,开 program surface 时重新生成 run_program 的整份 TypeScript API,只为比名字;`:267`
   算好的 `deferred` 没复用。
5. **`tool_merge_warnings` 手写一遍合并再调一次合并**(待复核)。`mod.rs:662-681`:668-679 行手写
   seen/insert 遍历，680 行又 `merged_source_defs` 加 `tool_defs(0)`;`all_tool_defs` 在 depth 0 且开
   program 时 `:529` 与 `:552` 各算一次相同的 `builtin_defs(0)`。
6. **(顺带修 bug)`+term` 必选判断比打分窄**(待复核)。`tool_search.rs:406`
   `term.required && !(name_substring || description_hit || schema_hit)` 少了 `name.contains(term)`;
   `+send_message` 会把 `slack__send_message` 排除，而去掉 `+` 时 `:411` 靠 `name.contains` 能找到。

## 做法

1. `request_view` 预留第 0 个位置，或交出视图所有权后 `insert(0, ctx)`(只移动指针)。
2. 抽 `Turn::estimate()`;每轮算一次 injected context 并传给 `sample_with_retry`。
3. `classify_source` 对 `Builtin::from_name(name).is_some()` 直接判不 defer;内置计数改
   `builtin::ALL.iter().filter(|t| t.in_catalog(0, sp)).count()`;`is_deferred` 先判是否超阈值，再查
   单个名字。
4. `select:` 只算名字集合：内置用 `Builtin::from_name` + `unoffered(..).is_none()`,source 用
   `merged_source_defs` 减去 `deferred`。
5. 抽 `merge_source_defs(sources) -> (defs, collisions)` 两处共用;`all_tool_defs` depth 0 复用 `:529`
   的结果。
6. 先算 `name_hit = name_substring || name.contains(..)`,必选判断与打分都用它(单独提交、带测试)。

## 验收

- 优化提交行为零变更：agent/tools 现有测试不变;补计数型测试(测试钩子统计 `tool_defs` 构建次数、
  历史 clone 次数),断言每次内置调用不再重建 schema、每轮 injected context 只算一次。
- 第 6 条单测先红后绿。
- 外层执行沙箱之外 `make check`;HANDOFF 记进展。

## 完成记录(2026-11-17)

- **请求视图**：`sample_with_retry` 改为接收 `Vec<Message>`(调用方从 `request_view` 交出所有权)，
  合成首条消息用 `insert(0, ..)` 移动，不再 `Vec::with_capacity + extend_from_slice` 深拷贝整段
  历史。重试仍复用同一份字节。
- **每轮一次**：`Turn` 增加 `injected: Option<String>`，在回合边界(刷新工具与提醒之后)构建一次；
  预测估算、溢出上限估算、请求本体和每轮 Usage 都读这一份。`sample_with_retry` 不再收 `depth`。
- **按名字判定 defer**：新增 `source_tool_deferral`(先判阈值、按 merge 顺序找单个名字，不克隆
  ToolDef)与 `builtin_count`/`merged_source_count`;`classify_source` 对 `Builtin::from_name`
  直接返回不 defer，内置调用不再走两次完整分区。`tool_search::is_deferred` 复用前者。
- **`select:` 不建定义**：新增 `tool_name_is_inline`，与已算出的 `deferred` 配合只比名字
  (内置走 `in_catalog`/`in_surface`,`tool_search`/`call_tool` 看 defer 期)，不再为比较字符串
  重建 run_program 的整份 TypeScript API。
- **合并共用**：`merge_source_defs` 同时返回定义与冲突消息，`tool_merge_warnings` 不再手写一遍
  合并;`all_tool_defs` 在 depth 0 开 program 时复用已构建的 catalog，`partition_source_defs`
  用 `builtin_count` 计数。
- **第 6 条**：单独提交，回归 `a_required_term_matches_across_the_name_separator` 先红后绿。
- **验证**：kloop-core lib 1440 通过、1 ignored(含新增：每轮 injected context 只构建一次、
  内置调用/`select:` 不重建内置 catalog、按名字判定不建定义);`tools::` 503 项、
  `clippy -p kloop-core --all-targets -D warnings`、`cargo fmt --all`。未跑全量 `make check`、
  parity、真实 API。
