# Plan 161 — 跑着的这个二进制,是哪一份代码

> 来源:2026-09-17,用户看着启动横幅说「应该显示程序的版本号,比如编译时的 git」。
> 当场做完(✅ 见文末)。

## 现状

启动横幅(`render.rs::session_header_lines`,plan 38 片 6)只有四行:model、cwd、
branch、mode。**全仓一处都没有读过 `CARGO_PKG_VERSION`**,也没有 `--version`;
workspace 版本从建仓到今天一直是 `0.1.0`,单独显示它等于什么都没说。

于是横幅回答不了唯一值得问的那个问题:**我现在跑的这个二进制,是哪一份代码编的。**

## 裁决

### 一、戳的是编译时的 commit,不是运行时的

kloop 是通用 agent,**会在任意目录下运行**,cwd 的 git 是用户项目的 git,不是 kloop 自己的
(横幅里那行 `branch` 就是用户项目的分支)。所以"跑一次 `git rev-parse` 看自己是哪个版本"
这条路根本不存在,只能编译期戳进二进制。

### 二、戳进去的是 `crate version + 短 sha`,**不带 dirty**

形状 `v0.1.0 (2319ea3)`,sha 固定 `--short=7`(不跟 `core.abbrev` 走,免得基线宽度随仓库大小变)。

**不做 `-dirty`**,两条理由:

- 准确的 dirty 要求 build script **每次构建都重跑**(改一个 `.rs` 文件不会碰 `.git/` 里的
  任何东西);只在 HEAD 移动时重跑的话,dirty 标记会停在上一次 commit 时的状态,**是错的**。
- 就算做准了,开发中它几乎永远亮着,信息量趋近于零。而它想回答的那个问题——"我刚改的代码
  在不在这个二进制里"——dirty 也答不了(编译完再改,它照样说 dirty)。

sha 的语义因此写死成:**这次构建坐在哪个 commit 上**,不是"树是干净的"。

### 三、戳在标题行右边,不占新的一行

照 cc(`Claude Code v2.1.x` 在框第一行)和 codex。横幅已经吃掉首屏五行,版本是**二进制的
身份**不是**会话的属性**,不配跟 model/cwd/branch/mode 平起平坐占一个 label 行。

窄终端放不下就**整条丢掉,不截断**:半个 sha 指向的是另一个 commit,比不显示更坏。

### 四、build.rs 放在 `crates/cli`(叶子),不放在公共 crate

版本是**二进制**的属性。放 `core`/`protocol` 这类被所有人依赖的 crate 里,build script 一
重跑全 workspace 跟着重编。放 cli:重跑只影响 cli 自己,而且 Cargo 只在 build script 的
**输出变了**时才重编 crate——HEAD 没动时这条路是零成本的。

TUI 因此**不自己读版本**:`run()` 多一个 `version: &str` 参数,`Cell::SessionHeader` 多一个
`version` 字段。这也正好让整屏基线能 pin 住它(见第六条)。

### 五、两个逃生口

- `KLOOP_BUILD_SHA`(**构建时**):有它就直接用,不跑 git。给 tarball / 无 checkout 的打包。
- `KLOOP_VERSION`(**运行时**):替换整条串。PTY 整屏基线靠它 pin。

两个都没有、又不在 checkout 里时,横幅显示光秃秃的 `v0.1.0`,**不编造 commit**。

### 六、rerun 的边界

`cargo:rerun-if-changed` 只挂三个:gitdir 的 `HEAD`、当前分支的 ref 文件、`packed-refs`。
**不存在的路径要剔掉**——Cargo 对 stat 不到的路径当成"永远脏",挂上去等于每次构建都重跑
build script。剔掉是安全的:`refs/heads/main` 与 `packed-refs` 这两种状态是一起换的,
`git gc` 把 ref 打包走,那个文件的**消失本身**就是我们正在监听的那条路径的变化,下一次重跑
自然改挂 `packed-refs`。linked worktree 里 `HEAD` 在自己的 gitdir、refs 在 common dir,
所以 ref 用 `--git-common-dir` 解析。

### 七、`-V`/`--version`(用户追加)

横幅之外再给一个不用起会话就能问的入口,输出与横幅同一条串:`kloop v0.1.0 (2319ea3)`。
和 `--help`/`--list-sessions` 一样是 **local fast path**——在解析 provider 凭据、加载 runtime、
连 MCP 之前就返回,配置坏了也答得出来。

**不占 `-v`**:小写那个字母读起来是 verbose,留给将来;`-v` 仍然落在未知参数那条错误上,
并有一条测试钉住这件事。

## 顺带修掉的一个真 bug:框比终端宽 2 列

`cap = width.saturating_sub(2)` 只减了两条竖线,可一行的 chrome 是**四列**(`│ ` … ` │`)。
于是内容顶到 cap 时,框会比终端宽两列而折行。以前标题行短、很难顶到,加了版本才撞出来。
改成 `saturating_sub(4)`。

## 做了什么

- 新增 `rust/crates/cli/build.rs`:env 优先,否则 `git rev-parse --short=7 HEAD`,
  注入 `KLOOP_BUILD_SHA`,并按第六条挂 rerun。
- `args.rs` 加 `version_string()`(运行时 env 覆盖)+ 纯函数 `format_version()`。
- `render.rs`:`session_header_lines` 多一个 `version` 参数,标题行拼 dim 戳;
  放不下丢掉;cap 修正。
- `app.rs` / `lib.rs`:`Cell::SessionHeader` 加 `version` 字段,`run()` 加参数,
  main.rs 在调用点传 `version_string()`。
- `tui_pty_support`:被测二进制注入 `KLOOP_VERSION=v0.0.0 (0000000)`。
- `CliArgs` 加 `version: bool`,`-V`/`--version` 在 main.rs 的 help 分支旁边退出。

## ✅ 验收(2026-09-17,两次提交:第二次是用户要的 `--version`;提交号以本条所在提交为准)

`cargo fmt` + `cargo clippy --workspace --all-targets --all-features -D warnings`(零警告)
+ `cargo test --workspace` 全绿。另外用 `strings` 核过 debug 二进制里确实嵌着当前 HEAD 的
`2319ea3`。

| 测试 | 守住什么 |
|---|---|
| `args::version_names_the_commit_only_when_the_build_stamped_one` | 有 sha 是 `v0.1.0 (2319ea3)`;没有(或空白)就是 `v0.1.0`,不出现空括号 |
| `render::session_header_renders_a_branded_box_with_fields` | 戳在标题行上,和 `>_ kloop` 同一行 |
| `render::session_header_drops_the_build_stamp_before_truncating_it` | 窄终端整条丢掉而不是截半个 sha;框不超过终端宽度(cap 修正的回归) |
| `tui_pty` 六张整屏基线 | 真二进制跑出来的横幅新宽度 |
| `args::parse_args_version_flag` | `-V` 与 `--version` 两种拼法都只动 version 一个字段;`-v` 仍是未知参数 |
| `args::parse_args_help_flag` | usage 文本里点名 `-V, --version` |

README 同步三处:横幅段(戳的含义、两个 env、窄屏行为)、整屏基线的规范化清单、
local fast path 段(`--version` 与不占 `-v`)。实跑核过 `kloop --version` / `-V` 输出
`kloop v0.1.0 (36fd556)`,`-v` 报未知参数。
