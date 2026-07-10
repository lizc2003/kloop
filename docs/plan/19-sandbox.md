# Plan 19 — 沙箱基建(第一片)

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
