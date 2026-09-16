# Plan 156 — 三件小事,各值一次提交

> 来源:2026-09-15/16,一轮工具层通读(plan 154、155 同源)里攒下的零碎。用户拍板
> "立 plan,把一些细碎的改进放进去"。

## 〇、这个 plan 和别的不一样

**它是一个池子,不是一个任务。** 三件事互不相干、各自能单独提交、**做一件也算数**。
接手的人可以只挑一件做完就收工,在下面对应小节补 ✅ 和提交号,不必等三件齐。
验收按件算,没有整体验收。

三件都很小(各自估计几十到一两百行),共同点是:不需要任何架构决定、不推翻任何已有裁决、
不引入新依赖。

**2026-09-16:四件全部完成**(四次提交)。第四件是问过用户之后加进来的,它**不满足**上面
那句"很小、不引入新依赖"——它是这批里最大的一件。

---

## 一、`bash` 少一个 `description` ✅(2026-09-16,提交 SHA 以本条所在提交为准)

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

### ✅ 做完了什么

`builtin.rs` 的 `bash_def` 加 `description`,逐字段抄 `run_agent` 的那一条;`bash.rs` 的
`BashInput` 加上同名字段(`deny_unknown_fields` 否则直接拒),约束走既有的
`optional_display_description`,**没有新发明一套**。`toolrow.rs` 的 bash 行改成
"description 在前、`$ 命令` 在后",没有 description 时和以前逐字节一样。

三条测试:`display_description_is_one_schema_contract`(bash / run_agent / run_program
三个 schema 的该字段**去掉散文后整对象相等**,这样将来谁改松一个 `maxLength` 就会红)、
`bash_description_is_display_only`(带与不带 description 的 `run_tool` 返回值整对象相等;
空白 description 在 spawn 之前被拒)、`bash_row_leads_with_the_description_when_there_is_one`。

**一条既有测试要改**:`background_field_is_strict_and_old_name_fails_closed` 原本拿
`description` 当"未知字段"的样本——它现在是已知字段了。换成 `timeout`(cc 的拼法,
单位不同,静默忽略会按错的单位跑),严格性的守卫没有变弱。

README 的 "Foreground bash lifecycle" 一节和 `capability-report` 第 4 节各补一条。

---

## 二、"deny read 必须配 deny write"没有测试守着 ✅(2026-09-16,提交 SHA 以本条所在提交为准)

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

### ✅ 做完了什么(以及 plan 这一节的前提说错了一半)

**"没有任何测试守着"不对。** `startup.rs` 里早就有
`sandbox_denies_private_state_tree_reads_and_writes`,它两条 deny 都断言了,验收第 2 条
(删掉 `:805` 那行必须红)在动手之前就已经满足。真正缺的是 plan 那句"将来有人只加一条
read deny 就会留洞"——**那条既有测试点名断言 `private_state_root` 这一个路径,第二个
read deny 加进来它一声不吭**。

所以按"测试路径"补的是那条**普遍**不变量:`every_read_deny_is_covered_by_a_write_deny`——
`build_sandbox` 构出的 policy 里,每个 `denied_read_paths` 条目都要能在 `denied_write_paths`
里找到覆盖它的祖先(`Path::starts_with`,按 component 比,`/a/bc` 不会被 `/a/b` 蒙混)。
断言的是 uncovered 列表整个等于空,失败时直接把漏掉的路径打出来。

负对照跑过:临时删掉 `startup.rs:805`,新测试红,报出那两个别名路径
(`/var/...` 与 canonical 的 `/private/var/...`,`push_path_aliases` 两条都会进列表)。

没动任何生产行为。**顺手修了一处文档错位**:`sandbox/mod.rs` 里
"Add a file the model-facing shell must never read…" 这段注释挂在了
`with_allowed_read_path` 头上——它显然是 `with_denied_read_path` 的,后者反而裸着。
归位,并在里面写清这个配对为什么必须成对(`mv x y && cat y`)。

---

## 三、edit 被拒时不告诉你该读哪一段 ✅(2026-09-16,提交 SHA 以本条所在提交为准)

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

### ✅ 做完了什么(plan 给的那句提示词是错的,换掉了)

**plan 建议的 `read_file(path, offset=<N-20>, limit=60)` 照着做解不开锁。** 这一件明确
"不动资格判定",而资格要的是 `is_complete()`——`ReadCoverage::recompute_complete` 只认
"合并后是一条从 0 覆盖到 total 的区间"。读 old_string 周围那 60 行,覆盖率还是残的,
再 edit 还是同一句拒绝。模型多走一轮,回到原地。

改成报两件事:**old_string 落在第几行**(定位,plan 要的那个数)+ **第一行还没读过的行**
(offset,能真正合拢覆盖率的那次读)。后者往下靠 `read_file` 自己的续读提示接力,
和截断读是同一套idiom:

> `edit_file: must read the entire file /x/y.rs before modifying it (old_string is at
> line 33 of 41; call read_file with offset=11 to continue)`

**不给 `limit`**(验收第 1 条写了"含 offset 与 limit 的具体数值"):给了就把这次读截在
半路,覆盖率照样合不拢。不给 limit = 读到 EOF(仍受 `READ_CONTENT_CHARS` 预算,截断时
read_file 自己会给下一个 offset)。

落点四处:`text_edit::first_match_line`(与 `apply_text_edit` 同一套 exact-first →
logical-LF 匹配,CRLF 文件不会报一个模型找不到的行号;行号在 logical view 里数就是在
raw 里数)、`FileObservation::first_unread_unit`(coalesce 过的区间,第一个洞的起点)、
`fs.rs` 的 `unread_edit_hint` + 给 `validate_observation_metadata` 加一个
**只在那两种"读一下就能解开"的判定上才调用**的 `unread_hint` 闭包(另外两个调用点传
`|| None`;Stale 那条不碰——它要的是重读,不是补读)。

验收逐条:①②在 `unread_edit_names_the_line_and_the_read_that_clears_it` 里整对象断言
(不是 contains);③资格判定一行没动,既有 fs 测试一条没改;④old_string 不存在时
**不加任何括号**,报错与改动前逐字相同。**另外加了一条 plan 没要求但更重要的**:照着提示
读完之后那次 edit **真的成功**——否则这条提示只是把模型送回同一堵墙。

顺带记一个坑:失败的 edit 会**撤销**已有观察,所以同一个测试里连着三次拒绝,第三次的
判定是"从没读过"而不是"读得不全"。

---

## 四、第四件:`read_file` 读图只管字节、不管像素 ✅(2026-09-16,提交 SHA 以本条所在提交为准)

**`read_file` 读图片只管字节、不管像素。** `core/src/image.rs:16` 写得很坦白:
"Oversized images are refused, not resized — client-side downscaling is deferred
(the user shrinks the image and retries)",上限是 `MAX_IMAGE_BYTES = 5 MiB`。
但开销是按**像素**算的:一张高压缩率 JPEG 可以只有 500 KB 却是 6000×4000。
grok 在客户端处理这件事(`compress_image_for_conversation` /
`should_embed_as_conversation_image`)。

**这条要不要做,取决于用户实际是否给 kloop 看图。** 如果基本不看,`deferred` 这个状态是
对的,不要动——那句注释已经把取舍写清楚了。**开工时问一句**,要做再加进本 plan 当第四件。

### ✅ 问了,用户先问"参考项目是怎么做的",看完答"做"

**五家参考全都做,而且形状一致**(三层常量:解压炸弹守卫 → 像素/边长预算 → 字节预算 +
质量阶梯)。cc `imageResizer.ts`(sharp,3.75 MB raw / 2000 px / JPEG 80-60-40-20);
grok `read_file/image.rs`(768 KiB base64 / 面积 1.05 Mpx / 边 2000 / 下限 128 / ×3/4 阶梯);
codex `utils/image`(按 32×32 patch 算,2048 边 / 2500 patch,并向模型发
`<image_resize_notice>`);codewhale `read_media.rs`(detail 参数 + crop + 平面图/照片分类,
降级过的原图存盘);dsh `attachment-local`(2048² 总像素 / 4 MiB,prompt 里写明"Normalized
copy … may be resized")。

### 动手前查了官方文档,两个数改了设计

按仓库纪律(不能凭记忆答 API limit)拉了 `build-with-claude/vision`,**两条推翻了我原本的
判断**:

1. **超限的图 API 自己会降采样,不是拒绝**(high-resolution tier:长边 2576 px / 4784
   visual token;patch 是 **28×28**,不是 codex 的 32×32)。所以客户端降采样**省不到
   token**——token 账 API 已经封顶了。它省的是**请求字节**(kloop 每轮重发整段历史)。
2. **单图上限是 10 MB base64(Claude API 直连)/ 5 MB(Bedrock、Google Cloud)**,不是我
   以为的"kloop 的 5 MiB raw 已经超了"。5 MiB raw ≈ 6.7 MB base64,直连没问题,Bedrock/
   Vertex 会超。取 3.75 MiB raw(= cap × 3/4)让一种编码在所有路线上都安全。

两条**真的把它从"省钱"变成"正确性"**的规则:单图 **8000×8000** 直接拒;**一个请求里超过
20 个 image block 时,对所有图套用更严的逐图尺寸限制**,文档给的办法就是"把每张图缩到两边
都不超过 2000 px"——这正是 cc 和 grok 都落在 2000 的原因。

### 落点

`core/src/image.rs`:`prepare_image_from_bytes` 返回 block + `Resized`(原尺寸/新尺寸);
`image_block_from_bytes` 变成它的薄包装,`notebook.rs` 和 TUI 粘贴白拿降采样。
`WIRE_TARGET_BYTES` = 3.75 MiB、`MAX_WIRE_DIMENSION` = 2000、`MIN_WIRE_DIMENSION` = 256、
JPEG 阶梯 80/60/40/20、`MAX_DECODE_PIXELS` = 64 Mpx、`MAX_DECODE_ALLOC_BYTES` = 256 MiB。
**PNG 每一级先试、合就留**,不像 grok 那样"取更小的"——kloop 看的绝大多数是截图,而文档
明确警告重 JPEG 压缩会让文字难认;照片的 PNG 本来就不合,自己会落到 JPEG 那级。
`fs.rs` 的 read_file 在降采样时追加一条 `<system-reminder>` 报出两个尺寸(模型接下来要读
像素,它报的坐标是降采样后那份的)。

**依赖没有想象中贵**:`image` 0.25 早就在树里(`arboard` 的依赖,只开了 png+tiff),
这次只是补上 jpeg/gif/webp 三个解码 feature,不是新引一个 crate。

### 非目标(做了但要写清没做什么)

- **5 MiB 读上限不动**。超过 5 MiB 的图仍然是拒绝,不是"读进来再压小"。抬这条线是
  plan 61 的裁决,不属于本件。
- **不加 `detail` / `crop` 参数**(codewhale 有),也不存降级前的原图。没有痛感。
- **不碰 GIF 动画**。API 本来就只用第一帧,现状注释已经说对了。

### 验收

六条新测试(`image.rs` 五条 + `fs.rs` 一条):字节像素都够小时**逐字节原样送出**(整对象
断言);**字节小、像素大**的那张被降采样(并先断言它确实在字节预算内——否则这条测试证明
的是字节上限而不是像素预算);**永不放大**且比例不变;68 字节的 PNG 声称 40000×40000 时
**在解码前**被守卫挡掉;**头读不出来就照旧原样送**;read_file 的降采样通知整串相等。

## 五、非目标

- **不做 `HeadTailBuffer` / 进程表 LRU**。`capability-report` 第 4 节挂着这笔"小卫生件",
  但它的触发条件写的是"有痛感整段抄",目前没有痛感。
- **不动 `argv_is_dangerous` 的网络命令判定**。`curl`/`wget` 不算危险这件事查过了,
  但用户已拍板"网络可以暂时这样"。
- **不碰 plan 155 的资格放宽**、不碰 plan 153 的 hooks 挂点、不碰 154 的并发与轮预算。
- **不重命名 `background` / `disable_sandbox` 去贴 cc**。那三项差异是有意的改良。
