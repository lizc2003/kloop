# Plan 34b — MCP 远程 OAuth(授权码 + PKCE + discovery)

> 承接 plan 34。远程传输(streamable HTTP)已通,认证首片只做了
> `bearer_token_env_var`(静态 bearer)。生态里的主流托管 MCP server(GitHub、
> Linear、Notion 这类代表用户身份的)走 **OAuth 2.1 授权码流**,bearer 接不进来
> ——"下载即用"缺的另一半。补上交互式 OAuth 登录 + token 持久化 + 传输挂钩。

## 回源结论(2026-07-16,两家真读,file:line)

约定:`cc/` = `~/work/claude-code/src/`;cc 的 SDK 指
`@modelcontextprotocol/sdk` 的 `client/auth.js`(cc 把协议编排全委托给它)。
`sky/` = `refs/codex/codex-rs/rmcp-client/src/`;codex
把协议原语委托给 `rmcp` crate(v1.8.0,feat `auth`)+ `oauth2` crate v5,
`RMCP` = 该 crate 的 `transport/auth.rs`。

**两家形态**:都是"薄自研胶水 + 厚现成原语"。
- **cc**:自实现 `OAuthClientProvider` 回调 + keychain 存储 + 本地回调 server +
  发现缓存 + 刷新锁;PKCE/DCR/授权 URL/code 换 token 全由 SDK `auth()` 做
  (`cc/services/mcp/auth.ts:847 performMCPOAuthFlow`、`:1376 ClaudeAuthProvider`)。
- **codex**:自实现登录编排(`sky/perform_oauth_login.rs`)+ token 持久化
  (`sky/oauth.rs`)+ HTTP 适配 + 传输挂钩(`sky/rmcp_client.rs`);PKCE/discovery/
  DCR/交换/refresh 全在 `rmcp`(`RMCP` 全文),`rmcp` 又建在 `oauth2` v5 上。

**两家收敛的骨架(必然解,直接照抄机制)**:

1. **授权码 + PKCE(S256)+ state**。verifier = 随机 32B、challenge =
   base64url(sha256(verifier))、`code_challenge_method=S256`;state = 随机 CSRF,
   回调**必须校验一致**才收 code。两家:cc SDK `auth.js:705-728` + 回调校验
   `cc/…/auth.ts:1109-1118`;sky `RMCP:1200-1241`(PKCE+CSRF)+ 客户端要求
   `code`&`state` 同在 `sky/perform_oauth_login.rs:354-355`。verifier/state 只需
   单次 flow 内存持有(cc 明确不落盘 `auth.ts:1946-1949`)。

2. **两步 discovery + 从 401 拿 metadata URL**(RFC 9728 → RFC 8414):
   - GET server URL,读 **401 的 `WWW-Authenticate` 头**,正则抠
     `resource_metadata="…"`(cc SDK `auth.js:399-408`;sky `RMCP:1817-1848,1906-1923`)。
   - GET 该 protected-resource metadata → 取 `authorization_servers[0]`
     (cc `auth.js:662-685`;sky `RMCP:1727-1758`)。
   - 对 AS GET `.well-known/oauth-authorization-server`(失败退
     `openid-configuration`)拿 `authorization_endpoint`/`token_endpoint`/
     `registration_endpoint`。**必须带 path-aware 变体**(`/.well-known/oauth-
     authorization-server/<path>`,path-scoped server 很常见):cc `auth.js:555-586`
     + `cc/…/auth.ts:302-310`;sky `RMCP:1671-1682,834-848`。

3. **本地 loopback 回调 server**:`127.0.0.1:<随机高位端口>`、path `/callback`、
   `redirect_uri = http://localhost:<port>/callback`(RFC 8252 §7.3:loopback 只需
   path 匹配、端口任意);oneshot/channel 等 code、**~5 分钟超时**、收到回 200 HTML。
   cc `auth.ts:1099-1213` + `oauthPort.ts`(端口随机、可 `MCP_OAUTH_CALLBACK_PORT`
   固定);sky 用 `tiny_http` 随机端口 + `spawn_blocking` + `timeout(300s)`
   `sky/perform_oauth_login.rs:260-303,507-596`。

4. **client_id:预配 vs DCR(RFC 7591)**。有预配 client_id 就直接用;否则 POST
   `registration_endpoint`(body `grant_types:[authorization_code,refresh_token]`、
   `token_endpoint_auth_method:"none"` = public client、`response_types:["code"]`),
   取回 `client_id` 存下复用。cc `auth.js:892-916` + `cc/…/auth.ts:1417-1538`;
   sky `RMCP:1077-1170,2476-2497`。**CIMD(SEP-991,URL-as-client_id)是 Anthropic 托管
   专属**(cc `clientMetadataUrl` 固定 claude.ai,`auth.ts:1445-1452`)→ kloop 不做。

5. **token 存储:绝对 `expires_at`,key=`name|hash(url)`**。存
   `{url, client_id, access_token, refresh_token, expires_at(绝对时间), scope}`;
   **持久化算绝对过期时间而非存相对 `expires_in`**(否则重启无法判临期——两家都
   踩过并这么修:sky `oauth.rs:733-745,155-175`;cc `auth.ts:1704-1731`)。后端:
   两家都 keyring 优先、file 回退(sky `oauth.rs:103-283`;cc `utils/secureStorage/`)。

6. **刷新:请求前临期主动刷 + 401 兜底 + 失败清 token**。剩余寿命 < skew
   (sky 30s `oauth.rs:71`;cc 300s `auth.ts:1650`)就 `grant_type=refresh_token`
   先刷再发;`invalid_grant`/refresh 失败 → 清 token、标记需重登(sky `RMCP:1570-
   1616`+`persist`;cc `auth.ts:2177-2359`)。跨进程刷新锁两家都有(file lock),对
   kloop 是 nice-to-have,单进程先 `Mutex`/in-flight 去重。

7. **不在 401 自动弹浏览器**(两家一致,关键 UX):连接/请求 401 → 只标记该 server
   `needs-auth`(cc 15min 缓存避免反复探测 `client.ts:2313-2329`),**交互式登录由
   用户命令显式触发**,不后台抢 TTY。cc `performMCPOAuthFlow` 全仓无自动调用点;
   sky 交互登录是独立入口。

8. **RFC 8707 `resource` 参数**:授权 URL 与 token/refresh 请求都带
   `resource=<mcp server url>`(audience 绑定),面向强校验 audience 的 server 必需。
   cc `auth.js:725-726`;sky `RMCP:1209` + `perform_oauth_login.rs:546-550`。

## kloop 现状与落点

- plan 34 已有:`Transport` trait、`HttpTransport`(`crates/mcp/src/http.rs`,reqwest
  在 mcp 内)、`base_headers` 注入 `Authorization`、session/version/重试/404 重握手。
  配置 `McpTransport::Http{url, bearer_token_env_var, http_headers}`。
- 落点:
  - **协议原语**(PKCE/discovery/DCR/token 交换/refresh):新模块
    `crates/mcp/src/oauth.rs`。复用 `HttpTransport` 已有的 reqwest client 发
    discovery/token 请求。
  - **回调 server**:极简手写 `tokio::net::TcpListener`(只需读一行 GET、回一个
    200 HTML)——**不引 tiny_http/axum**,沿用 kloop"能手写的小东西就手写"风格
    (SSE parser、HTML→text 都是手写)。
  - **token 注入**:登录成功后把 `Authorization: Bearer <access_token>` 放进
    `HttpTransport` 的动态头(现在 `base_headers` 是构造期固定的,要加一个
    `Mutex<Option<String>>` 的 oauth token 槽 + 请求前临期刷新钩子——和现有
    session_id 槽同形)。
  - **存储**:MVP 单文件 `.kloop/mcp-oauth.json`(0600),cli 侧读写;结构照抄第 5 点。
  - **登录入口**:cli 子命令/flag(如 `--mcp-login <server>`)触发交互登录;连接时
    server 需 OAuth 且无有效 token → 沿用 plan 34"降级警告不阻塞启动" + 提示跑登录。
- 命名消毒/超时/readonly/权限默认询问全部零改动继承。

## 关键决定(开工时定 / 问用户)

1. **借 `oauth2` crate 还是手写协议原语**。倾向**手写核心**:PKCE = sha2 +
   base64(kloop 已有 base64,只需引 sha2)、token/refresh = 一个 POST form + JSON
   解析、discovery = 两个 GET,都是小东西;`oauth2` v5 是重依赖且其类型体系会渗透。
   开工看 sha2 依赖成本 + 手写 token 错误分类的把握度定。(codex 借 oauth2 是因为它
   走 rmcp 全家桶;kloop 自写 wire 协议,风格是借小 crate 不借框架。)
2. **首片 client_id 范围**:先只支持**预配 client_id**(config 里给)跑通授权码流,
   还是首片就做 **DCR**?倾向首片做 DCR——它只是一个 POST `registration_endpoint`,
   而"零配置连任意 server"正是 OAuth 的价值;预配 client_id 作为并存旁路。开工定。
3. **存储后端**:MVP 单文件(倾向,和两家 file 回退一致)vs 直接上 keyring。keyring
   跨平台(macOS keychain / Linux secret-service / Windows credential)是独立体量,
   倾向后置。
4. **登录 UX 入口形态**:CLI 子命令 `kloop mcp login <name>` vs 首连失败交互提示 vs
   TUI 里的命令。倾向先做 headless 友好的 CLI 子命令(打开浏览器 + 等回调 + 存
   token),TUI/server 集成后续。**问用户**:有没有偏好的触发方式。
5. **真 key 验收用哪个 server**:需要一个真实要 OAuth 的远程 MCP server(GitHub MCP、
   或自建带 OAuth 的)。**问用户**要地址;没有就自建一个最小 OAuth MCP server 做
   端到端(仿 plan 34 的本地 Python server 思路,加 discovery + authorize + token 端点)。

## 切片建议(MVP 六件套,照 codex agent 的排序)

**必做**:① 授权码 + PKCE(S256)+ state 校验;② 两步 path-aware discovery(含
WWW-Authenticate 解析);③ loopback 回调 server(随机端口 + 5min 超时 + state 校验);
④ client_id(预配 + DCR 二选一或并存);⑤ token 交换 + 单文件存储(**绝对 expires_at**);
⑥ 传输挂钩(注入 Bearer + 请求前临期刷新 + 401 兜底刷一次 + 失败标记需重登)。

**后置(挂账,记为可能性)**:keyring/加密后端 + 跨进程刷新文件锁、CIMD(SEP-991)、
XAA(SEP-990,cross-app access)、step-up(403 `insufficient_scope` 重授权提权)、
手动粘贴回调降级(无浏览器环境)、`headersHelper` 动态头、多 server 共用回调的
callback_id 消歧、固定端口/自定义 redirect_uri 配置。

## 不做(挂账)

旧版 SSE 传输、ws/sdk/claudeai-proxy 传输(plan 34 已挂账,延续);OAuth 里的
CIMD/XAA/step-up/keyring/跨进程锁(见上);client credentials / device code 等其它
grant(MCP 用授权码流)。

## 测试

wiremock 契约(plan 34 已有范式):① 401 带 `WWW-Authenticate: …resource_metadata="…"`
→ 客户端解析出 PRM URL;② protected-resource metadata → `authorization_servers`;
③ AS metadata 两步探测含 path-aware 变体;④ DCR POST 到 registration_endpoint、
回 client_id;⑤ 授权 URL 形态(response_type/code_challenge/S256/state/resource);
⑥ 回调 server 收 `code`+`state`、state 不符即拒、超时;⑦ token 交换(带 code_verifier
+ resource)→ 存绝对 expires_at;⑧ 请求前临期主动刷新、401 兜底刷一次并重放;
⑨ refresh 失败(invalid_grant)清 token 并标记需重登;⑩ 配置解析(url + oauth 相关键)。
keyring/浏览器打开不进单测(交互/系统副作用)。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 补 OAuth server 配置 + 登录命令示例;本文件
补完成记录;HANDOFF 补能力条目。真 key 验收:接一个真实需 OAuth 的远程 MCP server
(问用户要地址;或自建最小 OAuth MCP server)双轨跑通登录 + tools/call。
