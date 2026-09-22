# Plan 193 — 进门之前先问一次

> 来源:2026-09-22,紧接 plan 192 的同一次对话。用户贴了对照产品第一次进一个目录时的
> "信任这个文件夹吗"弹层,问「第一次在工作目录运行 kloop,询问用户了吗」。答:没有,
> 当初判断不需要(教训 51:kloop 不读仓库里的配置,恶意仓库改不了闸门),但那条判断只
> 覆盖**策略面**,不覆盖**指令面**。用户:「开一个 plan 做吧」。
> **✅ 同一次会话做完,见文末。**

## 一、为什么当初不做,以及为什么现在要做

教训 51 的判断今天依然对:`permissions` / `hooks` / `mcp` / `sandbox` 全部只来自
`~/.kloop/config.toml` 一个私有文件(plan 46 删掉了 cwd config),cwd 只是 workspace
anchor。**一个陌生仓库改不了你的闸门。**

但它改得了模型**做什么**。今天从仓库里读进来的有:`AGENTS.md` / `CLAUDE.md` /
`AGENTS.local.md`、`.kloop/rules/*.md`、以及**项目级 skills 与 commands**——skill 对模型
可见、可由模型自己激活,而 skill 正文里的 `` !`cmd` `` 内联是**会真执行**的(过闸门,
沙箱 contained 就不问)。也就是说:策略面是封住的,指令面是敞开的。

plan 192 之后还多了一条:cwd 内的结构化写不再询问。围栏的论证没变,但"我还没看过这个
目录里有什么"这件事,合并前后都没有人问过。**这个门补的正是这一句。**

## 二、裁决

**交互式前端在分发之前问一次,按 `ProjectId` 记住;拒绝即退出。**

1. **键在 `ProjectId` 上。** `WorkspaceIdentity::resolve` 对普通非 git 目录也给得出
   ProjectId(directory 域),所以 `~/tmp/xxxx` 这种目录同样记得住;git 仓库的 ProjectId
   锚在 common dir 上,于是**信任一次覆盖它的全部子目录与 linked worktree**——和项目级
   allow 规则同一套语义,不另造一套"路径前缀"规则。
2. **`--headless` / `--serve` 视为已信任**(用户 2026-09-22 拍板:谁调起它谁负责)。
   `--mock` 是 hermetic 的,永不问也永不落盘。**stdin 不是 TTY 时同理**——那不是有人
   坐在键盘前的场景,问了也没人答。
3. **拒绝 = 退出**,退出码 0(这是用户的选择,不是错误)。不做"未信任模式":那是另一个
   半档权限,而 kloop 刚刚才把半档合并掉。
4. **没有 ProjectId 时**(git 探测坏掉等罕见情况)照样问,但答应了也记不住,提示里说明。
   信任文件读坏、身份对不上 → 当作未信任(fail-closed),再问一次。

## 三、形状

`~/.kloop/projects/v1/<ProjectId>/trust.json`,和 `permissions.json` 并列、各有各的锁:

```json
{ "version": 1, "projectId": "p1_…", "trusted": true }
```

不并进 `permissions.json`——那是一张带 revision 的规则表,把"信任"塞进去会让它看起来
像一条规则,而它不是规则,是**这张表能不能开始生效**的前置。

问的位置是 `main.rs` 的 `run_front_end` 第一行,在 `SessionState::open` **之前**:
没被信任的目录不该留下一个会话文件。用的是阻塞 `read_line`,早于任何 tokio stdin reader
建立,天然绕开既有的"BufReader 预读吞输入"那条坑。

## 四、不在这一版里的

- **会话中途换 cwd**(`worktree` 工具 Enter 一个外部 checkout)不重新问。它本身已经是
  hazard、每次都要审批,再叠一个门是重复的;真正的边界留到那条路径上去想。
- 撤销信任没有命令,删掉那个文件即可(私有目录,`~/.kloop/projects/v1/<id>/trust.json`)。
- 不加 `--trust` / 配置预信任清单:headless 与 serve 已经按"谁调起谁负责"放行,没有第二
  个需要它的场景。

## 五、✅ 完成

2026-09-22 排定并当次会话做完,一次提交(SHA 以本条所在提交为准)。`make check` 全绿
(fmt + clippy `-D warnings` + 全工作区 test)。开工前问用户的那一个点(非交互入口怎么办)
答案是"视为已信任",已落进 `asks_for_trust`。

落地形状:`cli/src/trust.rs`(新,约 130 行含测试)+ `project_store.rs` 的 `trust.json`
读写 + `main.rs` 分发前一行 + `startup.rs` 暴露 store。

| 测试 | 锁住什么 |
|---|---|
| `trust::tests::only_an_interactive_launch_asks` | 裁决本身:serve / headless / mock / 非 TTY 四种都不问,只有交互式启动问 |
| `trust::tests::nothing_but_yes_continues` | 只有 `y`/`yes` 放行;空行、`no`、`yep`、EOF 全是退出 |
| `project_store::tests::trust_round_trips_beside_the_rules_without_touching_them` | 授权前不创建任何状态;授权后 `trust.json` 整对象断言,`permissions.json` 不受影响;重复授权幂等 |
| `project_store::tests::a_damaged_or_foreign_trust_record_reads_as_untrusted` | 六种坏记录(`trusted:false`、版本不符、别的项目、缺字段、不是对象、不是 JSON)一律 fail-closed |

真二进制验过三轮(debug build,自己开 pty 驱动):陌生目录弹出提示 → 答 `n` 退出码 0
且不建会话文件;答 `y` 进 REPL 且 `~/.kloop/projects/v1/<id>/trust.json` 落盘;同目录
第二次启动直接进 REPL 不再问。验完删掉了那条演示记录。

两件比设计更清楚的事:

1. **门开在启动路径上,代价落在所有 pty e2e 上。** `plain_pty` / `tui_pty` 共用的
   `ChatFixture` 起的是真二进制、真 pty,于是它们的第一屏全变成了这个提示,24 条全红
   ("plain boot: timed out")。修法不是让它们去答这一问——那会把每条用例的转录都改掉,
   而它们测的是 boot 之后的事——而是**让沙箱一开始就是被信任的**,和一个回头再来的会话
   一样:用真的 `WorkspaceIdentity::resolve` 算出 id,往临时 `$HOME/.kloop/projects/v1/<id>/`
   写一份 `trust.json`(0700/0600)。顺带它也成了读路径的交叉验证——那份文件是手写的,
   不是 `grant_trust_blocking` 写的。
2. **第一轮 accept 路径验假了。**`printf 'y\n' | script -q /dev/null kloop`
   里 `script` 会先把管道的 EOF 送进 pty,`read_line` 拿到 0 字节 → 当成拒绝 → 看起来像
   "信任没落盘"。换成自己 `pty.openpty()`、**等提示符真的出现在 master 上再写答案**,一次
   就对了。教训 171 记了这条。
