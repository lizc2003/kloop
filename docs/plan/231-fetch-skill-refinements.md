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

## 验证

- `cargo test -p kloop-web`：31 项通过（含新增的同站跳转标注、`Retry-After` 合法与
  过长值、`application/*` 文本类型三类用例）。
- `cargo test -p kloop-core tools::subagent::tests::fork_skill_reports_allowed_tools_names_kloop_does_not_have`：通过。
- 外层执行沙箱之外 `make check` 全绿：fmt、clippy、workspace debug 全量测试、本机 parity。
- `git diff --check` 与暂存后 `git diff --cached --check`。
- 无真实模型样本；不改共享调度，也没有新增模块。
