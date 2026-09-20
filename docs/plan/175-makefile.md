# Plan 175 — 一条命令装上去

> 来源:2026-09-20,用户一句「加 Makefile,实现比如 make, make install, make test。其中
> make install, 如果 home 目录里没有 config, 把 config-demo 拷过去,并且提示用户修改,
> 如果已经有 config,则不做,只拷贝可执行文件。」

## 一、为什么仓库根需要一个 Makefile

三条命令此前分散在三处记忆里:CI 的 `ci.yml`(fmt/clippy/test/--mock/parity 的准确写法)、
README「Running」段(cargo run 的例子)、AGENTS.md 的完成标准(fmt + clippy + test 全绿)。
**同一组门禁写在三个地方,人手跑的时候总有一条被漏掉**——尤其 `--locked` 和
`--all-targets --all-features`,漏了就跟 CI 不是同一件事。`make check` 把这三条合成一个词,
参数照抄 CI。

另一件是安装:`~/.local/bin/kloop` 在这台机器上一直是手写的 dogfood 包装脚本。仓库里没有任何
东西说"怎么把 kloop 装到 PATH 上",新机器上只能重新发明一遍。

## 二、三个拍板点

**装到哪 —— `$(PREFIX)/bin`,默认 `~/.local/bin`,不是 `/usr/local/bin`。** 前者免 sudo,
而且这台机器上它本来就在 PATH 里、本来就住着 kloop。`PREFIX` / `BINDIR` 都可覆盖,
`BINDIR` 不在 PATH 上时安装完打一行 note(不报错——装到哪是用户的事)。

**配置有就一个字节都不碰。** 用户的原话是"如果已经有 config,则不做"。判据比"别覆盖用户文件"
更硬:`~/.kloop/config.toml` 里是这台机器的网关和凭据,**是我们既读不懂也重放不出来的东西**,
所以连"备份成 .bak 再铺新的"都不做——那只是把破坏推迟一步,还多出一份 0600 的凭据副本。
没有配置的那一支,目录 0700、文件 0600 一次做对(kloop 自己要求这两个模式),然后明说
"跑之前先改它":demo 里每个 `auth_header` 都是 `REPLACE-ME`,而 plan 172 之后
**kloop 没有任何兜底**,这个文件不改就没有能用的 provider。

**二进制先写 `.new` 再 `mv`。** 直接往 `$(BINDIR)/kloop` 上写,遇到正在跑的旧进程会
`ETXTBSY`;rename 是原子的,旧进程继续用它已经打开的 inode,下一次启动才换。

## 三、完成记录

## ✅ 已完成(2026-09-20;提交 SHA 以本条所在提交为准)

新增仓库根 `Makefile`(唯一新增文件):`all`(= `build`,release)/`debug`/`test`/`fmt`/
`fmt-check`/`clippy`/`check`/`parity`/`mock`/`install`/`install-config`/`uninstall`/`clean`/
`help`。每条 cargo 目标都 `cd rust &&`(仓库根不是 workspace 根),参数与 `ci.yml` 逐字一致。
`uninstall` 只摘二进制,不动 `~/.kloop`。

验证:`make install-config KLOOP_DIR=<tmp>` 两支都跑过——空目录那支铺出 0700 目录 + 0600 文件
且内容与 demo 逐字节相同,再跑一次报 `kept ... (already there, left untouched)` 且文件未变;
`make install BINDIR=<tmp> KLOOP_DIR=<tmp>` 装出可执行的二进制,`--version` 正常,
不在 PATH 的 BINDIR 打出 note,`.new` 不留残留,`make uninstall` 只摘二进制。
`make check` 全绿(fmt 干净、clippy `-D warnings` 无输出、**1602 passed / 0 failed**),`make parity` 通过。

README「Running」段开头补了这一节(Makefile 是仓库根新的入口,而 README 在 `rust/` 下)。
