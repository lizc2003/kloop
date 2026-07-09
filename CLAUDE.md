# kloop — 项目常驻记忆(每个会话自动加载)

> 读完本文件即可开工,无需重新调研。文档只有三类:本文件(活的入口:定位、决定、教训、流程)、`docs/plan/`(编号的会话任务:已完成的是历史,未完成的是施工图)、`refs/README.md`(参考库导读 + 调研结论 + 可移植设计参考)。
> 用户偏好:中文交流,短句增量澄清,别用多选项问卷。有疑问一次问一个点。

## 一、项目定位(2026-07-09 用户拍板,勿再偏航)

**kloop 就是目标项目**——从零做一个新的 Rust agent。`refs/codex`(codex 生产级 fork)、`~/work/claude-code`(cc 逆向 TS 版)、`refs/claw-code` 三个库**仅供参考,绝不在其上开发或推送**(上个会话犯过一次,已纠正)。开发阶段直接在本仓库 main 分支开发,不开 feature 分支。

已拍板的 P0 决定:① 编辑工具用 Edit(old_string/new_string)形态,不迁就 apply_patch(实测 gpt-5.4-mini 都能首试用对);② 目标模型双轨,Claude(sonnet-5)为主、OpenAI-compat 为副;③ 对 codex 上游只保持"可跟随性"。

## 二、当前状态(Phase 1 完成)

**结构**:Cargo workspace,四 crate 严格单向依赖链(见 `kloop/README.md` 的 Layout 节):
`kloop-protocol`(零依赖线格式)← `kloop-provider`(适配缝,独占 reqwest)← `kloop-core`(agent 本体,无网络)← `kloop`(cli 二进制)。

**能力**(全部真实 API 验证过,除注明):
- 5 个原始赌注:append-only 历史 + 录入时 offload(>8000 字符落盘 + 指针 + read_offloaded 回读)、tool_use 有无判续跑、按入参动态并发(连续安全调用并发批)、task 子 agent 递归复用 run_turn(深度限 1)、provider 适配缝(Anthropic SSE / OpenAI-compat / Mock)。
- 压缩双防线:predictive(采样前预判 当前+增长预留 是否爆窗,增长=min(输出上限,20k)+15k,**窗口≤预留时跳过**——这个守卫是 codex 预演抓出的 cc 盲点)+ reactive(溢出错误 downcast OverflowError,每 turn 压缩一次重试)。压缩=模型写交接摘要+保留约 2k token 近期原文,边界绝不切开 tool_use/tool_result 对,失败不动历史。
- 恢复语义:流式重试 3 次(指数退避+纳秒抖动)、fallback 模型(AGENT_FALLBACK_MODEL,每 turn 切一次)、截断续跑(stop_reason=max_tokens/length 且无 tool_use 时注入续跑提示,限 3 次;这是 stop_reason 的唯一合法用途)、中断孤儿修补、EndReason 四态。
- token 记账:provider 回传 usage(Anthropic message_start/delta;OpenAI include_usage,usage 块在 finish_reason 后到)锚点 + 其后消息 chars/4 估算;压缩后锚点作废。

**测试**:54 个,`cargo test` 1 秒跑完,无网络无 key。protocol 线格式契约 / provider wiremock HTTP 契约(SSE 序列进、StreamEvent 断言出)/ tools 全执行路径 / history 锚点数学 / compact 失败不动历史 / agent 恢复路径全覆盖。测试纪律:整对象断言优先;适配器行为变更必须先改契约测试。

**运行**:`cargo run -p kloop -- --mock`(无 key 演示);真 key 用 `ANTHROPIC_API_KEY`+`ANTHROPIC_BASE_URL`(不含 /v1,适配器自己拼 /v1/messages)或 `OPENAI_API_KEY`+`OPENAI_BASE_URL`+`AGENT_MODEL`;`AGENT_CONTEXT_WINDOW`(默认 200000,off 关压缩)、`AGENT_FALLBACK_MODEL`、`AGENT_PROVIDER` 可选。**测试代理和 key 问用户要**(上会话用过一个 OpenAI-compat 代理和 taiji 的 Anthropic 代理,勿写进任何提交文件)。

## 三、沉淀的实现教训(新代码沿用)

1. 递归 async 的 Send 推断有共归纳盲区:递归边界必须是签名级类型擦除(`execute_tool` 返回 `Pin<Box<dyn Future + Send>>`)+ 子 agent `tokio::spawn` 成独立任务。
2. offload 编号用进程级全局 AtomicUsize——父子 agent 共用目录,per-实例计数会互相覆盖。
3. 子 agent(depth>0)的文本不流式输出到主 UI,产出走 tool result。
4. cc 的公式移植前先想小参数退化(predictive 负阈值教训)。
5. 压缩/重写类操作失败时必须保证原数据原封不动,测试锁死这一点。

## 四、Phase 2 待做(按 plan 逐会话推进)

**工作方式:`docs/plan/` 一个编号文件 = 一个会话的任务。** 1–5 是已完成的历史记录(✅ 标注),6 起是待做;每个 plan 文件自带目标、设计要点、参考指针、测试与完成标准。新会话开场:读本文件 + 对应 plan 文件,开工前把 plan 里标注"开工时定/问用户"的点先问清。

当前进度:下一个是 **`docs/plan/6-ci.md`**,其后 7-会话持久化、8-权限、9-TUI、10-MCP、11-hooks(顺序可与用户重新商定)。

每完成一个 plan:cargo fmt + clippy + test 全绿 + commit(信息写清验证方式);行为变更同步改 README;把该 plan 文件补上"✅ 已完成 + 结果摘要 + 提交号",作为历史留档;新发现的教训写进本文件第三节。

## 五、代码风格约定(沿用)

format! 内联变量;match 尽量穷尽;避免 bool 位置参数(必要时 /*param*/ 注释);测试整对象断言;新增行为必须带测试;注释只写代码看不出来的约束,不写"这行干了什么"。
