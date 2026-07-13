# Plan 21 — Edit/Write 的 diff 呈现(备忘)

> 备忘,未开工。开工前读 HANDOFF。参考:cc 的编辑审批带 diff 显示。

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
