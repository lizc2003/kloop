# Plan 54 — 发现与扩展工具对齐

> 状态：未开工
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
- `list-mcp-resources@mcp-deferred-cli`
- `read-mcp-resource@mcp-deferred-cli`
- `read-mcp-directory@mcp-deferred-cli`

当前结论：

- Skill registration/schema 为 `missing`；kloop 没有同形的 model-visible Skill 工具。
- ToolSearch 的 registration/schema/parser/executor/output/lifecycle 为 `compatible`；permission/concurrency 未知。
- Dynamic MCP 的 registration/schema/executor/output/lifecycle 为 `compatible`；parser/permission/concurrency 未知。
- 三个 MCP resource surface 全维 `unknown`；Plan 48 local MCP 只证明 tools/list、tools/call 和 list_changed。
- 本地/proxy base URL 下 optimistic ToolSearch 会被禁用；现有 defer profile显式记录 `ENABLE_TOOL_SEARCH=true`。

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
- server disconnect/reconnect、重复通知、同名工具和并发刷新。
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
- 不删除或隐藏 `call_tool`。
- 不从公开 Skills/MCP 文档推导 2.1.220 schema。
- 不把 resources 能力折算成 tools/list/tools/call。

## Fixture 与测试

至少覆盖：

- defer 关闭/强制/阈值、首请求顺序和 placeholder；
- `select:`、关键词、无结果、多结果、错误输入；
- 100→101 list_changed、重复通知、disconnect 和并发刷新；
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

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。记录 `call_tool` 保留原因和 MCP resource 实际支持边界。

## 完成标准

- 当前可运行的发现/扩展链有 exact fixture 和 kloop golden。
- ToolSearch permission/concurrency 与 dynamic parser 已裁决。
- Skill/resources 未运行分支继续有明确 unknown 理由，不通过删 row 收敛。
- 所有门禁全绿，一次提交，提交信息带 `plan54`。

## 开工时定 / 问用户

- Skill 采用 model-visible tool、现有 slash surface 或明确有意差异。
- ToolSearch ranking/threshold 与 permission/concurrency 的兼容边界。
- MCP resources 取得 profile 证据后是否进入 kloop 产品范围。
