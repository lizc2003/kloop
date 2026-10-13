# Plan 231 — fetch 与 skill 的六项收紧 ✅

> 2026-10-13。plan 230 回退原生审查后，用户要求继续优化 `web_fetch` 与 skill 子系统
> （列出的第 7 项不做）。全部改动都不碰共享调度。
> 实现提交 SHA 以本文件所在提交为准。

## 范围与取舍

1. **同站跳转不再静默**：`fetch_url` 记下归一化后的请求地址；成功返回时若跟随过同站
   跳转，在正文尾附 `[followed same-site redirect to <最终地址>]`。跨站跳转已有自己的
   提示，不动。理由是这种跳转是静默的：登录页或中间页会以"就是被请求的文档"的样子
   送达，属于假成功——比它掩盖掉的 404 更危险。
2. **429 带出 `Retry-After`**：文案本来就要求模型"respect Retry-After"，而整个 web
   crate 从不读该头。合法值只有秒数或 HTTP 日期，超过 64 字符或为空的一律标为
   ignored 而不截断（截断会造出一个不同的值）。`HeaderValue::to_str()` 已把值限制在
   可见 ASCII，所以这里只判宽度。
3. **正文类型放宽**：除 `text/*` 与空类型外，接受常见 `application/*` 文本
   （json / xml / javascript / ecmascript / yaml / toml / x-sh / shellscript / sql /
   csv / markdown / `+text`）；二进制仍拒绝，不用有损解码冒充文本。
4. **skill 正文去掉与 fetch 文案重复的网络规则**：删掉"匿名 404 不等于仓库不存在"的
   归因句与"不要猜 URL/IP、不要查凭据文件"（401/403/404 分支已在失败点打印），改为
   指向 `web_fetch` 的错误文本。**保留** DNS 失败≠沙箱拒绝、敏感路径拒绝是硬停、
   退出码必须独立保留、用 Bash 的 `timeout_ms`——这四条 fetch 层没有对应文案：
   DNS/传输失败不走 HTTP 状态分支，本地路径拒绝也不是 HTTP 403，删掉就是丢规则。
5. **契约表同步**：删掉随第 4 项消失的那条断言；其余 11 条仍在保留文本里。
6. **`allowed-tools` 里 kloop 没有的工具名给出提示**：在 `fork_skill` 套用 allowlist
   前校验，命中 `tools::builtin::ALL` 或任一 tool source 的 `defs()` 即为已知，未知的
   用 `Event::Note` 点出名字。这类名字不是授权而是收窄，全是这样的名字时子代理几乎
   无工具可用、skill 静默失效。选 fork 时校验而不是加载时：加载期拿不到工具来源，
   也不必为它给 `Skill` 加字段并改 CLI 警告通道；代价是提示只在 skill 真被激活时出现。
7. 同一 skill 重复激活的正文去重**不做**（省的是 token，风险是模型误以为丢了指令）。

## 复核后修订（2026-10-13）

六项提交后复核提出两条，都成立，已修：

- **正文类型按子串匹配会放进二进制**：`contains("x-sh")` 同时命中
  `application/x-sharedlib` 与 `application/x-shockwave-flash`，两者会被
  `from_utf8_lossy` 之后当成功正文送出（此前是"不支持的内容类型"）。改为剥掉 MIME
  参数后按完整类型匹配、结构化后缀单独判断，并补这两条拒绝用例与一个纯函数用例。
- **`skill` 被归为并发安全，忽略了激活时的副作用**：正文的 `!cmd` / `@file` 注入在
  调用内执行（`inject::expand`），fork 技能还会跑子代理；与并发读并行会让注入命令的
  写入与读取竞争。该分类还使 `may_have_effects` 为假（它由 `is_concurrency_safe`
  推出），跳过 ToolStarted 记录，崩溃恢复会把跑过命令的调用说成"未运行"。改回串行
  分类，注释换成真实理由。
  归属说明：`true` 是 pre-range 的长期状态，范围里那次改成 `false` 的理由是 review
  的顺序屏障，已随 plan 230 回退；但分类本身的理由与 review 无关，仍然成立，因此这次
  按分类本身改回 `false`。

第二轮复核又一条，也成立：把标记换成精确类型名单时**收窄了原先能读的类型**——
`application/x-ndjson`、`application/json-seq`、`application/x-javascript` 原先靠
`json` / `javascript` 标记通过，改后既不在名单里也不满足结构化后缀，会退回
"unsupported content type"。改为：剥参数后按**子类型**判断，结构化后缀与安全标记各自
判断，只有二进制也会含的标记（`x-sh`、`sql`）整体匹配。按同一判据排查发现 `sql` 会命中
`application/x-sqlite3`（二进制），一并整体匹配，并补上 `x-ndjson` / `json-seq` /
`x-javascript` 正例与 `x-sqlite3`、`application/octet-stream; x=json` 反例。

第三轮同一条的继续：即使按子类型匹配，**标记子串仍然推不出文本**——
`application/vnd.openxmlformats-officedocument.wordprocessingml.document`（IANA 登记为
二进制）子类型含 `xml`，会被放行。至此三次都是同一个根因（x-sh、sqlite3、OOXML），
所以彻底放弃标记方案：改为**明确类型名单 + `x-` 别名/旧拼写 + 结构化后缀**，名单之外
一律拒绝；并把这份名单写进 DESIGN，附上三个二进制反例。这正是第二轮"只列精确类型会
丢别名"那条教训的解：不是回到子串，而是把别名补齐（x-json / json5 / ndjson /
xml-dtd / x-javascript / x-ecmascript / x-sql / x-csv / x-markdown 等）。

## 验证

- `cargo test -p kloop-web`：31 项通过（含新增的同站跳转标注、`Retry-After` 合法与
  过长值、`application/*` 文本类型三类用例）。
- `cargo test -p kloop-core tools::subagent::tests::fork_skill_reports_allowed_tools_names_kloop_does_not_have`：通过。
- 外层执行沙箱之外 `make check` 全绿：fmt、clippy、workspace debug 全量测试、本机 parity。
- `git diff --check` 与暂存后 `git diff --cached --check`。
- 无真实模型样本；不改共享调度，也没有新增模块。
