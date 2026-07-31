# Plan 54 — 发现与扩展工具对齐

> 状态：✅ 已完成（2026-07-31）
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 已证明强制 defer profile 下 ToolSearch、`select:`、关键词搜索、错误输入和 MCP `tools/list_changed` 后 100→101 刷新。Skill、MCP resources、permission/concurrency 和精确 ranking/threshold 尚未闭环。

`call_tool` 是 kloop 为弱模型保留的 deferred-tool envelope，不计入 CC 缺口，也不能因 ToolSearch 相似而删除。

## 当前证据与差距

对应 matrix 行：

- `skill@clean-cli`
- `tool-search@mcp-deferred-cli`
- `dynamic-mcp@mcp-deferred-cli`
- `list-mcp-resources@mcp-scripted-deferred-cli`
- `read-mcp-resource@mcp-scripted-deferred-cli`
- `read-mcp-directory@mcp-scripted-deferred-cli`

完成结论：

- 固定 corpus 为 129 captures / 164 static evidence，matrix 仍为 56 行/448 单元：89 `compatible` / 143 `intentional-diff` / 34 `missing` / 150 `unknown` / 22 `n/a` / 10 `same`。
- Skill 与 ToolSearch 均闭合注册、解析、执行、权限和并发；schema/output/lifecycle 保留已证明的产品形态差异。
- Dynamic MCP 的注册/schema/parser/executor/output 已闭合；权限/并发没有权威 fixture，继续 `unknown`，生命周期因 CC 重组 provider tools、kloop 保持 round snapshot + `call_tool` 而为 `intentional-diff`。
- MCP resource list 八维兼容；read/directory 的权限因 kloop 对模型选择的 URI 保留人工批准而标 `intentional-diff`；图像/blob 输出和目录 capability lifecycle 分别保留已记录差异。
- ToolSearch 证据条件仍显式要求 `ENABLE_TOOL_SEARCH=true`；proxy 默认关闭不能外推。

优先复用：

- `kloop/crates/core/src/tools/tool_search.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/mcp/src/lib.rs`
- `kloop/crates/mcp/tests/client.rs`
- 现有 skill loader、deferred tool registry 和 `call_tool` 测试
- Plan 48 的 `local_mcp.py` 与 fake provider

## 目标

1. 固定 Skill、ToolSearch、dynamic MCP 和 MCP resources 的注册条件与完整行为链。
2. 固定 defer env/config gate、阈值、placeholder、tool_reference、请求重组和动态刷新。
3. 固定 ToolSearch 的 query parser、`select:`、关键词/ranking、错误、权限和并发。
4. 区分 MCP tools、resources 和 directories；不能用 tools/list 证明资源能力。
5. 保留 `call_tool` 的弱模型兼容价值，并明确它与 CC surface 的有意差异。
6. 所有 MCP fixture 只连接本地 stub。

## 开工证据闸门

- 延长 `cc-toolsearch-entry`、proxy gate、deferred placeholder 和 request assembly 静态链。
- 为 Skill 与三个 MCP resource surface 分别找 exact 构造点、gate、schema、parser、executor。
- 扩 `local_mcp.py` 支持 resources/list、resources/read、目录、错误、列表变化和调用日志。
- 保持 `ENABLE_TOOL_SEARCH=true` 的条件向量；另采 auto/threshold 只在可重复时纳入。
- 新 case 保存完整工具顺序和原始 schema；不得归一化 placeholder 或 lifecycle 顺序。
- 为 permission/concurrency 和 dynamic refresh 补 kloop 同输入 golden。

## 实施切片

### 0. ToolSearch 注册与 defer

- 固定开关 parser、proxy gate、阈值和首请求工具数组。
- 固定 deferred placeholder 与选中 schema 加入下一请求的时机。

### 1. ToolSearch 搜索与权限

- `select:`、关键词、空/坏输入、无结果、多结果和 ranking。
- 权限检查、调用时刷新和并发搜索。
- 不把简单字符串包含算法当目标语义，除非 fixture 证明。

### 2. Dynamic MCP

- list_changed 前后 schema、tool_reference、tools/call 和错误恢复。
- 重复/lag 通知、同名工具和并发刷新；disconnect/reconnect 只裁决现有边界。
- wire/命名差异可走 adapter，不要求内部结构同形。

### 3. Skill

- 固定 model-visible Skill 的 schema、发现范围、加载、错误和生命周期。
- 比较 kloop 现有 skill 加载与 slash 调用；名称相似不足以判同。

### 4. MCP resources

- 独立采 list/read/directory 的合法、缺失、坏 URI、目录和服务端错误。
- 取得 CC profile 前保持 `unknown`；不得因 kloop 未实现先写 `missing`。

### 5. `call_tool` 与产品回归

- 继续作为 kloop-only 兼容层。
- 增加 envelope 防御测试，确保 ToolSearch 适配不破坏弱模型调用。
- 同步 matrix、manifest、evidence、generator 与 verifier。

## 非目标与有意保留

- 不连接公网或真实 MCP server。
- 不在本计划实现 remote MCP/OAuth/Web；留 Plan 55。
- 不在本计划增加 stdio child 退出后的进程内自动重连；transport 关闭后调用报错，重新启动会话时重连。
- 不删除或隐藏 `call_tool`。
- 不从公开 Skills/MCP 文档推导 2.1.220 schema。
- 不把 resources 能力折算成 tools/list/tools/call。

## 完成记录（✅ 2026-07-31）

- 新增 19 个可重放 capture（每个均有 raw/normalized）：Skill 5、ToolSearch parser/ranking 4、select/refresh 后真实 MCP call 4、resources list/read/directory 6；另增 13 条 static evidence，verifier 对 Plan 54 的 fixture/schema/request rounds/MCP wire/Skill companion/资源结果逐项 fail closed。
- `skill` 继续复用现有渐进披露 registry：模型与 `/name` 使用同一 loader/参数展开；inline 返回 tool result、fork 复用子 agent，明确区别于 CC companion user block 生命周期。
- ToolSearch 补全大小写无关 exact/select、同次去重、`+required` term、prefix/name ranking、严格正整数 `max_results`、空 select 和坏输入；`tool_search` 本身只读且可并发，真实工具调用仍过原 hooks/权限/并发 gate。
- `call_tool` 继续作为严格 provider/弱模型的标准 deferred 执行桥；dispatch 在并发分类前拆 envelope，UI/history 保留原调用，inner tool 仍走正常 gate。
- `ToolSource` 定义改为不可变 `Arc<[ToolDef]>` snapshot，并增加 atomic definition snapshot/generation；ToolSearch 不会把刷新前 schema 绑定到刷新后 generation，同名 schema 更新后旧 unlock 必须重新发现。MCP refresh 与 calls 通过 async read/write gate 线性化：已开始的旧代调用先完成，catalog 随后发布；发布后的旧代调用拒绝。
- stdio MCP `notifications/tools/list_changed` 触发原子 catalog replacement；重复通知合并、broadcast lag 也视为 dirty，失败做有界退避重试，最终失败保留旧 catalog。Streamable HTTP 尚无 server-notification stream，因此启动状态明确报告 catalog 固定边界，不伪称动态刷新。
- MCP wire 增加 capability、resources/list、resources/read 与 directory extension；stdio/HTTP 在 JSON parse 前限制单消息 wire bytes，tools/resources paginator 另限制 page/cursor/item/bytes，read 与 tools/call 限 contents/bytes/image 并拒非法 base64，防止无限 cursor、接收期内存和上下文耗尽。
- 资源 list 支持按 server 或并发聚合，单 server/全失败返回错误、部分失败保留成功项并点名失败 server；read/directory 只接受当前 resources/list 中存在的 URI，server error 不再伪装为空/成功文本，图像进入模型 block、其他 blob 只给降级说明。

## 产品边界与有意差异

- CC ToolSearch 返回 `tool_reference` 并重组后续 provider tool array；kloop 返回完整 schema，unlock 不改当前 request prefix，下一 round 才采动态 source snapshot，并保留 `call_tool`。
- CC Skill 形态为 `Skill {skill,args}` + companion user block；kloop 保持 `skill {name,arguments}` + inline/fork tool result，用户 commands 仍 slash-only。
- 本地 stdio MCP 支持通知驱动动态 catalog；HTTP 只支持请求/短 SSE response，不实现长期 server notification subscription。
- `list_mcp_resources` 自动允许；`read_mcp_resource` / `read_mcp_resource_dir` 即使协议语义只读，也会把 server+URI 展示给人类并走普通批准，且动态 URI 不进入 session/persistent remember cache；只有显式配置 allow 或 bypass 才能放宽。Bypass/deny/ask 仍遵循既有权限层序。
- 资源 read 总结果有界；支持图像直接作为模型 image block，不复制 CC 的 100 MiB 本地 artifact 路径。资源 templates/prompts、server-initiated sampling/elicitation/roots 仍不在本计划范围。

## Fixture 与测试

至少覆盖：

- defer 关闭/强制/阈值、首请求顺序和 placeholder；
- `select:`、关键词、无结果、多结果、错误输入；
- 100→101 list_changed、重复/lag 通知、refresh 失败重试和在途 generation guard；
- Skill 成功/失败/缺失，只在真实 surface 被证明后采集；
- resources list/read/directory、坏 URI、目录和 server error；
- `call_tool` 的合法 envelope、坏 name/input 和 deferred bridge。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core tools::tool_search::tests
cargo test -p kloop-mcp
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

### 最终验收结果（2026-07-31）

以下全部通过：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

其中 workspace tests 最终包含 kloop CLI 85、core 457、MCP 26+14 等全部 crate/integration tests；Plan 54 定向测试另锁定 15 个 ToolSearch 分支、resource Manual/deny/ask/bypass + non-remember 权限、atomic definition snapshot/generation stale-call、call↔refresh 线性化、refresh retry、HTTP capability/wire boundary、resource partial/error/URI guard 与 MCP paginator/read/tools-call budgets。全部证据、实现、测试与文档合为一次 `plan54` commit（本次，见 git log）。

## 文档同步

本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物均已同步；`call_tool` 保留原因、resource 批准/大小边界和 HTTP notification 限制均有记录。

## 完成标准

- 当前可运行的发现/扩展链有 exact fixture 和 kloop golden。
- ToolSearch permission/concurrency 与 dynamic parser 已裁决。
- Dynamic MCP permission/concurrency 与不可运行的 transport/profile 分支继续有明确 unknown/边界理由，不通过删 row 收敛。
- 所有门禁全绿，一次提交，提交信息带 `plan54`。

## 开工决定

- Skill 使用现有 model-visible `skill {name,arguments}` 与 slash loader；不改成 CC 的 `Skill {skill,args}`。
- ToolSearch 对齐可观测搜索/解析/权限/并发，不复制内部 ranking 实现；保留 kloop-only `call_tool`。
- MCP resources 进入本地 MCP 产品范围；remote HTTP/OAuth 的资源调用继续可用，但 HTTP server notification 订阅不在本计划实现。
- Resource read/directory 不因“只读”自动越过权限门：list 自动允许，模型选择 URI 的 read 走普通外部工具批准。
