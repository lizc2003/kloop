# 222 — 资源 ID 跨进程防重

用户确认：使用 64 位随机数和 Base58，不考虑旧格式兼容。界面、工具参数、文件名
使用同一个 ID，不维护短编号与文件 ID 两套身份。
后续确认：项目、工作区与 scheduler 的确定性哈希取 SHA-256 前 128 位，使用小写 Base36。

## 范围

- offload、后台 Bash、子 agent、Program / Workflow 的执行 ID 和持久 run ID 统一随机生成。
- 文件与运行目录通过排他创建占位；碰撞重试，不扫描目录恢复编号。
- 子 agent 日志拒绝复用已有文件；自动 worktree 名加入随机身份，明确命名仍拒绝重名。
- 定时任务 ID 同样使用随机身份，查重与插入放进同一个事务。
- 同一已有会话的写入持有独占锁；OAuth、项目标签和 Git exclude 的共享更新保护读改写。
- scheduler 项目键与 projects 的项目/工作区身份取 SHA-256 前 128 位，使用固定 25 位全小写 Base36；`p1_` / `w1_` 加前缀共 28 字符，无旧目录查找或迁移。
- 主会话的时间戳名称保留：现有排他占位与后缀重试已经正确。
- 输入 ID 按不透明的安全路径组件解析，不再解释数字序号；不添加旧序号恢复或迁移分支。

## 验证

- ✅ `make check` 全绿：fmt、clippy、workspace 测试、release 核心测试与 parity。
- 编码覆盖 64 / 128 位边界与高低位，验证只取摘要前 128 位；哈希身份拒绝大写、错误长度和超出 128 位的值。
- 强制文件与 run 目录撞名，验证重试且不改已有内容；子进程验证写租约排他与释放。
- 活跃会话的 inspect 与 Server read 保持只读，第二个 writer 被拒绝；关闭原 writer 后恢复成功。
- Program / Workflow 恢复与重放、共享 offload、子 agent 既有日志保护继续通过。
- OAuth 并发保存保留所有条目；首次保存先建立私有目录，再加锁读取。
- DESIGN 已改写现有契约，HANDOFF 已记录跨进程身份、大小写别名与共享更新的教训。
- ✅ 复核（审查 `45874b5^..64d4ee8`）：scheduler 的 jitter 按 id 前 8 位十六进制解码，新 `job-` + Base58 id 恒解析失败而静默退化为 0；改为对完整 id 取 SHA-256 摘要，撞名重取 id 时同步重算 `next_fire_at_ms`，jitter 测试改用真实 id 形状并断言前缀相同的两个 id 分数不同。`bash_id` schema 示例与 DESIGN 的 sandbox 豁免粒度一并更正。
- ✅ 复核收尾：「id 不透明」从注释升级为 `resource_id` 的模块约定，派生值 `fraction` 与 id 生成同址（前缀相同的 id 必须给出不同分数，零分数即为当初的失效形态）；会话写租约进入 API 契约——`SessionRead::recover` 更名 `recover_for_writing`，`resume_session` 写明取写租约、被占用即失败，CLI 的失败文案随之从「读不了文件」改为 `cannot resume session`。

状态：✅ 已完成（提交 SHA 以本文件所在提交为准）。
