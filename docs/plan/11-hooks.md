# Plan 11 — Hooks ✅(0839d61)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:codex hooks crate、cc 的 hook 语义(可阻断、可注入上下文)。

**完成记录**:按设计要点全部落地(hooks.rs 归 core、tokio::process、pre_tool 先于权限门、阻断产物、子 agent 继承)。补充定案:阻断理由取 stdout>stderr>exit status;post_* 非 0 只告警;spawn 失败与超时同样放行+告警(fail-open,权限门才是强制层);同事件多 hook 首个阻断短路;matcher 精确匹配;stdout 注入形态 `[{event} hook]\n…`,工具 hook 的经 ToolCtx.hook_context sink 在该轮 tool_results 后记录;`Config` 新增 `session_id` + `hooks: Arc<Hooks>`。测试 143→158。手工验收双轨真 key 过:sonnet-5 与 gpt-5.4-mini 均调 `bash rm`,hook 阻断、模型收到 `blocked by hook: policy: rm is banned…` 后改口,文件未删。

## 目标

四个挂点的外部命令钩子:turn 开始前 / turn 结束后 / 工具执行前 / 工具执行后。

## 设计要点

- **配置**:`.kloop/config.toml` `[[hooks]] event = "pre_tool" command = [...] matcher = "bash"`(matcher 只对工具事件有意义,匹配工具名)。
- **协议**:事件 JSON 走 stdin(事件名、工具名、输入、会话 id),退出码语义:0 放行、非 0 阻断(pre_* 事件);stdout 有内容则作为附加上下文注入历史(user 消息,cc 的形态)。超时(默认 10s)按放行处理并告警。
- **归属**:`crates/core/src/hooks.rs`——体量不够开 crate;执行复用 tokio::process。
- **与权限的关系**(若 Plan 8 已做):pre_tool hook 在权限判定**之前**跑(hook 是自动化策略,权限是人的最后一道);hook 阻断则不再询问。
- **阻断的产物**:pre_tool 阻断 → is_error tool_result("blocked by hook: ...");pre_turn 阻断 → turn 不启动,提示用户。
- 子 agent 继承同一套 hooks。

## 测试

四挂点触发时机(mock 脚本写标记文件);阻断语义;stdout 注入形态;超时放行;matcher 过滤。

## 完成标准

fmt/clippy/test 全绿;手工验收:配一个 pre_tool hook 拦 rm,确认模型收到阻断信息;README 更新。
