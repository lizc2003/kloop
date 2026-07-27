# Plan 55 — Web 与 remote 条件对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 54
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 对 WebFetch 的本地 URL case 只取得权限顺序负证据：2.1.220 在 domain-safety 层先返回 `Unable to verify if domain 127.0.0.1 is safe to fetch.`，local Web stub 请求数严格为 0。

因此 redirect、auth、HTTP error、large body 和真实 executor 仍为 `unknown`。WebSearch 也只有 registration/schema 的保守 `compatible`。本计划不得用公网或实时服务填补未知项。

## 当前证据与差距

对应 matrix 行：

- `web-fetch@allow-cli`
- `web-search@clean-cli`

当前结论：

- WebFetch registration/schema/parser 为 `compatible`，permission 为 `intentional-diff`，output 只有前置拒绝的 `compatible`，executor/concurrency 未知。
- 现有 `webfetch-ok/redirect/cross-host-redirect/http-error/large/auth-error` 均未到达 local stub。
- WebSearch registration/schema 为 `compatible`；parser 到 lifecycle 均未知。
- CC profile 均为 `remote=false`；remote gate 和平台条件未被当前 fixtures 裁决。

优先复用：

- `kloop/crates/web/src/fetch.rs`
- kloop WebSearch backend/CLI/fetch 接线
- `kloop/crates/core/src/tools/mod.rs`
- Plan 48 的 `local_web.py`、fake provider 和零请求断言
- Plan 54 已稳定的本地扩展/MCP 边界

## 目标

1. 固定 WebFetch schema/parser、URL/domain permission、redirect 和 executor 顺序。
2. 在不绕过安全层、不访问公网的前提下，判断是否存在可 hermetic 到达本地 stub 的合法 profile。
3. 固定 WebSearch 的配置 gate、parser、executor、权限、并发、输出和失败。
4. 固定 remote 条件只影响哪些工具/adapter；未运行平台保持 `unknown`。
5. kloop SSRF、redirect、认证和内容大小防线更严格时，明确记录 `intentional-diff`。

## 开工证据闸门

- 从 `cc-webfetch-entry` 追 domain safety、permission、redirect、fetch、decode、truncate 和 result mapping。
- 为 WebSearch 与 remote gate 建 exact 静态链。
- local Web stub 保持 loopback；禁止临时放开代理、DNS 或公网访问来取得 fixture。
- 先验证 2.1.220 是否有合法可控 origin/profile。若没有，executor 继续 `unknown`，并保存负证据。
- WebSearch 使用 fake backend，不用实时搜索结果或真实 key 作 golden。
- 每个 fixture 断言 local request count、host/path、redirect hops 和禁止的 egress。

## 实施切片

### 0. WebFetch parser 与 safety ordering

- 固定 URL 类型、协议升级、坏 URL、host/domain permission 和前置拒绝。
- 保留现有零请求 cases，不能将其改名为成功执行。

### 1. WebFetch executor

- 仅在合规 hermetic profile 存在时采同域/跨域 redirect、auth、HTTP status、large body、content type 和 decode。
- 无合规入口时记录 inability，不绕过产品安全检查。

### 2. WebSearch

- 固定 query/schema、backend gate、结果形状、无结果、provider error、timeout 和截断。
- 使用可控本地 fake backend，固定并发与 permission。

### 3. Remote 条件

- 确定 `remote=true` 的真实入口、工具数组和生命周期。
- 当前平台不可运行的条件保留 unknown；不推断 PowerShell、SendUserFile 或云服务行为。

### 4. 产品与回归

只实现已裁决差距；SSRF/redirect 防线冲突优先保留安全策略并记录有意差异。

## 非目标与有意保留

- 不访问公网、不用真实搜索 key、不提交缓存或实时结果。
- 不绕过 CC domain-safety 生成假成功 fixture。
- 不因 loopback 被拒推断 CC 不支持 redirect/auth/large body。
- 不削弱 kloop SSRF、DNS rebinding、认证或大小限制。
- 不研究其他 CC 版本或未运行平台。

## Fixture 与测试

至少覆盖：

- WebFetch 缺 URL、错类型、坏协议、domain 拒绝和 zero-request；
- 若可 hermetic 到达：ok、同/跨 host redirect、redirect loop、auth、4xx/5xx、large body、decode；
- WebSearch 最小 query、错输入、无结果、backend error、timeout、大结果和并发；
- 每个 Web case 的请求计数和禁止 egress；
- remote profile 只在本机真实可运行时加入。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-web
cargo test -p kloop-core web
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。必须继续区分 permission-ordering 负证据和 executor 成功证据。

## 完成标准

- WebFetch 可到达层级被精确证明；未到达 executor 时继续保守 unknown。
- WebSearch 可执行链有本地 fake backend fixture 和 kloop golden。
- 无 stub 外网络访问，敏感信息扫描全绿。
- 所有门禁全绿，一次提交，提交信息带 `plan55`。

## 开工时定 / 问用户

- 是否存在合规、可重放的本地 executor profile。
- kloop 更严格 Web 安全策略哪些明确保留为有意差异。
- remote 的真实产品入口和当前平台必须覆盖的范围。
