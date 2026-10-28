# Plan 248 — 共享的文本、路径与文件 helper

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量。

## 背景

同一个小功能在多个 crate、多个模块里各写一份，有的已经开始分叉。这一条把它们收到
少数几个共享位置。**纪律照 plan 177–186:行为零变更**;下面标"行为修正"的两处单独
提交、单独带测试。

## 清单

1. **按字符截断**,十多份：
   - 共享版 `core/src/tools/mod.rs:1832` `char_prefix`;
   - 复刻：`tools/notebook.rs:620` `char_prefix_owned`、`history.rs:936` `clipped`、
     `text_edit/closest.rs:448` `quoted`、`web/src/limits.rs:21` `truncate_chars`;
   - "先数字符再 `take` 拼后缀":`tools/subagent.rs:1065`、`tools/codemode.rs:543`、
     `tools/workflow.rs:373`(前两份逐字相同)、`sandbox/mod.rs:718`、`diff.rs:223`、
     `structured_output.rs:80`、`provider/src/lib.rs:267`。
   → core 提供 `clip_chars(text, max, suffix) -> Cow<str>`(web/provider 也用的话放
   protocol),用 `char_indices().nth` 一次定位。
2. **自写的 UTF-8 字节边界截断**:`core/src/context.rs:296` `truncate_at_char_boundary`、
   `tools/inject.rs:266` `floor_char_boundary`(注释写着 stable stand-in)。工具链是
   1.98.1,开工先核实 `str::floor_char_boundary` 的稳定版本，已稳定就两处换成标准库并删掉
   自写版本与测试。
3. **原子替换文件**,五六份：`session_store.rs:261` 与 `worktree.rs:861` 逐行相同(都用
   `resource_id::create_file`);`scheduler/store.rs:213`(多一步目录 fsync)、
   `tools/run_store.rs:293`、`cli/src/private_store/*` 的 `write_atomic` 有更强的保证。
   → 在 `resource_id` 旁加 `replace_atomically(dir, prefix, target, bytes, sync_dir)`,
   前两份先合并;其余按需迁移，不强求。
   - **行为修正**:worktree 那份经 `create_file`(0600)把用户仓库的 `.git/info/exclude`
     换成 0600 文件，原权限(通常 0644)丢失。共享函数写入前沿用目标已有的权限位。
4. **不访问文件系统的路径规范化**,三份等价：`cli/src/context.rs:255`、
   `permissions/facts.rs:680`、`file_state.rs:708`。保留 `file_state` 那份并公开，另两处
   改调。(带 canonicalize 的"最近存在祖先"那组在 plan 245 处理，这里不碰。)
5. **`~` 展开与显示缩写**:展开在 `cli/src/context.rs:237`、`permissions/facts.rs:562`
   (还处理单独的 `~`)、`sandbox/mod.rs:371`;显示缩写在 `tui/src/lib.rs:347`,用
   `env::var_os("HOME")`,其它地方用 `std::env::home_dir()`。
   → `paths::expand_tilde(raw, home)`、`paths::display_with_tilde(path)`。
   - **行为修正**:TUI 改用 `home_dir()`,Windows(`USERPROFILE`)与 `HOME` 被改过时
     与其余地方一致。
6. **摘要转十六进制**,六份：`cli/src/mcp_auth.rs:165`、`provider/src/tool_id.rs:80`、
   `provider/src/lib.rs:405`、`execution_provenance.rs:203`、`tools/notebook.rs:600`、
   `server/src/events.rs:522`。除 server 外都是每字节一次 `format!`。→ protocol 加
   `hex_lower(bytes)`。
7. **其它小重复**:
   - skills 描述截断：`skills.rs:191` 与 `commands/skills.rs:26`(注释明说"跟另一处一样");
   - `rollout.rs:421` `id_prefix` 与 `rollout/sessions.rs:873` `session_id_of`;
   - `inbox.rs:77` `ScheduledOrigin` 与 `event.rs:116` `ScheduledTaskOrigin` 同构，外加
     `agent.rs:1458-1463` 的转换与 `inbox.rs:246-249`、`event.rs:325-328` 两份
     `"cron"/"loop wakeup"` 字符串 → 复用一个枚举并给它 `label()`;
   - `history.rs:936` `clipped` 改调 `char_prefix`。

## 验收

- 每个合并点保留原有测试(搬到共享函数旁边);两处行为修正各一条先红后绿的测试
  (exclude 文件权限保持 0644;TUI 路径缩写用 `home_dir()`)。
- 搬运提交的 diff 除调用点外接近零改动;外层执行沙箱之外 `make check`;HANDOFF 记进展。
