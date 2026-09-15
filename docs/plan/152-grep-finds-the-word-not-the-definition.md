# Plan 152 — grep 找得到那个词,找不到那个定义

> 来源:2026-09-15,借鉴项目调研后按 macOS-only 前提重排的第四条。参考
> `refs/grok-build` 的 `xai-codebase-graph`——这是逐项核过 codex **也没有**对应物的少数几项
> 之一(codex 的 tree-sitter 只用在 `apply-patch`/`shell-command` 解析,`file-search` 只有
> 文件名模糊搜索)。见 `refs/README.md` 2026-09-15 节。

## 一、现在能做什么

`core/src/tools/search.rs` 是 ripgrep 自家 crate 上的 `grep` + `glob`(`:1`),行为对齐 cc
(500 列截断、20s 上限等,`:35`/`:46`)。加上 `read_file`,这就是 kloop 认识一个代码库的全部手段。

**符号层是空白的,而且是两次空白**:

- 没有代码图。`tree-sitter` 确实在依赖里,但只用于两处解析:`core/src/shell.rs` 的
  bash 命令拆分、`codemode` 的 JS。没有任何索引。
- **也没有 LSP**。plan 57 的三重证据门没闭合,产品裁决是"门未闭合时保留 `unknown`,
  不写推测性 client"(`docs/plan/57-notebook-lsp-parity.md`)。

于是"这个函数在哪定义的""谁调用了它"只能靠 grep 猜名字。同名符号、同名方法、
被 `use` 改过名的、宏生成的,一律靠模型自己在 grep 结果里大海捞针。

## 二、两条路,先选一条

| | 自建 tree-sitter 索引(grok 路线) | 接 LSP client(cc 路线) |
|---|---|---|
| 准确度 | 语法级:定义/引用够用,类型推导做不到 | 语义级,真类型信息 |
| 依赖 | 无外部进程,grammar 编进二进制 | 依赖用户装了对的 language server |
| 可测 | hermetic,`cargo test` 里能跑全 | 要起 stdio 子进程,plan 57 正是卡在这 |
| 语言覆盖 | 每种语言要加一个 grammar | 装了就有 |

**倾向自建**,理由不是技术优劣,是性格:plan 57 已经因为"拿不到权威 hermetic 证据"而拒绝过
LSP,同一个理由在这里仍然成立。自建索引全程在进程内,测试不需要任何外部安装。

## 三、做什么(按自建路线)

1. 一个新 crate(`crates/index` 之类),不要塞进 `core`。它有自己的 grammar 依赖,
   `core` 已经 72k 行了。
2. 先做**定义**,不做引用:`find_definition(symbol)` 给出 `file:line`。引用查询
   (`find_references`)放第二步——它的误报率和成本都高一个量级。
3. 语言先只上仓库自己用得到的:Rust、TypeScript/JavaScript、Python。**别一次铺十种**。
4. 索引存哪:按 plan 105 的项目分区放 `~/.kloop/projects/v1/{project-id}/`,
   和 sessions/offload 并列。**不要**写进工作区(会进 git status、会被 worktree 复制)。
5. 增量:先做"文件 mtime 变了就重解析这个文件",不要一上来就上 fsnotify。
6. 模型面:一个新工具。schema 要窄——`symbol` + 可选 `kind`,不要做成第二个 grep。

## 四、坑

- **索引不能挡住第一次使用**。冷启动要能立刻回答"还没索引好",而不是卡住等全仓解析完。
- **sandbox 边界**:`~/.kloop` 整体在沙箱的 deny-read/write 里(plan 105 的已知副作用),
  索引文件放那儿,沙箱内的 bash 读不到——这是对的,但要确认进程内读者走的是进程路径而不是 shell。
- **worktree**:linked worktree 与主仓共享 ProjectId(plan 105),索引该共享还是分开要定。
  共享会让两个工作树的行号互相污染。
- **生成代码与 vendored 目录**:`target/`、`node_modules/` 必须排除,否则索引体积失控。
  glob 侧已有 .gitignore 尊重逻辑(`search.rs:4`),复用它,别再写一套。
- tree-sitter 的 grammar 会显著拉长编译时间和二进制体积。上之前先量一次。

## 五、开工时问用户(先问,再动手)

**这条要不要做?** 前四条 plan(149-151、153)都是在已有能力上补长尾,这一条是**新增一个子系统**:
新 crate、新依赖、新存储、新工具面、编译时间变长。

它的收益也是最不确定的——收益取决于模型有了符号跳转会不会真的少绕路,而这件事
`docs/capability-report.md` 第 16 节说得很清楚:只有实战里程能回答。

**建议先不做,等一个具体的痛感**:比如某次 dogfood 里明确观察到模型因为 grep 不到定义而
改错了地方。有那个案例再开工,没有就把这个 plan 挂着。

## 六、非目标

- 不做 LSP client(plan 57 的裁决不因本 plan 改变)。
- 不做跨语言的类型推导、不做 rename/重构。
- 不做 grok 的 mmap 索引缓存与 rayon 并行(那是 175 万行仓库的量级问题,不是 13 万行的)。
- 不替换 `grep`/`glob`。符号查询是补充,不是替代。
