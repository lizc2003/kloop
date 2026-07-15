# Plan 34 — MCP 远程传输(streamable HTTP)

> 一句话定位:kloop 的 MCP 客户端只有 stdio 子进程一种传输(plan 10),生态里越来越多
> MCP server 是远程托管的(url 声明、bearer/OAuth 认证)——整类接不进来,"下载即用"
> 承诺缺了一半。cc 与 codex 双家收敛于 streamable HTTP,补上。

## 回源结论(2026-07-15,两家真读,file:line)

- **claude-code**:config 以 `type` 字段区分传输(discriminated union,
  `services/mcp/types.ts:23-24/124-135`):stdio(type 可省)/ `sse`+url / `http`+url /
  ws / sdk / claudeai-proxy。`http` → `StreamableHTTPClientTransport`(现行协议,
  `client.ts:785-866`),`sse` → `SSEClientTransport`(**旧版协议**,:620-678);两者
  显式选择、无运行时回退。认证:静态 `headers`、动态 `headersHelper`、完整 OAuth
  (`auth.ts:847` 授权码 + 本地回调 + keychain 存储、401 处理)。
- **codex**:统一后端 `rmcp-client` crate(stdio 也走它)。config 是 **untagged 二
  态**(`config/src/mcp_types.rs:435`):`Stdio{command,args,env,…}` vs
  `StreamableHttp{url, bearer_token_env_var, http_headers, env_http_headers}`——
  **明文 `bearer_token` 直接拒绝**,报错引导用环境变量名
  (`core/src/config/mod.rs:2119-2131`)。OAuth:授权码 + 本地回调 + keyring/文件存储
  (`perform_oauth_login.rs`)。重试:仅 HTTP 传输,退避 250ms/1s 再终局一次,
  408/429/5xx 与瞬时网络错可重试,401/403/session-expired 不重试
  (`streamable_http_retry.rs:23-48/137-195`);**404 session-expired → 重新
  initialize 恢复一次**(semaphore 串行,`rmcp_client.rs:326/1094-1149`)。**不做旧版
  SSE 传输**。
- **收敛点(照抄)**:① config 里 `command` vs `url` 二选一区分传输(kloop 的
  `[mcp.servers.<name>]` 天然适配 untagged 形);② 现行协议是 **streamable HTTP**
  (cc 的 `sse` 是 legacy 档,codex 根本不做旧 SSE → kloop 不做);③ **秘密不落配
  置明文**——bearer 走环境变量名引用(codex 形,kloop 有 `.kloop/env.local` 先例);
  ④ OAuth 两家都有、都是大件(授权码 + 回调服务器 + 系统凭据存储)→ 切片挂账。
- **权威提醒(教训 9)**:传输细节(POST/GET、`Mcp-Session-Id` 头、Accept、SSE 响应
  流分帧)以 **MCP 官方 spec(2025-03-26 streamable HTTP)+ 官方 SDK** 为准,两家代
  码只作旁证——plan 10 的教训(claw 分帧是错的)在传输层同样成立。

## kloop 现状与落点

- `crates/mcp` 是自写 stdio 换行 JSON-RPC,只依赖 protocol;reqwest 现在被
  `kloop-provider`/`kloop-web` 独占。
- 落点:`McpClient` 后面加传输抽象(或平级新类型),握手/翻页/tools/call/图内容块那套
  协议层复用;配置 `[mcp.servers.<name>]` 加 `url`(与 `command` 互斥,都给报错)+
  `bearer_token_env_var` + `http_headers`。命名消毒/超时(30s/30s/60s)/readonly 标
  注/权限默认询问全部零改动继承。
- 重试/恢复最小面:初始化瞬时错误重试(codex 退避表)+ session-expired 重新握手一
  次;不做后台重连管理层。

## 关键决定(开工时定 / 问用户)

1. **HTTP 客户端放哪**:`kloop-mcp` 直接引 reqwest(破"只依赖 protocol",但最直)vs
   传输经 trait 从 cli 注入(保持 crate 纯度,多一层)。开工看依赖图定。
2. **认证首片范围**:只做 `bearer_token_env_var` + 静态 `http_headers`(倾向;明文
   token 照 codex 拒),OAuth 整体挂账。
3. **spec 版本协商**:kloop stdio 现发 protocolVersion 2025-06-18;HTTP 侧版本头与降
   级策略对着 spec 定。

## 不做(挂账)

OAuth(授权码 + 回调 + keyring,独立 plan 级体量;做时对 RFC/spec 不对两家代码)、旧
版 SSE 传输(cc legacy、codex 不做)、ws/sdk/claudeai-proxy 传输、`headersHelper`
动态头、XAA/jwt-bearer、后台自动重连(codex-mcp 的托管层)、`required` server 启动失
败即退出(kloop 现行"降级警告"先例保持)。

## 测试

wiremock HTTP 契约(provider 已有范式):initialize 握手 + `notifications/initialized`
+ tools/list 翻页 over HTTP、session id 头往返、tools/call 与图内容块、
`bearer_token_env_var` 注入 Authorization、配置明文 token 拒绝、408/429/5xx 重试语义、
session-expired 重握手一次、401 不重试直接报错;`[mcp.servers]` 解析:url/command 互
斥、畸形拒绝。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 补远程 server 配置示例;本文件补完成记录;
HANDOFF 补能力条目。真 key 验收:接一个真实远程 MCP server(问用户要地址/token)双轨
跑通 tools/call。
