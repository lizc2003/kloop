# Plan 13 — 项目上下文注入(system prompt 与项目指令文件)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 CLAUDE.md 语义(自动加载、分层、@import)、codex 的 AGENTS.md(project_doc 机制)。教训 11:两边的具体语义都回源核对,别凭印象。

## 目标

kloop 的 system prompt 现在是 cli 里一行硬编码。这个 plan 把它变成组装产物:基础指令 + 环境块(cwd、平台、日期、是否 git 仓库)+ 项目指令文件(AGENTS.md / CLAUDE.md 之类),让 kloop 在真实项目里"知道自己在哪、这个仓库的规矩是什么"。

## 设计要点

- **组装归属**:纯函数放 core(可测),文件读取和拼装由 cli 做(core 保持无 IO 的边界?rollout 已经破例——开工时定归属)。
- **指令文件名与优先级**(开工时问用户):只认 AGENTS.md?兼容 CLAUDE.md?还是 kloop 自己的名字?找多个时的合并/覆盖规则。
- **分层**:仓库根 + 全局(`~/.kloop/`?)两层先做;cwd 向上查找到 git 根为止。子目录级懒加载(cc 形态)本 plan 不做,记为可能性。
- **大小上限**:指令文件超限截断 + 告警(具体阈值开工时定);system 不走 offload(那是 tool result 的机制)。
- **环境块**:cwd、平台、日期、git 仓库与否。git status/最近提交快照要不要进去(token 成本 vs 价值)开工时定。
- **子 agent**:继承同一 system 还是精简版,开工时定(cc 的子 agent 是独立 prompt)。
- `--mock` 保持 hermetic:不读任何文件,用现在的硬编码。
- server 模式:各 thread 同进程 cwd,同一份组装结果,不做 per-thread。

## 测试

组装纯函数的快照/整对象断言(有无指令文件、有无 git、超限截断);缺文件不报错;`--mock` 不读文件(hermetic 锁死)。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收:项目里放一条可观察的规则(如"所有回复以某词开头"),确认模型遵守;README、HANDOFF 更新。
