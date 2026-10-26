# Plan 241 — 用户配置替换内置系统提示词

> 2026-10-26。来源：用户希望系统提示词可以自己配置，并明确选择「替换内置提示词」。
> 状态：✅ 已完成（2026-10-26）。提交号以本节所在提交为准。

## 设计

- 唯一入口是 `~/.kloop/config.toml` 顶层 `system_prompt_file` 路径，正文放在独立的
  UTF-8 文本文件中。
- 相对路径基于配置文件所在目录（正常为 `~/.kloop/`），也接受绝对路径，不依赖 cwd。
- 未配置时使用 `BASE_SYSTEM`；配置后整体替换基础方针，不拼接内置正文。
- 保留文件正文原文；路径非字符串、空串或全空白，以及文件缺失、不可读、非普通文件、
  非 UTF-8、正文为空或全空白均在启动时报错，不静默回退。
- 环境块与开场 Git 快照仍由 `assemble_system` 追加；项目指令继续走独立用户消息。
- 进程启动时读取一次，普通 CLI、plain、headless 与服务端线程共用同一份基础正文；
  服务端按各线程 cwd 分别组装环境。重启进程后，新会话与恢复会话使用最新配置。
- 普通子 agent 继承父 Config，自定义 agent 类型的 `system` 继续按原约定覆盖；
  worktree 仍由既有路径改写环境中的工作目录。`--mock` 保持隔离，不读取用户配置。
- 不加环境变量、命令行覆盖或运行中热加载。

## 实现与验收

- `user_config.rs` 接受新顶层键；`startup.rs` 解析、校验并冻结正文；`context.rs`
  接收选定的基础正文；两个真实入口显式传入。
- 单测覆盖默认、空白/类型拒绝与 mock 忽略覆盖，根配置拒绝内联 `system_prompt`；
  集成测试覆盖相对/绝对文件、多行原文与文件错误。
- 使用本地合成 provider 捕获真实 CLI 请求，验证内置正文被替换、环境与项目指令
  保留，以及 headless 恢复与服务端线程使用配置正文。
- 改写 DESIGN 对应上下文段落，补 README 与配置示例，HANDOFF 记录进展与教训。
- 外层执行沙箱之外完整 `make check`，差异检查与暂存检查通过，一次提交。

## 完成记录

- CLI 启动层按配置目录解析 `system_prompt_file`，读取普通 UTF-8 文件并冻结基础正文；
  普通会话和服务端两个入口显式传入，继续由 core 追加环境与 Git 快照。
- 新增两条解析单测、三条本地合成 provider 集成测试，扩展根配置与 mock 断言。
  实际请求验证默认/替换、多行 Unicode 与原文空白、相对/绝对路径、重启后恢复、
  服务端不同 cwd 与进程快照；错误回归确认空白、非 UTF-8、缺失、目录和 FIFO 在
  sampling 前失败，FIFO 不阻塞。真实 provider、Linux 与 Windows 本次未实测。
- 外层执行沙箱之外完整 `make check` 全绿：fmt、workspace all-target/all-feature
  clippy、workspace debug 全量测试与本机 parity；CLI 158、core 1192、TUI 290、
  TUI PTY 25、plain PTY 8，以及新增文件集成 3 条全部通过。
- DESIGN 改写原上下文说明并修正服务端组装作用域，README 与 demo 同步文件入口；
  HANDOFF 记录教训 273 和另行待修的恢复提示污染 headless JSON 输出问题。
