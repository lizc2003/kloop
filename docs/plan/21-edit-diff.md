# Plan 21 — Edit/Write 的 diff 呈现 ✅(commit c86882a)

> ✅ 已完成。开工前读 HANDOFF。参考:cc 的编辑审批带 diff 显示。

## 目标

文件写/改的审批与转录里**看得见改了什么**(unified diff),而不只是路径。这是编码
agent 的信任核心——批准一个看不见的改动是没意义的。

## 现状(gap)

- 权限 `describe()` 对 `write_file`/`edit_file` 只显示路径(`edit_file: <path>`);
- TUI 工具行折叠成 `✓ edit_file {...}`;审批弹层只有路径;
- 用户批准/回看时都看不到具体改动。

## 关键决定(开工时定)

- **diff 生成**:`edit_file` 有 old_string/new_string,直接成 diff;`write_file` 是整
  文件覆盖——对已存在文件读旧内容生成旧→新 diff,新建文件显示"新建 + 内容预览"。
  用 `similar` crate(轻、纯 Rust、成熟)还是最小手写 line diff。倾向 similar。
  (注意:引第一个非 core 已有的依赖,权衡;或只在 cli/tui 侧用。)
- **在哪呈现**(按价值):① 审批弹层 y/n 前看 diff——**最有价值**;② 转录工具行可展开;
  ③ server 通知带 diff。最小版先做审批弹层。
- **接口缝(主要决定)**:审批走 `ConfirmRequest`(现只有 description/remember_rules)。
  加一个可选 `preview: Option<String>`(diff 文本),TUI 渲染、plain 打印。这是把 diff
  送到审批 UI 的通道。谁生成 diff:工具执行前(权限门里)还是前端?倾向权限门/工具层
  生成塞进 ConfirmRequest(前端只渲染)。
- **截断**:大 diff 截断(前 N 行 + "省略 M 行")。

## 不做

交互式逐 hunk 批准(cc 也不做);语法高亮(先纯文本 +/- 行);二进制文件 diff。

## 测试

edit diff 生成(增/删/改行)、write 对已存在文件与新建文件、大 diff 截断文案、
`ConfirmRequest.preview` 契约、TUI/plain 渲染 diff。

## 完成标准

fmt/clippy/test 绿;真 key 一次编辑任务审批时看到 diff;README、HANDOFF。

## 完成记录

**决定落地**(与关键决定对照):
- **diff 生成**:用 `similar` crate(workspace + core 各加一行,首个为功能引入的新依赖;
  纯 Rust、无重传递依赖,`similar 2.7`)。**回源核对后**(见下"回源修正")定为 cc/codex
  收敛形态:`edit_file` **读文件、应用编辑、整文件 diff 带真实行号**(不是直 diff 两串);
  `write_file` 已存在文件读旧内容 diff 旧→新,不存在/读不到则 `(new file)` 逐行 `+`
  (空内容标 `(empty)`)。读不到/文件 >1MiB/`old_string` 非唯一匹配 → 退化直 diff 两串
  (行号从 1,cc 同款分级)。每行 `{+/-/空格}{行号}  {内容}`,3 行上下文,非相邻 hunk 用 `⋮`。
- **谁生成**:权限门里(`crate::diff::file_change_preview` 在 `ask_user` 里 await),
  前端只渲染——依 plan「倾向权限门/工具层生成」。读盘用**原始 path**(与
  `write_file_tool`/`edit_file_tool` 落盘路径一致,不经 cwd 归一化),故与实际会被覆盖的
  文件严格对齐;应用编辑复刻 `edit_file_tool` 语义(replacen / replace_all)。
- **接口缝**:`ConfirmRequest` 加 `preview: Option<String>`;非 edit/write 一律 `None`
  (bash、敏感路径以外的、escalation 都 None——但 edit/write 落敏感路径/ask 规则时
  **仍带 diff**,因为 preview 只看工具名不看触发原因)。只在真要问人时才生成(no-approver
  提前返回,不浪费读盘);no-op 编辑(old==new)→ 空 diff → `None`,前端不渲染空框。
- **截断**(过渡,弹层不可滚动的妥协——见 plan 25):单行 >200 字符裁剪加 `…`(防
  minified 撑爆);总行数 >40 截断 + `… (N more line(s))`。弹层做了滚动后 core 侧行数
  截断应放宽(plan 25)。
- **三前端呈现**:审批弹层(①,最有价值)= TUI `draw_confirm` 在 description 与选项间插
  彩色 diff(`diff_preview_lines`:绿 `+` / 红 `-` / 暗 上下文/行号);plain REPL
  `color_diff` 同款 ANSI;server `approval/request` 加 `preview` 字段(③)。**②转录工具行
  可展开未做**(需 diff 在 tool_end 时可得,是更大改动;记为后续)。

**回源修正**(用户问"参考那3个项目了吗"后补做——初版跳过了回源,教训 11):
交叉核对 cc(TS)/ codex-rs / claw-code 的编辑审批 diff。发现两个**收敛漏点**(cc 与
codex 独立都做、初版没做,教训 14):① 真实行号;② edit 读文件应用编辑再整文件 diff
(而非直 diff 两串)——两者关联,补 ② 才能有 ①。已补齐。cc 的"文件读不到/超大退化直
diff 两串"分级也照抄为兜底。**可不抄的分歧**(核对确认):intraline 词级高亮(仅 cc)、
语法高亮(nice-to-have)、`⋮` vs cc 的 `...`(kloop 用 `⋮` 与 codex 一致)。**claw-code
的 diff 是反面教材**:全 `-`+全 `+` 朴素拼接、行号退化为 1、审批处根本不渲染。核对
结论:初版最小实现没踩坑、`similar`/3 行上下文/省 `@@` 头/着色都在收敛区间内,只是漏了
行号+读文件这对收敛点,已补。长行截断(vs 两家换行)、40 行硬截断是 kloop 因弹层不可
滚动的自创妥协 → 单开 **plan 25**(审批弹层滚动 + 届时放宽截断)。

**测试**(+9,总 317):diff.rs 6 个(**带行号**增/删/改、远距 hunk `⋮` 分隔且中段上下文
不全展、长行裁剪至 200、大 diff >40 截断文案、write 已存在 vs 新建 vs 空文件、**edit 读文件
应用编辑 vs 读不到退化直 diff 两串** + no-op/非文件返 None);permissions
`ConfirmRequest.preview` 契约(edit 退化两串带行号、write 新建 new file、bash 不带);tui
`diff_preview_lines` 按符号着色;cli `color_diff` ANSI;server 集成 `approval_denied_then_allowed`
断言 `preview == "(new file)\n+1  x"`(端到端:Mock 模型真调 write_file 走真权限门 + 真 approver)。

**真 key 验收**(Anthropic / sonnet-5,单轨——diff 生成在 core 权限门、与 provider 无关):
`--plain` 下让模型把 main.rs 里变量 x 改 count(let + println 两处),模型自发先 `cat` 再
`edit_file`,审批弹层显示 **带真实行号 + 周围上下文** 的整文件 diff(暗 ` 1  fn main() {`、
红 `-2/-3`、绿 `+2/+3`、暗 ` 4  }`),EOF→deny 文件未改。write_file 两路由单测 + server 集成端到端覆盖。

**未做**(记为可能性):②转录工具行内联/可展开 diff;审批弹层滚动(plan 25);intraline
词级高亮 / 语法高亮;逐 hunk 批准(cc 也不做);二进制文件 diff。

commit: c86882a(feat 初版)+ 903b870(回源修正:行号 + edit 读文件整文件 diff)。
