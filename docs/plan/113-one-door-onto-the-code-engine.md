# Plan 113 — QuickJS 引擎只留一道模型面的门（`run_program` 默认下架）

> 承接 plan 112 的只读调研。本 plan **不删任何实现**：引擎、`CoreBridge`、journal、MCP 结构化
> 结果全部保留，只把 `run_program` 从**发给模型的工具数组**里撤下，并做成可配置回退的实验。

## 判断依据

plan 112 摆出的事实里，决定性的是这一条:

| | code runtime 的地位 | 实测结果 |
|---|---|---|
| **codex** | 唯一工具 | 每次都用 |
| **cc** | 根本没有,`Bash` 顶上 | 不存在这个问题 |
| **kloop** | 十六个工具里的一个 | 引导之前 45 个会话用 2 次;同期真实工作会话 100 次 bash、0 次 run_program |

两个参考各自自洽,kloop 卡在中间:**一个永远不是局部最优的第三选项**。要读文件 `read_file` 更便宜,
要抽字段 `bash` + `python3 -c` 更便宜(plan 111 成绩最好的那次 2 轮就是它,也正是 cc 的做法)。

**"合并 run_program 与 workflow"解决不了这个。** 合并后仍是十六选一里的一个、仍要每步考虑。
它只处理了"重复"这个整洁性问题,没处理"没人用"这个实际问题。

`workflow` 留下的理由不同:它**不和 bash 竞争**(agent 编排 bash 表达不了),它的 0 用量是**策略
造成的**(描述写死"只在用户明确要求时用" + `startup.rs:1004` 的 `workflow: !args.headless`),
不是竞争失败。

## 改动

1. **`SurfaceCapabilities` 新增 `program: bool`**,与既有的 `workflow`/`worktree`/`scheduler`
   同型。`all_tool_defs` 和 `tool_defs` 都在这个开关后面 push `run_program_def`。
   **CLI 默认 `false`。**
   —— 用开关而不是注释掉那一行,有三个理由:`run_program_def` 保住调用点、不产生死代码;
   实验两个分支都可测;回退是配置翻转而不是 revert。

2. **`stop_program` 随同一开关下架**。`run_program` 不可调时它无对象可停。

3. **`tool_defs` 的计数语义跟着变。** 它原本恒含 `run_program`(注释:"仅计数用、从不发模型"),
   为的是让 defer 阈值的计数与 `all_tool_defs` 对齐。现在两边都按开关走,计数继续如实反映
   **真正发出去的工具数**——开关关时总数少 2(`run_program` + `stop_program`)。

4. **互相引用的描述改写**:`run_agent`、`workflow`、`web_fetch`(plan 110 加的那句)、
   `history.rs` 的 offload 指针(plan 111 那句)——都不再点名 `run_program`。指路统一改为
   **bash 就地抽取**,这是 plan 111 实测最好的形态,也是 cc 的形态。

## 不做

- **不删任何实现**。`kloop-codemode`、`CoreBridge`(带完整 gate 回灌)、journal v3、
  plan 27 的 MCP 结构化结果、`RunStore` 的 Program namespace 一行不动;dispatch 分支保留,
  开关一开即恢复。
- **不给 `workflow` 加 `tools` 对象**。那是"将来若真需要混合编排"的动作,应当在有真实需求时
  单独做,并且天然落在 workflow 已有的"用户明确授权"门后面——比现在这个随手可调的形态更该有的
  样子。本 plan 不预先造它。
- **不动 `run_agent`**(单个委派,有 cc 对应物)。

## 风险与回退

- 若模型在真实任务里**不**回落到 bash、而是变得更笨,把 `program` 默认改回 `true` 即可,
  没有数据迁移、没有协议面变化。
- server/原生协议面不受影响:工具数组本就按会话构造。

## 验证

- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace`
- 回归:开关关时工具数组不含 `run_program`/`stop_program` 且计数相应减少;开关开时逐字恢复
  原有形态(两个分支都测);四处描述不再点名 `run_program`。
- **真实验收(本 plan 的重点)**:用户当下正在做的那类真实审查任务,看模型是否自然回落到
  `bash`,以及是否出现"本该编排却没得用"的场景。单元测试证明不了这个。

## 实测结果（不点名工具，均为默认即 `program: false`）

| 题目 | plan 111/112 时（有 run_program） | 本 plan（无 run_program） |
|---|---|---|
| schema required | 2 轮（bash）/ 3 轮（run_program） | **2 轮**，`web_fetch` → `bash python3` |
| add-on 百分比 | 3 轮（run_program + tools.bash） | **5 轮**，`web_fetch` → 4 次 `bash python3` 迭代 |

**回落如预测发生**：两题都自然走到 `bash` + `python3 -c`，都答对，没有出现"本该编排却没得用"的场景。

**但第二题变差了：5 轮 vs 3 轮。** 多出的两轮花在反复调整那段 python(猜字段路径)。诚实地说这不是
干净的胜利:第一题追平历史最好成绩,第二题退了两轮。两题各 n=1,不足以判断是系统性的还是噪声——
但它确实说明 `run_program` 在"需要多次试错的抽取"上可能有真实价值(JS 里改一行比重写一段 heredoc
便宜)。如果后续真实使用里这个形状反复出现,把 `program` 翻回 `true` 是一行配置。

## 完成

✅ 2026-09-03，提交 SHA 以本条所在提交为准。
