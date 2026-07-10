# Plan 13 — 项目上下文注入(system prompt 与项目指令文件)✅

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 CLAUDE.md 语义(自动加载、分层、@import)、codex 的 AGENTS.md(project_doc 机制)。教训 11:两边的具体语义都回源核对,别凭印象。

## 完成记录(2026-07-10)

**拍板**(问用户两点,其余开工时定):
- 文件名:AGENTS.md 为主 + 兼容 CLAUDE.md,同目录都在只取 AGENTS.md;不发明 kloop 自己的名字。
- git 快照:进 system(cc 形:branch + status --short 截 1000 字节 + 最近 5 提交,标注"开场快照不更新");codex 不注入,弃其形。
- **注入位置改了 plan 原案**:回源发现 cc/codex 都不把项目指令放 system,而是首条 user 消息。跟随两家——指令走每次采样请求的合成首条 user 消息(`<project-instructions>` 包裹),**不进 History/rollout**,resume 自动拿新内容、压缩吃不掉;predictive 记账单独加上这块估算。环境块 + git 快照进 system 尾部(cc 形)。
- 归属:纯组装 core/src/context.rs(无 IO,整对象断言),文件发现/git 命令 cli/src/context.rs;core 无 IO 边界守住了。
- 分层:全局 ~/.kloop/ + 项目层(cwd 向上到 git 根,根→cwd 顺序);无 git 根只看 cwd。上限抄 codex:总预算 32KiB,截断+启动告警。
- 子 agent 走 `(*cfg).clone()` 天然继承,零改动;server 各 thread 共享一次组装;--mock 保持 hermetic(不读文件不跑 git,沿用原硬编码 system)。

**实现**:`Config.project_instructions: Option<String>`;注入缝在 `sample_with_retry`(压缩请求天然不带);provider Mock 加 `mock_recording`(请求记录器)供测试断言请求形态。测试 159→178:core 组装纯函数(有无 git、截断边界含多字节、预算耗尽/恰好)、agent 注入契约(每请求首条 + 不进 History + predictive 记账 + 压缩请求无注入)、cli 发现(AGENTS>CLAUDE、根→cwd 顺序、git 根外忽略、无根只看 cwd、全局层在前、缺文件不报错)。

**验收**:fmt/clippy/test 全绿;--mock 全程无恙;真 key 双轨(anthropic + openai-compat gpt-5.4-mini)在临时项目验证:AGENTS.md 规则(回复以 BANANA 开头)遵守,日期/分支/cwd 不调工具直接答对(环境块与 git 快照生效)。提交 eb8a1b4。

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
