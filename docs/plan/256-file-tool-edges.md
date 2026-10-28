# Plan 256 — 文件工具边界

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

1. **notebook 改 `cell_type` 不同步该类型要求的字段**(待复核)。`core/src/tools/notebook.rs:291-317`:
   code → markdown 留下 `execution_count`、`outputs`(markdown 不允许);markdown → code 缺
   `outputs`/`execution_count`,原 `attachments` 还在。nbformat validate 报错。
2. **Unix 新建文件有覆盖窗口**(待复核)。`tools/fs/unix.rs:186-195` `commit_rename(..,
   _replace_existing)` 只调 `renameat`;新建时 `verify_target_unchanged` 看到 (None, None) 放行，
   随后的 `verify_parent_binding`、`verify_temp_binding`(要整份重读 temp)期间若有进程在 leaf
   建了文件，会被直接覆盖。Windows 传 `replace_existing=false` 会拒绝。HANDOFF 写的是
   "leaf race 不覆盖"。
3. **rename 成功后 `sync_parent` 失败被当成提交失败**(疑似)。`fs/unix.rs:204-208`:目录 fsync 返回
   EINVAL/ENOSYS 的文件系统(部分 FUSE/网络盘)上，每次 write/edit 都已落盘却报
   "cannot sync parent directory";随后 `restore_cleared`(`fs.rs:742-746`)放回旧 observation,
   模型以为没改成会重试，edit_file 报 old_string not found。Windows 同名函数忽略错误。
4. **含超过 3 万字符长行的文件永远拿不到 write_file 资格**(待复核)。`fs.rs:539-545`、`563` +
   `file_state.rs:636-662`:超长行只给截断前缀并设 `partial_line`,`observed_end` 停在行首，
   `is_complete()` 恒为 false。minified JS、单行 JSON、SVG 都会碰到，模型只能绕去 bash。
5. **覆盖已有文件时预览显示 "(new file)"**(待复核)。`core/src/diff.rs:83-92`:目标不是 UTF-8
   (读过一张 PNG 后 write_file 同一路径)或 `read_bounded` 返回非超限错误(EACCES、Changed),
   都落到 `new_file_preview`,审批界面误导用户。
6. **grep 把"多硬链接被跳过"计入 deny/敏感规则隐藏**(待复核)。`tools/search.rs:247-253`:
   pnpm/`cp -al` 目录里提示 "[N paths hidden by deny/sensitive rules]";EACCES 等其他错误静默
   `continue`,不计数。
7. **回退匹配层内存放大约 35 倍**(优化)。`core/src/text_edit.rs:82`、`163-164`、`284-286`、`375`、
   `380-383`:对 5 MiB 文件 not found 时，`logical_lf_view` 分配 (len+1) 个 usize,`fold` 按字节数
   给 `Vec<char>` 与 `Vec<Span>` 预分配，瞬时峰值约 185 MiB;`closest_match` 在检查 10000 行上限
   之前先整份 `normalize_newlines`。
8. **grep 每个文件都走完整 `prepare_read_target`**(疑似，先量)。`search.rs:243-268`:每文件两次
   canonicalize、两次 open parent、两次 openat;content/count 模式还多取一次不用的 `modified`。
9. **notebook 可见文本上限 7,000,read_file 已是 30,000**(观察)。`MAX_NOTEBOOK_TEXT_CHARS`:
   超过 7k 的 notebook 读出来 `editable=false`,notebook_edit 永远用不了。

## 开工先问用户

- 第 4 条：(a) 读到截断行时把"已看到前缀"算作这一行已覆盖(写入前的 stale 检查仍按全文
  指纹);(b) 不放宽，只在拒绝消息里明说这个文件 write_file 拿不到资格、指向 edit_file。
  推荐 (a)。
- 第 9 条 7k 是有意保留的吗?不是的话对齐到 30k。
- 第 2 条三平台做法：Linux `renameat2(RENAME_NOREPLACE)`、macOS `renameatx_np(RENAME_EXCL)`,
  都不支持时退回 `linkat(temp→leaf)`(EEXIST 即失败)再 unlink temp。rustix 是否已暴露这些
  调用开工先查;没有的话问用户能否直接用 libc。

## 做法

1. 类型变化时按目标类型删/补字段(改成 markdown 删 `outputs`/`execution_count`;改成 code
   补齐并去掉 `attachments`)。
2. `replace_existing=false` 时走非覆盖 rename(按问答)。
3. rename 之后的 sync 失败降级成结果文本里的提示(durability 未确认),不当提交失败。
4. 按问答处理长行。
5. 预览：非 UTF-8 显示 "(overwriting existing binary file, N bytes)";只有 NotFound 才是
   "(new file)";其他错误显示 "(existing file could not be previewed)"。
6. 硬链接单独计数与文案;其他打开错误计 "N files unreadable"。
7. boundaries 只记 CRLF 位置(或用 u32);`fold` 改惰性或按 char 数估容量;`closest_match` 先数
   行再规范化。
8. 先在大仓库量一次 grep 耗时，确认有必要再按目录缓存已校验的 parent fd;`modified` 只在
   FilesWithMatches 取。

## 验收

- 单测(旧代码先红):两种 cell_type 转换后的字段集合;Unix 新建时 leaf 已被抢先创建则失败
  不覆盖(测试钩子注入竞争);sync_parent 失败(测试钩子)时结果为成功 + 提示、observation 更新;
  长行文件读完后可 write;二进制目标的预览文案;硬链接与 EACCES 的计数文案;5 MiB 文件 not found
  的峰值分配(用计数分配器或只断言不 OOM 的小规模比例)。
- DESIGN 的文件工具一节先读再改;外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。

## 刻意设计，不改(审查时核过，记一笔免得后来者重查)

- `read_file` 传 `offset=0` 时行号从 0 开始：测试明确断言、plan 61 写了"维持 offset=0"。
- CRLF 文件里单行 old_string 换成多行 new_string 时精确层原样插入 LF,结果是混合换行：测试
  `raw_exact_match_wins_and_preserves_raw_replacement` 断言的正是这个。模型看到的永远是 LF,
  这恰是最常见的插入方式，值得以后重新评估，但不在本条做。
