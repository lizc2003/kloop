# Plan 19 — 沙箱基建 ✅(片 1 完成 68bcf18;片 2 完成 41b4fd1;片 3 完成,提交号见下)

> 可能不止一个会话,开工时切片。开工前先读 docs/plan/HANDOFF.md。参考:refs/README.md 权限系统对比"codex 独有、kloop 暂不做"一节——sandbox+approval 双轴、escalation 环、execpolicy、Starlark 规则都**依赖沙箱基建**,这个 plan 就是去补基建;codex codex-rs 的 seatbelt(macOS)/landlock+seccomp(Linux)实现回源精读(教训 11,这是安全层,更不能凭印象)。

## 目标

bash 工具可在受限沙箱里执行(文件系统写限 cwd、默认断网),从而解锁 codex 那套"受限沙箱内少问、失败升级询问后裸跑"的 escalation 体验。第一片先做执行原语 + 最小策略;escalation 环、execpolicy 是后续片。

## 设计要点

- **平台**(开工时问用户):先 macOS seatbelt(开发机,`sandbox-exec` profile)还是双平台一起;CI 两平台都有,做哪个测哪个。
- **与权限管线的关系是本 plan 最大的决定**(开工时定):codex 的形态是沙箱改变询问策略("会被沙箱兜底的就不问"),这会动 plan 8 的管线顺序;保守形态是先只加一层"批准后仍在沙箱里跑",不动询问逻辑。倾向保守起步,双轴联动放下一片。
- **策略粒度**:写 = cwd + tmp;读 = 全盘还是也收紧;网络默认禁、config 开口子。粒度开工时定,但"解析不了/不确定 = 最严"的白名单原则沿用(教训 8)。
- **失败的呈现**:沙箱内非零退出要能区分"命令本身失败"和"被沙箱拦了"(后者是 escalation 的触发信号)——seatbelt 的报错形态去实测,别猜。
- **归属**:执行原语放 core 还是独立 crate(有平台条件编译),开工时定。
- `--mock` / 测试:沙箱不可用的环境(CI 容器?)优雅降级 + 告警,不挡运行。

## 测试

沙箱内写 cwd 外失败、写 cwd 内成功;断网生效(curl 之类失败);沙箱不可用时降级路径;平台条件编译两边 CI 都绿。

## 完成标准

fmt/clippy/test 全绿(macOS+Linux CI 过);手工验收:真 key 让模型试写 /tmp 外任意路径与访问网络,观察被沙箱拦下的报错回给模型;README、HANDOFF 更新;下一片(escalation 环)的取舍写进本文件末尾。

## ✅ 第一片完成记录(2026-07-10)

**开工决定**(与用户对齐):平台 = 先 macOS seatbelt,Linux/Windows 留缝优雅降级(用户明确 Windows 也是后期目标);管线关系 = 保守 + 最小逃逸口——沙箱是权限门**之下**的执行层,批准后仍罩沙箱跑,bash 加 `disable_sandbox` 参数(cc 的 dangerouslyDisableSandbox 形态),带参调用走同一权限门(describe 打 `[no sandbox]` 标签),plan 8 管线零改动;粒度 = 写 cwd+/tmp+$TMPDIR+config extras、读全盘、网络默认禁 `[sandbox] allow_network` 开口;归属 = `core/src/sandbox.rs` 模块(策略→argv 纯函数,学 codex sandboxing crate 的解耦;Linux 片要 helper 二进制时再拆 crate)。

**回源调研结论**(两个 Explore agent 精读,细节在会话):两家收敛 = deny-default + 读全盘 + 写白名单(cwd+tmp)+ 网络默认禁 + `sandbox-exec` 外部二进制 + "沙箱内少问/失败升级批准后裸跑"。分歧:escalation cc 是模型驱动(参数 + prompt 教学),codex 是 orchestrator 代码环;denial 判定 codex 纯输出判定(7 关键词 + 2/126/127 快速否决 + Linux 128+SIGSYS),cc 靠 macOS `log stream` 抓内核事件(重,未抄);Linux 两家现役都是 bwrap+seccomp,**codex 的 landlock 已退居 legacy**(下一片对着 bwrap 设计);`.git` 粒度 cc 只锁 hooks+config(git commit 沙箱内可用),codex 锁整个 `.git`(靠 escalation 付账)——kloop 取 cc 粒度,另锁整个 `.kloop`(权限规则所在)。

**实测事实**(plan 要求实测别猜,均已进测试或注释):拦截报错 = `Operation not permitted`(exit 1,命中关键词表);**继承 fd 跨界写可行**(seatbelt 在 open 时检查,不逐 write)——后台输出文件不需要开洞;`sh -lc` 沙箱内正常;**断网沙箱里 DNS 失败报 `Could not resolve host`(curl exit 6),不命中 codex 关键词表**——kloop 扩展:network_disabled 时 DNS 失败形态(could not resolve host / name resolution / nodename nor servname / getaddrinfo)也算 denial 证据。

**落地**:`core/src/sandbox.rs`(SandboxPolicy/WritableRoot、SBPL 纯函数生成——base/network 模板逐字来自 codex 的 .sbpl 文件、路径经 `-D` param 不内联、denial 判定移植+DNS 扩展、availability 探测)+ bash 前台/后台接线(`shell_command` 统一包装点、BgShell 记 sandbox 旗标、denial hint 注入 tool_result)+ `Config.sandbox: Option<Arc<SandboxPolicy>>`(子 agent/server 线程随 clone 继承)+ cli `[sandbox]`(enabled/allow_network/writable_roots,未知键报错)+ `AGENT_SANDBOX=off` + 不可用降级警告(fail-open,权限门仍是强制层)+ `--mock` 恒不沙箱。测试 +13(=285):SBPL 整串断言、workspace roots 规范化去重、denial 表、`[no sandbox]` 标签、cli 解析,macOS-only 真 sandbox-exec 集成 6 个(写内/写外+hint、保护子路径、逃逸、真 listener 判别断网、后台继承 fd + 后台 denial 注释)。

**真 key 验收**:anthropic 轨(sonnet-5)全闭环——写 `~/...` 被拦 → hint 回给模型 → 自发 `disable_sandbox: true` 重试成功;curl 断网(DNS 拦截)同样闭环。openai 轨(gpt-5.4-mini)看到 denial 先汇报征求确认(保守但正确),任务声明访问必需后正确带参升级成功。`[no sandbox]` 标签在审批提示中确认可见。验收方法注记:管道 stdin 下 plain REPL 的异步 reader 会吞掉后续审批行(交互终端无此问题),脚本化验收用 `--yolo`(bypass 只影响审批不影响沙箱,恰好也验证了这一点)。

## ✅ 片 2 完成记录(2026-07-10):auto-allow 双轴联动

cc 的 autoAllowBashIfSandboxed 形态落地:plan 8 管线在 ask 规则之后、bypass 之前插入 sandbox auto-allow 层——会被沙箱兜住的 bash 调用跳过其下所有询问层。三个关键取舍:① **opaque(重定向/子 shell)也覆盖**:OS 遏制替代解析级审查,这是主要 UX 收益(`echo > file` 类询问噪音清零);接受的风险 = 藏在 opaque 里的破坏可无询问改工作区(.git/hooks|config、.kloop 仍受 SBPL 保护,git 史兜底),两家参考同款取舍。② **deny/安全检查/显式 ask 规则保持优先**——可解析的 `rm -rf` 仍问;ask 规则这半**比 cc 严**(cc 的 tool 级 ask 规则会被沙箱 auto-allow 跳过,kloop 认为"always confirm"是用户的话,自动化不得盖过)。③ **判定按调用喂入**:`Permissions::check_call` 第 4 参 `sandbox_auto_allow`(gate 不感知 sandbox 模块;旧 `check` = 恒 false 包装,签名零churn),dispatch 由 `bash::sandbox_auto_allowed`(bash + 未逃逸 + policy.auto_allow)计算;`disable_sandbox: true` 调用天然不享受。配置 `[sandbox] auto_allow` 默认开,false 回退片 1 纯遏制形态。测试 +3(=288):分层契约(contained 跳问含 opaque、同调用不 contained 照问、deny+安全+ask 压过沙箱)、macOS 端到端(无 approver 下 contained 跑通/逃逸被拒/auto_allow=false 回退)、cli 解析。真 key 验收:默认模式(非 --yolo)sonnet-5——重定向写 cwd 零询问直接跑,写外拦截 → 自发升级 → `[no sandbox]` 审批弹出(拒后如实汇报);gpt-5.4-mini 零询问路径同过。

## ✅ 片 3 完成记录(2026-07-10):代码级 escalation 环

codex orchestrator 的 retry-on-denial 落地为 bash 工具内的循环:沙箱内命令 denial-shaped 失败 → 代码自问一次"去掉沙箱重跑吗" → 批准即**同一次工具调用内**裸跑重试。收益 = 省一轮模型往返(片 1/2 是 denial→模型→disable_sandbox 重试两次工具调用;片 3 一次搞定,验收里 bash 只调一次)。设计:

- **只问"移除遏制",不重查命令**:命令已过权限门(deny 层 1、safety 层 2 在 sandbox auto-allow 层 4 之上),到 bash_tool 执行时已是"可运行",escalation 只问是否去掉 OS 遏制——与 codex 一致(它的 initial approval 也只做 assess_command_safety,escalation 是独立的 containment-removal 同意)。因此不需要对裸跑变体重跑规则管线,没有绕过 deny 的洞(被 deny 的命令根本到不了 bash_tool)。
- **三态**:`Permissions::escalate_sandbox(command, depth) -> EscalationOutcome`。Approved(用户批准 / Bypass 自动批准,因 --yolo 下 disable_sandbox 本就在 bypass 层自动放行,保持一致)→ 裸跑重试、结果加 `ESCALATED_PREFIX` 前缀;Declined(用户拒绝)→ 保留沙箱失败 + `ESCALATION_DECLINED`(明确别再劝 disable_sandbox,引导换法);NotAttempted(无 approver / allow_all 测试 / mock)→ 回退片 1/2 的 `DENIAL_HINT` 模型驱动。判定纯代码、provider 无关。
- **前台专属**:后台命令立即返回,denial 在 bash_output 里出 hint(不改);gate 不感知 sandbox 模块——escalation 消费者是 bash_tool,`escalate_sandbox` 只用 approver + mode。`disable_sandbox: true` 的调用 sandbox=None,天然不进 escalation 分支。
- 配置 `[sandbox] escalate` 默认开,false 回退模型驱动 hint。测试 +3(=291):三态映射(含 bypass 不问、无 approver NotAttempted、描述串)、macOS 端到端(批准后裸跑写入成功且无残留 hint / 拒绝后保留失败且不含 hint 邀请 / 各问一次)。真 key 验收:anthropic --yolo 自动批准路径——bash 单次调用完成"拦截→自动升级→裸跑写入",模型拿到 escalated 结果;非 --yolo 下 `[sandbox denied — run without sandbox?]` 审批提示正确弹出(管道 stdin 吞行是 plain REPL 老限制,交互终端无碍),拒绝路径下模型如实换到工作区内写。gpt 轨倾向预设 disable_sandbox(走逃逸参数路径),escalation 环是 provider 无关代码,anthropic 轨已证实。

**剩余(仅平台扩展)**:① Linux 片对着 bwrap+seccomp 设计(landlock 是 legacy),需要 helper 二进制(arg0 dispatch)时把 sandbox.rs 拆成独立 crate;本机无 Linux,宜等仓库推远端后 Linux CI 能实跑再做。② Windows 更后。③ 小挂账:seatbelt 下 `.git` 文件型(worktree gitdir 指针)未解析真实 gitdir(codex 有,罕见场景);bash 工具描述提沙箱但 system prompt 未提(靠 denial hint / escalation 事发时教学,验收显示够用)。macOS 侧沙箱(执行原语 + auto-allow + escalation 环)已完整。
