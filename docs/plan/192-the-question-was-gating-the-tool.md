# Plan 192 — 拦的是工具,不是写盘

> 来源:2026-09-22 一次对话。用户截了一张审批弹层的图问「工程所在目录,为什么要询问?」,
> 查完答"manual 档下 `inside_cwd` 不参与判断,只有 accept-edits 档才免问",用户的判断是
> 「那应该缺省是 accept-edits,否则还要手工切」,接着是这一条:「manual 和 accept-edits,
> 能合吗」。定名与兼容性也是用户拍的:「还叫 manual,以及不用考虑兼容性,保持代码干净」。
> **✅ 同一次会话做完,见文末。**

## 一、原来是什么样

四档:manual / accept-edits / bypass / plan。manual 与 accept-edits 的**全部**差别是
权限管线第 9 层的一个 `if`——三个写工具(`write_file`/`edit_file`/`notebook_edit`)
且目标落在 cwd 内,则免问。其余十一层逐字相同。

而 manual 这一问,在今天的形态下拦不住它名义上要拦的东西:

- 沙箱缺省开,`trust_sandboxed` 缺省真,沙箱可写根就是 **cwd + tmp**;
- 于是第 6 层(sandbox auto-allow)让 `bash: sed -i src/x.rs` 在 manual 档**直接跑**,
  一句不问;
- 同一个文件走 `edit_file`,一路掉到第 12 层问人。

manual 拦住的不是"改工程里的文件",是"**用结构化工具**改工程里的文件"。它拦的是工具,
不是写盘。一个用 bash 就能绕开、而且模型每天都在用 bash 的门,不是门。

## 二、裁决

**合并。`Mode::AcceptEdits` 删除,它那一层改成无条件。** 活下来的档位仍叫 `manual`
(用户拍板;`default` 是上一轮特意砍掉的名字,不捡回来)。

判据不是"信任等级",是**围栏**:`write_file`/`edit_file`/`notebook_edit` 在过闸
**之前**就已经把目标解析、open、冻住了(dispatch 的 `prepare_mutation`),落在 cwd 内
的写在构造上就被框住——这和"沙箱能框住这条 bash,所以不必问人"是同一句话。围栏是
**调用**的属性,不是用户拨的旋钮,所以它不配当一个档位。

三条边界:

1. **上面的层一层不动。** deny 规则、敏感路径安全层(`.git`/`.kloop`/`.ssh`/`.gnupg`/
   `.aws`/`.env*`/rc 文件在 cwd 内照样每次问,bypass 都免不了)、plan 档硬拦写、显式
   ask 规则——全在第 9 层之上,合并一个都碰不到。
2. **越界不算被框住。** 路径逃出 cwd 就落到下面的层照问;worktree 子 agent 回头写主仓
   同理(`for_workspace` 重锚 cwd,这条行为不变,测试改名不改断言)。
3. **丢掉的那个东西有精确替代,而且位置更高。** manual 唯一真实的收益是落盘前那个阻塞
   的 diff 复核点;`[permissions].ask = ["edit_file(**)"]` 在第 5 层完整还原它——永不进
   记忆缓存、永不被 allow 规则压过,还能只盯一个目录。**一个档位如果能被一行配置还原,
   它就不配当档位。**

## 三、连带的三处

- **shift+Tab** 从三档循环变 `manual ⇄ plan`;bypass 照旧只能从 flag 进、退出落回 manual。
- **`--permission-mode accept-edits` 直接报错**(用户定:不留兼容别名)。`default` 之后
  第二个被彻底删掉的档位名。
- **headless**:审批缺省拒,但"够不到审批器就跑"的那批里现在多了 cwd 内的写——无 flag 的
  headless 会直接改自己的工作目录。这和它早就无条件直接跑沙箱内 bash 是同一条线,
  DESIGN.md 的 headless 段落写明了。

## 四、改动面

`permissions.rs`(枚举、`label`、`cycled`、第 9 层、模块头的管线图与理由)、`args.rs`
(解析 + help + 文档注释)、`app.rs`(循环注释 + 按键测试)、`config.rs` / `subagent.rs`
的注释、`plan_mode.rs` / `tool_search.rs` / plan53 / plan57 / plan59 里拿 AcceptEdits
当"能写"或"plan 的返回档位"用的测试、`headless.rs` 模块头、`real_agent_program_workflow.rs`
的启动参数,以及 `DESIGN.md` 九处。

`fs.rs` 的 `approval_ctx` 是唯一一处**真需要重新想**的:那六个测试锁的是审批期的目标冻结
与 workspace generation 固定,它们用的全是 cwd 内的普通路径——现在那是不问的。给它加
`ask = ["write_file(**)", "edit_file(**)"]`,正是用户要恢复复核时写的那一行,测试因此顺便
变成了逃生口的活证据。

## 五、✅ 完成

2026-09-22 排定并当次会话做完,一次提交(SHA 以本条所在提交为准)。`make check` 全绿
(fmt + clippy `-D warnings` + 全工作区 test)。开工前问清的两点都由用户拍板:活下来那档
仍叫 `manual`、不留兼容别名。

| 测试 | 锁住什么 |
|---|---|
| `permissions::…::manual_auto_allows_contained_writes_inside_cwd_only`(原 `accept_edits_…`) | 它不再是档位:`manual` 下 cwd 内三个写工具直接过,越界路径、`..` 逃逸、bash 照问 |
| `permissions::…::an_ask_rule_outranks_containment_and_never_caches`(**新**) | 逃生口:`ask = ["edit_file(**)"]` 每次都问、不给记忆项;规则没点名的写工具仍然免问 |
| `permissions::…::sensitive_paths_ask_every_time_and_never_remember`(换档位) | `.git/**` 在 cwd 内照样每次问——安全层在第 9 层之上,合并碰不到 |
| `permissions::…::rebased_reanchors_contained_writes_onto_the_new_cwd`(原 `…accept_edits…`) | 围栏跟着新 cwd 走:worktree 子 agent 回头写主仓照问 |
| `permissions::…::plan_mode_allows_reads_and_blocks_mutations`(未动) | plan 档硬拦 `write_file src/main.rs`——cwd 内的写也拦,plan 在第 3 层 |
| `permissions::…::allow_rules_match_tool_bash_prefix_and_path_glob`(改路径) | 路径 allow 规则从此只在 cwd 外有话说 |
| `permissions::…::file_write_remembers_parent_directory_scope`(改路径) | 只有会问的写才有记忆项;父目录作用域本身不变 |
| `args::…::parses_…`(改) | `--permission-mode accept-edits` 直接报错,和 `default` 同待遇 |
| `app::…::shift_tab_cycles_mode_and_mode_changed_syncs_badge`(改) | 循环收成 manual ⇄ plan |
| `fs.rs` 里走 `approval_ctx` 的六个审批测试(加 ask 规则) | 审批期的目标冻结与 workspace generation 固定仍有覆盖,顺带成了逃生口的端到端证据 |
| `server/tests/server.rs` 的 `factory(gated)`(换 gate cwd) | 三个挂在审批门上的协议测试(approval 往返、steer、busy/interrupt)仍然挂得住 |

实现里比设计更清楚的两件事:

1. **编译错误全是机械替换,`cargo test` 之后才是真正的改动面**——红了 9 个,外加一个
   **挂死**:`fs.rs` 的审批测试在等一个再也不会来的审批,那里的 1 秒超时罩的是"收到请求"
   而不是整条调用,于是整个 test binary 卡住、输出一片空白,`sample <pid>` 看栈才定位到。
   9 个里没有一个在测档位,它们只是需要"一个会走到审批器的调用",而 cwd 内的 `write_file`
   以前最顺手。教训 169 记了怎么逐个判断该换越界路径、该加 ask 规则,还是该动测试设施
   自己的 cwd——server 那三个是第三种:它们同时断言 `approvalScopes == ["once",
   "workspaceSession"]` 且 `rememberRules` 是数组,而 ask 规则走第 5 层、**不带记忆项**,
   加规则能让它们不再挂死,却会悄悄把测的东西换掉。正确的改法是把 gate 的 cwd 挪到测试
   不写入的地方,让那条写重新落回第 12 层。
2. **有一类劣化不会变红:路径 allow 规则。** `write_file(src/**)` 这种规则在 cwd 内从此
   够不到——第 9 层在它上面先放行了。它不报错、不失效,只是变成死规则。测它的用例要一起
   挪到 cwd 外,否则测的是"两条路都放行",而不是那条规则。用户配置里的同类规则同理:
   合并只会让它们授权更多的那一侧变得多余,不会让谁少拿到权限。
