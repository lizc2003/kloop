# kloop — 全局记忆

从零做一个新的 Rust agent。详细交接(当前状态、能力清单、实现教训、进度)读 `docs/plan/HANDOFF.md`。

- **kloop 是唯一开发对象**。`refs/codex`、`~/work/claude-code`、`refs/claw-code` 仅供参考,绝不在其上开发或推送。开发阶段直接在 main 分支,不开 feature 分支。
- **工作方式**:`docs/plan/` 一个编号文件 = 一个会话任务(✅ 为已完成历史)。开工前读 `docs/plan/HANDOFF.md` + 对应 plan 文件,plan 里标"开工时定/问用户"的点先问清。
- **完成标准**:cargo fmt + clippy + test 全绿,一次 commit(写清验证方式);行为变更同步 README;plan 文件补 ✅ 与提交号;新教训写进 HANDOFF.md。
- **验证**:`cd kloop && cargo test`(1 秒,无网络无 key);`cargo run -p kloop -- --mock`。真实 API 的代理和 key 问用户要,勿写进任何提交文件。
- **风格**:format! 内联变量;match 尽量穷尽;避免 bool 位置参数(必要时 /*param*/ 注释);测试整对象断言优先;新增行为必须带测试;注释只写代码看不出来的约束。
- **用户偏好**:中文交流,短句增量澄清,别用多选项问卷,一次问一个点。
