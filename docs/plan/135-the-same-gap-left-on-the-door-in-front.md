# Plan 135 — 同一个缺口,在前面那道门上原样留着

> 来源:2026-09-11,用户截图:
>
> ```
> cd 被审仓库 && go test ./upstream/<pkg>/ \
>   -run 'Test<product>TransientAssetFailureFallsBackBeforeVideoSubmit|...' -count=1 -v 2>&1
> ⚠ no OS sandbox — full filesystem and network access
> > 1. Yes
>   2. No, and tell kloop what to do differently
> ```
>
> 「这个没出来 permissions 的选项」。

## 一、plan 129 修的是第二道门,用户撞的是第一道

两道门是分开的:

| 门 | 什么时候问 | opaque 脚本能记住吗(改前) |
|---|---|---|
| 沙箱升级 `escalate_sandbox` | 沙箱内跑过一遍、被拒之后 | **能**(plan 129,通配整个会话) |
| 普通权限 gate `ask_user` | 执行之前,每一次调用 | **不能** |

截图这条走的是**前一道门**:命令带着 `disable_sandbox: true`(notice 里的 `no OS
sandbox` 就是它),而 `sandbox_auto_allowed` 对带逃逸标志的调用返回 false,于是它一路
落到 `ask_user`。`remember_payload` 对 `BashAnalysis::Opaque` 返回 `None`,
`approval_scopes` 只剩 `Once`,前端就只画得出 Yes / No——**和 plan 129 截图里一模一样
的两行,只是来自另一个函数**。

把它变成 opaque 的是末尾的 `2>&1`:`analyze_bash` 的白名单遍历只放行
`&& || ; |`,重定向节点一到就整条判死(`shell.rs:107` 的 `ALLOWED_PUNCT_TOKENS`,已有测试 `ls > out.txt`)。
`cd A && go test …` 这半边本来完全可解析,是一个重定向 token 把整条拖下水的。

plan 129 的结论写的是「审查里的实证脚本几乎全是 opaque」,但只把这个认识用在了升级那
道门上;**同一份理由对 gate 一字不差地成立,当时没跟过去**。

## 二、做了什么

粒度按「能不能写成一条规则」分档,gate 这一侧新增最后一行:

| 调用形态 | 记什么 | 落盘 |
|---|---|---|
| 可解析 bash(`go test …`) | 两词前缀 `bash(go test *)` | 可写进项目 |
| 文件写 | 父目录 `write_file(src/**)` | 可写进项目 |
| **opaque bash**(重定向/替换/赋值) | **这一条命令原文,逐字** | **`bash_script(<原文>)`** |
| opaque PowerShell | 仍然什么都不记 | — |

- 会话签名 `bash-script[!no-sandbox]:<命令原文>`;持久规则 `bash_script(<原文>)` /
  `bash_script_no_sandbox(<原文>)`,新 `Rule::BashScript`,由同一个 `parse_rule` 读回来
  逐字比对。规则的右括号就是整条 entry 的最后一个字符,所以**命令里带括号也能原样
  round-trip**(测试用的命令故意带 `(`、`)`、`|`、`2>&1`)。
- **`disable_sandbox` 编进键**:沙箱内批准的那一句 yes,不能悄悄覆盖同一条命令的脱沙箱
  运行;反过来 `_no_sandbox` 那条**覆盖**同一原文的沙箱内运行(更强的同意包含更弱的)。
  whole-tool 的 `bash` allow 仍然一条 opaque 脚本都不放行——它是在没人读过这条脚本的
  情况下写下的。deny 方向只看原文本身(拒沙箱内、却放脱沙箱的,方向就反了)。
- 提示里 `remember_rules` 显示 `only this exact command text`,而不是把上面两行刚显示过
  的命令再印一遍(`Remember.echo` 与 `Remember.rules` 因此分成两个字段)。
- 没有给 gate 做 plan 129 那种通配:**gate 是第一道门,「本会话所有 opaque 脚本都放行」
  就是 bypass 模式换个名字**。升级那道门可以通配,恰恰因为命令已经先过了这道门。

顺带修了 README 的一处 drift:`Escalation consent is remember-able …` 段落还写着
「Opaque scripts offer no remember and are asked every time」——那是 plan 129 **改掉之
前**的行为,文档没跟。

## 三、非目标

- **不给沙箱升级那道门加逐字持久规则**。`sandbox_escalate` 侧对 opaque 仍然只有会话通配
  (plan 129);用户这次撞的是 gate,截图环境根本没有沙箱,升级门不触发。
- **不动 PowerShell**。bash 是解析过了才放弃,PowerShell 压根没有分析器,「每次都问」
  是它仅剩的安全网;`powershell_*` 两个测试把这个立场写死,本轮原样保留。
- **不收窄可解析路径的既有签名**。`bash:go test` 这类前缀签名同样没编码
  `disable_sandbox`,所以「沙箱内批准过的 `go test`」理论上能覆盖之后一次脱沙箱的
  `go test ./x`。只在 auto_allow 关闭的沙箱环境里够得着,属于既有行为,记在这里备查。

## 四、第二轮:只到会话级还是不够

用户当场 dogfood(截图二,已经能看到 `2. Yes, and don't ask again this workspace
session / only this exact command text`),一句「只是对话级,还是不方便,还是要加一个
project 级别的」——**本轮把「不给 Project 档」这条非目标推翻了**,因为它的理由(规则写不
下来)其实不成立:写得下来,只是不能写成前缀模式,得整条存。

## ✅ 已完成(2026-09-11;提交 SHA 以本条所在提交为准)

`crates/core/src/permissions.rs`:`Rule::BashScript` 与 `bash_script(...)` /
`bash_script_no_sandbox(...)` 的解析、`allows_script` / `hits_script` 两个方向的匹配、
`CallFacts.opaque_script`(原文 + 逃逸标志)、`allow_rules_match` 与 `rules_hit` 的 opaque
分支、`Remember` 拆出 `echo` 与 `persistable()`、`remember_payload` 的 opaque 分支、
`VERBATIM_REMEMBER_RULE`,以及 bypass 层注释更正。README 的规则语法表、Asking 段与
escalation 段。纯权限层改动,无 wire/schema 变化。

### 测试

- `opaque_bash_is_remembered_verbatim_for_the_session`(替换锁住旧行为的
  `opaque_bash_is_never_cacheable`):`cd sub && go test ./pkg -run X 2>&1` 拿到
  `Once + WorkspaceSession` 而**没有** `Project`;回显文案正确;同一条原文第二次不再问;
  同一条原文加 `disable_sandbox: true` **重新问**;另一条 opaque 仍然问。
- `a_verbatim_grant_persists_as_a_bash_script_rule`:`p` 之后 writer 收到的正是
  `bash_script_no_sandbox(cd sub && go test ./pkg -run 'X(Y)' -count=1 2>&1 | tail -15)`;
  把落盘的 raw 规则装进一个新 gate(相当于下一次会话)后,同一条调用不再问、同一原文的
  沙箱内运行也放行、另一条脚本照问。
- `a_bash_script_rule_knows_which_run_was_consented_to`:沙箱内那条规则加 whole-tool
  `bash` allow 都不放行脱沙箱调用;deny 里写同一条原文,一个人都不问就拒。
- 按 plan 91 的教训先 `--list` 证明两个名字命中,再 `--exact` 跑到 `2 passed`。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
`cargo test --workspace`,各自单独跑并当场取退出码(HANDOFF 111(b2))。
