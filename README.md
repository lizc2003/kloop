# kloop

一个用 Rust 从零写的终端编码 agent。给它一句话,它自己读代码、改文件、跑命令、查资料,
一直做到交差。

> 状态:自用中,不是发行版。配置和接口还在改,没有兼容承诺。

## 它能做什么

- **接任何网关** —— messages / responses / chat 三条线协议各一套完整实现,一个 provider
  写一段 profile。模型、effort、上下文窗口都在配置里说清,运行时不做发现。
- **一整套工具** —— 读/写/编辑文件、bash(前台可中断、后台可轮询)、grep/glob、
  notebook、网页抓取与搜索;Windows 上是原生 PowerShell,不是 bash 模拟。
- **先问再动手** —— 每次工具调用都过一遍权限判定,读文件放行、删东西问你;同意过的可以
  记住。macOS 上再套一层 seatbelt 沙箱:写只能落在白名单里,还能整个断网。
- **会话是可以回到过去的** —— 全部落盘。`-c` 续上一次,`-r` 从列表里挑一个,
  `--fork <id>#<行号>` 从中间某一步分叉重来。
- **不是一个模型在干活** —— 子 agent(同步派或后台跑)、MCP 客户端(stdio / HTTP /
  OAuth 登录)、skills、code mode(让模型写一段 JS 来编排工具,一百次循环只回一条结果)。
- **不止 TUI** —— `--headless` 一次跑完适合进脚本(可输出 NDJSON 事件流),`--plain` 是
  行模式 REPL,`app-server` 把它变成一个跑在 stdio 上的 agent 服务。

## 装上

需要 Rust 1.96+。主力平台是 macOS(沙箱目前只有 macOS 后端);Linux 和 Windows 能编能跑,
CI 三个平台都跑。

```sh
make install          # release 构建 + 装进 ~/.local/bin(PREFIX= 可改)
```

第一次安装会把 `config/config-demo.toml` 铺成 `~/.kloop/config.toml`(目录 0700、文件 0600),
**然后你必须去改它** —— demo 里的 key 全是 `REPLACE-ME`,kloop 没有任何兜底。
已经有配置的话它一个字节都不碰,只换二进制。

## 用起来

```sh
kloop                          # 进 TUI,当前目录就是工作区
kloop --mock                   # 不要 key 的演示,跑一遍就知道长什么样
kloop -c                       # 接着上次那个会话
kloop --headless "修一下 CI 里那个 flaky 测试"
kloop --worktree=fix-ci        # 在一棵独立的 git worktree 里干活
kloop --help                   # 全部开关
```

## 配置

只有一个文件:`~/.kloop/config.toml`。没有任何环境变量能决定这次运行接哪个网关——
换了 shell 也不会换答案。

```toml
provider = "my-gateway"

[providers.my-gateway]
wire_api    = "messages"                                # messages | responses | chat
base_url    = "https://gateway.example.com"
auth_header = { Authorization = "Bearer ..." }          # 或 { x-api-key = "..." }
model       = "claude-opus-4-8"
effort      = "high"
```

权限、沙箱、MCP、hooks、skills、子 agent、code mode 的开关也都在这个文件里,
完整字段见 [`config/config-demo.toml`](config/config-demo.toml)。

## 仓库里有什么

| 路径 | 是什么 |
|---|---|
| `rust/` | cargo workspace,10 个 crate:`core` 是引擎,`cli` 是二进制入口,`tui` / `server` 是另外两种前端 |
| `rust/README.md` | 设计与行为的详细说明:每条取舍为什么是这样。长,但那是唯一权威 |
| `docs/plan/` | 一个编号文件 = 一次开发任务,连同踩过的坑;`HANDOFF.md` 是当前状态 |
| `config/` | 配置样例 |

`make help` 列出全部构建目标(`make check` = fmt + clippy + test,与 CI 同令)。

## 许可

[Apache-2.0](LICENSE),Copyright 2026 lizc2003@gmail.com。
