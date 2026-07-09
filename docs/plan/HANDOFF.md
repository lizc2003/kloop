# HANDOFF — 当前状态与会话交接

> 全局约束(定位、工作方式、风格、偏好)在根目录 CLAUDE.md(自动加载)。本文件是细节:读完即可开工,无需重新调研。参考库知识在 `refs/README.md`。

## 一、已拍板的架构决定(P0,2026-07-09)

1. 编辑工具用 Edit(old_string/new_string)形态,不迁就 apply_patch(实测 gpt-5.4-mini 都能首试用对)。
2. 目标模型双轨:Claude(sonnet-5)为主、OpenAI-compat 为副。
3. 对 codex 上游只保持"可跟随性",不追求可合并。

## 二、当前状态(plan 1–7 完成)

**结构**:Cargo workspace,四 crate 严格单向依赖链(详见 `kloop/README.md` Layout 节):
`kloop-protocol`(零依赖线格式)← `kloop-provider`(适配缝,独占 reqwest)← `kloop-core`(agent 本体,无网络)← `kloop`(cli)。

**能力**(全部真实 API 验证过,除注明):
- 5 个原始赌注:append-only 历史 + 录入时 offload(>8000 字符落盘 + 指针 + read_offloaded 回读)、tool_use 有无判续跑、按入参动态并发(连续安全调用并发批)、task 子 agent 递归复用 run_turn(深度限 1)、provider 适配缝(Anthropic SSE / OpenAI-compat / Mock)。
- 压缩双防线:predictive(采样前预判 当前+增长预留 是否爆窗,增长 = min(输出上限,20k)+15k,**窗口≤预留时跳过**)+ reactive(溢出错误 downcast OverflowError,每 turn 压缩一次重试)。压缩 = 模型写交接摘要 + 保留约 2k token 近期原文,边界绝不切开 tool_use/tool_result 对,失败不动历史。
- 恢复语义:流式重试 3 次(指数退避 + 纳秒抖动)、fallback 模型(AGENT_FALLBACK_MODEL,每 turn 切一次)、截断续跑(stop_reason=max_tokens/length 且无 tool_use 时注入续跑提示,限 3 次;这是 stop_reason 的唯一合法用途)、中断孤儿修补、EndReason 四态。
- token 记账:provider 回传 usage(Anthropic message_start/delta;OpenAI include_usage,usage 块在 finish_reason 后到)锚点 + 其后消息 chars/4 估算;压缩后锚点作废。
- 会话持久化(`core/src/rollout.rs`):`.kloop/sessions/{id}.jsonl` 逐条写透(History 可挂 Rollout;写失败降级纯内存);每行带信封 id(`{stem}#{seq}`,无 rand)/ parent(上一行 id,跨恢复续链)/ ts,重放线性但链是未来 rewind/fork 的 schema 地基,未知字段读取忽略(前向兼容,测试锁死);压缩追加 compacted 标记内嵌完整替换历史(仿 codex rollout),文件保持 append-only;恢复(`resume_session`)= 重放 + 双向配对修补(正向补 interrupted、反向删孤儿 tool_result)+ 坏尾**物理**截断 + offload 计数器 fetch_max 同步;`load_session` 只读不动文件;usage 锚点不落盘,首次采样重锚定。CLI `--resume [id]` / `--list-sessions`,session id 为 UTC 时间戳(手写 civil_from_days,无 chrono);子 agent 历史不持久化。

**测试**:71 个。protocol 线格式契约 / provider wiremock HTTP 契约(SSE 序列进、StreamEvent 断言出)/ tools 全执行路径 / history 锚点数学 + 写透 / compact 失败不动历史 / agent 恢复路径 / rollout 往返、标记重放、孤儿修补、跨重启 resume / cli 参数与时间戳。纪律:适配器行为变更必须先改契约测试。CI(`.github/workflows/ci.yml`,仓库根)在 push/PR 上强制 fmt --check / clippy -D warnings / test,macOS+Linux;**仓库尚无远端,workflow 只做过本地等价验证,首次推远端后要看它实际跑绿一次**。

**运行**:真 key 用 `ANTHROPIC_API_KEY`+`ANTHROPIC_BASE_URL`(不含 /v1,适配器自己拼 /v1/messages)或 `OPENAI_API_KEY`+`OPENAI_BASE_URL`+`AGENT_MODEL`;可选 `AGENT_CONTEXT_WINDOW`(默认 200000,off 关压缩)、`AGENT_FALLBACK_MODEL`、`AGENT_PROVIDER`。

## 三、沉淀的实现教训(新代码沿用)

1. 递归 async 的 Send 推断有共归纳盲区:递归边界必须是签名级类型擦除(`execute_tool` 返回 `Pin<Box<dyn Future + Send>>`)+ 子 agent `tokio::spawn` 成独立任务。
2. offload 编号用进程级全局 AtomicUsize——父子 agent 共用目录,per-实例计数会互相覆盖。
3. 子 agent(depth>0)的文本不流式输出到主 UI,产出走 tool result。
4. cc 的公式移植前先想小参数退化(predictive 负阈值教训)。
5. 压缩/重写类操作失败时必须保证原数据原封不动,测试锁死这一点。
6. 追加型文件的坏尾必须**物理**截断再续写:只做逻辑跳过的话,追加会和半行拼接成一行,之后的数据全部不可达(plan 7 潜伏 bug,7b 修复)。
7. 磁盘 schema 是基础模块里最难改的部分:演化字段(id/parent/ts)要在有存量文件之前落上;性能工程不改格式,可以后补。

## 四、进度

下一个:**plan 8(权限)**,其后 9-TUI、10-MCP、11-hooks(顺序可与用户重新商定)。
