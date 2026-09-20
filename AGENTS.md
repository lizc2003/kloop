# kloop — 全局记忆

从零做一个新的 Rust agent。详细交接(当前状态、能力清单、实现教训、进度)读 `docs/plan/HANDOFF.md`。

- **kloop 是主开发对象**。参考项目都在 `refs/` 下、只读:`refs/codex`(上游 openai/codex,引擎 codex-rs)、`refs/claude-code`(逆向 TS 版)、`refs/codewhale`,仅供参考,绝不在其上开发或推送(`refs/*` 已被根 `.gitignore` 排除;`refs/claude-code-2.1.220` 例外,那是 kloop 自己的 parity fixtures,不是克隆);`refs/claw-code` 已退休为固定 commit 的历史调研来源,结论见 `refs/README.md`,不要假设本地克隆仍存在。kloop 自身开发直接在 main 分支,不开 feature 分支。
- **工作方式**:`docs/plan/` 一个编号文件 = 一个会话任务(✅ 为已完成历史)。开工前读 `docs/plan/HANDOFF.md` + 对应 plan 文件,plan 里标"开工时定/问用户"的点先问清。
- **完成标准(只适用于本次实际改了仓库文件的任务)**:cargo fmt + clippy + test 全绿,一次 commit(写清验证方式);行为变更同步 README;plan 文件补 ✅ 与提交号;新教训写进 HANDOFF.md。**只读任务**(审查、调研、解释代码、回答问题)不适用:不检出 worktree、不编译、不跑测试,除非用户明确要求验证;结论的证据来自读代码,不要用跑一遍全量测试来代替判断。
- **验证**:仓库根有 Makefile,门禁与 CI 同令——`make test`(= `cd rust && cargo test`,1 秒,无网络无 key)、`make check`(fmt + clippy + test,提交前跑这一条)、`make mock`。真实 API 的代理和 key 问用户要,勿写进任何提交文件。
- **风格**:format! 内联变量;match 尽量穷尽;避免 bool 位置参数(必要时 /*param*/ 注释);测试整对象断言优先;新增行为必须带测试;注释只写代码看不出来的约束。
- **用户偏好**:中文交流,短句增量澄清,别用多选项问卷,一次问一个点。
