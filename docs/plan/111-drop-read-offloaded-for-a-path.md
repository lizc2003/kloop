# Plan 111 — 删掉 read_offloaded，改用路径 + 通用文件工具（cc 形态）

## 触发

用户看着 plan 110 的成果问了两句:"read_offloaded 这个名字不直观"、"参数从哪里来,参考项目有类似
这个的吗"。查下来这两问指向同一件事——**这个工具本身就不该存在**。

**参数只有一个来源。** `NEXT_OFFLOAD_ID` 铸出 `off-{:04}`(`history.rs:405`),模型唯一能看到它的
地方是那一句指针字符串(`history.rs:429`,后台 agent/program 结果走 `agent.rs:1022/1032` 的
`offload_text` 同一条路)。推导不出、猜不到、也枚举不了——没有 list 工具。**一个工具的唯一参数,
只能从另一条工具结果的文本里抄。**

**参考项目的答案。** kloop 自己的 parity 矩阵(`refs/claude-code-2.1.220/tool-matrix.json`)记着:

```
Read                   ↔ read_file
ReadMcpResourceTool    ↔ read_mcp_resource
None                   ↔ read_offloaded      ← cc 侧为空
```

cc **有等价机制、没有等价工具**:大结果给 `outputFile` 绝对路径 + `canReadOutputFile` 标志,回读用
普通 `Read`。codex **连机制都没有**:本次 dogfood 的 rollout 里只有
`Warning: truncated output (original token count: 53729)`,直接截断不保存,137 次调用里 53 次被截,
模型只能重跑更窄的命令。

名字不直观是表症。真问题是 kloop 比 cc 多一个工具、多一套不透明 id 词汇,而这套词汇只服务于一个
无法自证来源的参数。

## 为什么 plan 109 试过路径却退了回来，以及第二次调查纠正了什么

Plan 109 的指针本来就给了绝对路径——那正是 cc 的设计。它失败是因为通用工具读不了那份工件,于是
plan 110 退回 id。本 plan 开工时我的判断是"给 `read_file` 加 `raw`",**这个判断是错的**,是用户
追问"参考项目是怎么做的"逼出来的纠正:

- **cc 的 `Read` 也恒定 `cat -n` 编号,没有 raw 开关**(描述原文:"Results are returned using
  `cat -n` format, with line numbers starting at 1";参数只有 `file_path`/`offset`/`limit`/`pages`)。
- **codex 连 Read 都没有**,唯一工具是 JS runtime 里的 `exec_command`,本身就是 shell。

也就是说"把大文件原样搬进上下文"这个需求在两个参考项目里**都不存在**。它们拿大工件的办法是
**在文件旁边跑一段代码,只打印抽取结果**——cc 读那份 2.1MB spec 用的是 `curl -o` + `python3 -c`
打印三个字段。

那 kloop 为什么照抄不了?**障碍在沙箱,不在 `read_file`**:

```
crates/core/src/sandbox/mod.rs   "full-disk read minus credentials"
                                 ("DENIED_READ_0", "/home/u/.kloop")
```

`~/.kloop` **整个**在 deny-read 里(因为 `config.toml` 存着 provider 凭据),offload 目录连坐,于是
kloop 的 bash `cat` 不到自己的 offload 文件——只能走 escalation(多一轮 + 一次审批)。cc 没这个
问题,它的 Bash 读得到 outputFile。

所以最小改动是**开沙箱的一个只读缺口**,不是给 `read_file` 加参数。`raw` 那一版实现已撤销。

## 改动

1. **沙箱只读缺口**(`sandbox/mod.rs` + `cli/startup.rs` + `cli/main.rs`)。新增
   `SandboxPolicy::allowed_read_paths` 与 `with_allowed_read_path`,SBPL 在 deny 之后再发一条
   `(allow file-read* …)`——seatbelt 取最后一条匹配规则。放行的只有**本项目的 offload 目录**:
   `config.toml`(凭据)与 `sessions/`(所有项目的转录)继续禁读,**写**也继续禁。两处调用点把
   `session_dirs.ensure(cwd)` 上提到 `build_sandbox` 之前(它只依赖 cwd)。

2. **offload 指针改成 cc 形态**(`history.rs::spill`)。给路径与字符数,指路"就地查、别读回来",
   并点名最便宜的做法是 `run_program` 里 `tools.bash({command:"python3 -c '...'"})`。
   **模型面不再出现 "offloaded" 这个词,也不再有 id。**

3. **删掉 `read_offloaded`**:工具定义、dispatch、只读/并发安全表、TUI 行、`OFFLOAD_WINDOW_CHARS`、
   `MAX_INVITED_WINDOWS`、`read_offloaded_tool` 本体、`--mock` 演示脚本里的那一步,以及全部相关
   测试。不留 alias(plan 107 教训 (b))。

4. **`agent_type` 的基建例外收缩**为 `send_message`/`list_agents`。原先 `read_offloaded` 在里面,
   是因为它是个除了"回读自己的大输出"什么也不能干的口子;现在工件就是普通文件,打开它用的是
   `read_file`/`grep`/`bash`——**允许清单可能是有意要收走的真能力**,不该偷偷豁免。

## 第二道门:实测才发现沙箱不是拦路的那个

改完上面四条,第一轮真实验收**回归了**:一次打满轮次没答,一次答对但过程是子 agent 放弃文件、
5 次 web_search、最后 `python3 urllib` **重新下载**。把 carve-out 单独测一次才看清:

```
[bash {"command":"wc -c …/offload/off-0001.txt"}]
bash: reading this sensitive path is blocked. Do not retry or work around the protection.
```

这句来自 **`permissions.rs:830`**,不是 seatbelt。**有两道门,沙箱是第二道**:
`mentions_sensitive_needle` 把 `~/.kloop` / `/.kloop/` 当硬编码 needle,任何提到它的 bash 命令在
**权限层**就被否决——在沙箱之前,且注释写明"hard verdict before plan/sandbox/bypass and cannot be
remembered or approved away",`--permission-mode bypass` 也豁免不了。

所以第五条改动(用户单独拍板):**在权限层加第二条结构化豁免**,与既有的
`.kloop/worktrees/` 同型但更窄——只放行**文件名**为 `off-NNNN.txt` / `bg-N.out` 的路径。

- **路径层**(`path_is_sensitive`):读**尾部**而非下一段,因为 store 在中间按项目分区
  (`.kloop/projects/v1/<id>/offload/off-0001.txt`)。命中时 `continue` 跳过 `.kloop` 而不是直接
  返回,扫描继续往下走,所以路径别处的 `.ssh` 或嵌套 `.kloop` 照样触发。
- **字符串层**(`mask_spill_tokens`):**按 token** 中和,不是整串替换——否则屏蔽一个路径会把同一条
  命令里旁边那个 `.kloop/config.toml` 一起解锁。`..` 一律作废豁免,与 worktree 同规。

放行的理由:这些文件是模型**自己的**输出,它已经拿到 head/tail 预览和这个路径,再读一遍不会泄露
任何工具本来不会返回的东西;而拒绝会让每一份超大结果变成死路。凭据在 `config.toml`、各项目转录在
`sessions/`,两者继续 sensitive。

**连带发现(既有洞,非本 plan 引入)**:后台 bash 的输出文件 `offload/bg-N.out`,kloop 把路径直接给了
模型,但模型此前同样读不了它。这条豁免一并修好。

## 接受的后果## 接受的后果

- **受限子 agent 若白名单不含任何文件读取或 bash,将读不回自己的大输出。**这是上面第 4 条的直接
  结果,也更自洽:工件是文件,读不了文件就读不了它。会产生大结果的 agent_type 应当带上其中一个。
- **`OFFLOAD_CAP_CHARS` 的文档理由换了担保人**:原文是"`read_offloaded` 必须严格小于它"。
  现在承担这个不变量的是 `read_file` 的 `READ_CONTENT_CHARS`(30000 < 32000),非程序路径的回复
  仍不会被二次 offload。

## 验证

- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace`
- 回归:指针含路径与字符数、含 `run_program`、**不含 `read_offloaded` 也不含 `id=off-`**;
  沙箱策略里 offload 目录进 `allowed_read_paths` 且不进 `denied_write_paths`;
  allowlist 不再豁免文件读取,仍豁免协调工具。
- **老会话 resume**:本机今天的会话历史里记着 `read_offloaded` 的 tool_use。删掉工具名之后
  `--resume` 是否 fail closed,必须实测。
- 真实端到端(`--headless`,不点名工具):重跑 plan 110 的两个问题。

## 完成

✅ 2026-09-03，提交 SHA 以本条所在提交为准。
