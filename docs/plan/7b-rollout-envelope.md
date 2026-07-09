# Plan 7b — rollout 底子加固(信封 / 双向修补 / 坏尾物理截断)✅ 已完成(2026-07-09)

> 历史记录。plan 7 复盘(对照 cc)后按"基础模块底子做实且最优"的标准补齐。磁盘 schema 是最难改的部分,趁开发期无存量文件一次到位;性能工程(cc 的 chunked 读/50MB 上限)不属于底子,格式不变可后补。

## 目标

1. **行信封**:每行加 `id` / `parent` / `ts`(unix 毫秒)。cc 的 fork/rewind/审计全部长在 uuid/parentUuid 上;现在把地基字段落上,重放逻辑保持线性(线性是树的特例),plan 9 TUI 做 rewind 时不用迁移格式。
   - id 不引 rand:`{文件名 stem}#{单调序号}`,文件内唯一、可复现、好测试。
   - 恢复后续链:新行的 parent 接文件里最后一行的 id,序号接最大序号 +1。
   - 前向兼容锁死:未知字段必须被忽略(serde 默认行为,测试锁住)。
2. **反向孤儿修补**:恢复时删掉引用不存在 tool_use 的孤儿 tool_result 块(cc `ensureToolResultPairing` 是双向的);先反向删、再正向补,块删空的消息整条丢弃。
3. **坏尾物理截断**(plan 7 潜在 bug):load 只做了逻辑截断,半行字节还在文件里,恢复后追加会与半行拼接、之后的行全部不可达。恢复(写入路径)时把文件物理截断到最后一个完整合法行;只读路径(--list-sessions)不动文件。

## 归属

全部在 `crates/core/src/rollout.rs`(+ history/cli 适配签名)。破格式:旧 demo 会话文件不做迁移,直接作废。

## 测试

- 信封:id 连号、parent 成链、ts 非零(直接解析文件行断言)。
- 恢复续链:resume 后追加,新行 parent == 旧末行 id,序号不撞。
- 坏尾:半行 → resume 物理截断 → 再追加 → 重读两条都在。
- 反向修补:孤儿 tool_result 被删;和正向补齐同时发生的混合场景。
- 前向兼容:带未知字段的行照常解析。
- 既有 65 个测试全绿。

## 完成标准

fmt/clippy/test 全绿;--mock → --resume 实跑;README 同步;HANDOFF 补教训(追加型文件的坏尾必须物理截断)。

## 结果

提交 `d0c1785`:

- 行格式:`{"type":"message","id":"{stem}#{seq}","parent":"...","ts":毫秒,...消息字段}`;LineMeta 双 flatten 进 tagged enum;链状态在写入成功后才推进。恢复用新入口 `resume_session`(替换"load_session + Rollout::new"组合),返回续链的 Rollout;`load_session` 保留为只读(--list-sessions),绝不动文件。
- 双向修补:先反向删孤儿 tool_result(删空整条丢弃),再正向补 interrupted。
- 坏尾:`parse_session` 记录最后完整行的字节偏移,`resume_session` 发现尾部有残余就 `set_len` 物理截断;无结尾换行的行一律视为撕裂写入不信任。
- 测试 65 → 71;实跑:全新 --mock → --mock --resume,信封链跨重启相连、id 无碰撞;手工注入撕裂尾行 → resume 截断 → 再追加,30 行全部干净解析。旧格式 demo 文件直接作废,未做迁移。
