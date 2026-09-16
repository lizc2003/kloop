# Plan 156 — 三件小事,各值一次提交

> 来源:2026-09-15/16,一轮工具层通读(plan 154、155 同源)里攒下的零碎。用户拍板
> "立 plan,把一些细碎的改进放进去"。

## 〇、这个 plan 和别的不一样

**它是一个池子,不是一个任务。** 三件事互不相干、各自能单独提交、**做一件也算数**。
接手的人可以只挑一件做完就收工,在下面对应小节补 ✅ 和提交号,不必等三件齐。
验收按件算,没有整体验收。

三件都很小(各自估计几十到一两百行),共同点是:不需要任何架构决定、不推翻任何已有裁决、
不引入新依赖。

---

## 一、`bash` 少一个 `description`

### 现状

对着 parity 基线比 `bash` 的 schema(`core/src/tools/builtin.rs:639`):

| cc 2.1.220 | kloop | |
|---|---|---|
| `command` | `command` | 同 |
| `timeout` | `timeout_ms` | 有意:单位更明确 |
| `run_in_background` | `background` | 有意改名,旧名有专门的引导报错(`tools/bash.rs:103`) |
| `dangerouslyDisableSandbox` | `disable_sandbox` | 有意 |
| **`description`** | **无** | **缺** |

cc 2.1.220 的 `Bash` 属性集是
`['command','dangerouslyDisableSandbox','description','run_in_background','timeout']`
(来源:`refs/claude-code-2.1.220/fixtures/normalized/` 的工具清单)。前三项差异是 kloop 的
主动改良,`description` 是真缺,而且 `docs/capability-report.md` 第 4 节**没有这笔账**。

### 为什么值得补

它是 UI 用的。`tui/src/toolrow.rs:92` 那条 `"bash" =>` 分支现在只能显示命令原文,
一条 `find . -name '*.tmp' -exec rm {} \;` 要用户自己解析。这是 macOS 上天天看的界面。

### 做什么

照抄 `run_agent` 已有的同名参数(`builtin.rs:843`)的全部约束——**display-only、不进 prompt、
不改结果、单行、非空白、`MAX_DISPLAY_DESCRIPTION_CHARS` 上限**,别另发明一套。
然后让 `toolrow.rs` 的 bash 分支在有 description 时优先显示它(命令原文仍要能看到,
折叠或次行)。

### 验收

1. schema 加上 `description` 后,`run_agent` 的同名参数约束逐条对齐(整对象断言两个 schema
   的该字段)。
2. **它不改变执行**:同一条命令带与不带 description,`dispatch_tools` 的结果逐字节相同。
3. TUI 有 description 时显示它、没有时回落到命令原文,两种都有测试。
4. 顺手在 `capability-report` 第 4 节补这笔账并当场销账(✅ + 本 plan 编号)。

---

## 二、"deny read 必须配 deny write"没有测试守着

### 现状

`cli/src/startup.rs:804-805` 对同一个 `private_state_root` **同时**设了
`with_denied_read_path` 和 `with_denied_write_path`;`:811` 再用 `with_allowed_read_path`
把 offload 目录 carve 回来。`sandbox/mod.rs:205` 的注释明说这个配对是有意的:
"Read-only: the write deny on that ancestor is untouched."

这个配对是**防 `mv x y && cat y` 的关键**:deny 是按路径的,只 deny 读不 deny 写,
agent 就能把文件移出被 deny 的路径再读。grok 记录过这个绕过(见 `refs/README.md`
2026-09-15 节),kloop 已经防住了——**但只靠 startup.rs 里手写的两行,没有任何测试守着**。
将来有人只加一条 read deny 就会留洞,而且不会有任何东西报警。

### 做什么

加一条回归测试,把"配对"变成被守护的不变量。两种做法二选一,开工时看哪个更顺:

- **测试路径**:断言 `startup` 构出的 policy 里,每个 `denied_read_paths` 条目都能在
  `denied_write_paths` 里找到覆盖它的祖先(或它自己)。
- **类型路径**:给 `SandboxPolicy` 加一个 `with_denied_path`(同时加两个列表),让
  `with_denied_read_path` 变成需要显式说明理由的窄接口。更强,但动的是公共 API。

**倾向测试路径**——它不改 API,而且 `bash.rs:2194` 已经有
`denied_read_path_cannot_be_read_through_a_symlink` 这条同族测试可以并排放。

### 验收

1. 新测试在当前代码上**通过**。
2. **它真的能红**:临时把 `startup.rs:805` 那行 `with_denied_write_path` 删掉,测试必须失败。
   红不了就说明断言没咬住。
3. 不改任何生产行为——sandbox 相关的既有测试一条不动。

---

## 三、edit 被拒时不告诉你该读哪一段

### 现状

`edit_file` 要求整个文件完整读过(`builtin.rs:744`),判定在 `fs.rs:1283` 的
`if !expected.is_complete()`。被拒时的信息只是"整个文件必须完整读过"——模型不知道
**要读多少**,于是要么从头分页读完(大文件 5-6 次),要么瞎猜 offset。

`read_file` 在这件事上做得好得多,它的截断提示是
`[showing lines 2-3 of 5; call read_file with offset=4 to continue]`,连下一个 offset 都给了。
`glob` 也是(`Showing 100 of 147 … Narrow the pattern or path`)。**只有 edit 的拒绝信息是哑的。**

### 做什么

工具本来就要读文件才能做替换,所以它知道 `old_string` 落在第几行。把拒绝信息改成指名道姓:

> `old_string` 在第 N 行附近,你还没读过那一段。先 `read_file(path, offset=<N-20>, limit=60)`。

`replace_all` 的情形报最靠前的那个匹配位置即可。找不到 `old_string` 时保持现有报错不变
(那是另一类错误)。

### 与 plan 155 的关系

**这一件独立于 155,先做或不做 155 都该做。** plan 155 讨论的是要不要把资格从"读过全文"
放宽到"读过那一段"(那是推翻 plan 49 的有意裁决,需要用户拍板);本件只改**拒绝时说什么**,
不动资格判定本身。155 若最终决定不放宽,这条仍然省轮数;155 若放宽了,这条正好是它第 3 步
的一半,届时合并即可。

### 验收

1. 未读文件直接 edit:错误信息包含**具体的行号**与一条可以照抄的 `read_file` 调用
   (含 offset 与 limit 的具体数值)。
2. 只读了文件前半、edit 后半:同样给出后半那一段的建议 offset,而不是笼统的"读全文"。
3. **资格判定本身没变**:`fs.rs:1283` 的通过/拒绝集合与改动前完全一致,
   既有的 `existing_write_requires_a_complete_fresh_read_and_refreshes_state`
   (`fs.rs:2422`)等测试一条不改。
4. `old_string` 不存在时的报错不变(别把两类错误混成一句)。

---

## 四、可能的第四件,要先问用户

**`read_file` 读图片只管字节、不管像素。** `core/src/image.rs:16` 写得很坦白:
"Oversized images are refused, not resized — client-side downscaling is deferred
(the user shrinks the image and retries)",上限是 `MAX_IMAGE_BYTES = 5 MiB`。
但开销是按**像素**算的:一张高压缩率 JPEG 可以只有 500 KB 却是 6000×4000。
grok 在客户端处理这件事(`compress_image_for_conversation` /
`should_embed_as_conversation_image`)。

**这条要不要做,取决于用户实际是否给 kloop 看图。** 如果基本不看,`deferred` 这个状态是
对的,不要动——那句注释已经把取舍写清楚了。**开工时问一句**,要做再加进本 plan 当第四件。

## 五、非目标

- **不做 `HeadTailBuffer` / 进程表 LRU**。`capability-report` 第 4 节挂着这笔"小卫生件",
  但它的触发条件写的是"有痛感整段抄",目前没有痛感。
- **不动 `argv_is_dangerous` 的网络命令判定**。`curl`/`wget` 不算危险这件事查过了,
  但用户已拍板"网络可以暂时这样"。
- **不碰 plan 155 的资格放宽**、不碰 plan 153 的 hooks 挂点、不碰 154 的并发与轮预算。
- **不重命名 `background` / `disable_sandbox` 去贴 cc**。那三项差异是有意的改良。
