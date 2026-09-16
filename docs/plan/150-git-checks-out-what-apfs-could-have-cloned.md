# Plan 150 — git 在逐字节抄,APFS 本可以一次克隆 ⛔ 已撤销

> **2026-09-16 撤销,不要实施。**理由见文末「⛔ 撤销」一节;第五节那个前置问题有答案了,
> 答案正是该节写明会导致撤销的那一个。

> 来源:2026-09-15,借鉴项目调研后按 macOS-only 前提重排的第二条(第一条是 plan 149)。
> 参考 `refs/grok-build` 的 `xai-fast-worktree`(见 `refs/README.md` 2026-09-15 节)。

## 一、现在怎么建的

`core/src/worktree.rs:258` 的 `create_managed`:先 `create_dir_all` 建
`<root>/.kloop/worktrees/`(`:296`),再 `git worktree add`(`:304`)。checkout **整个由 git 做**——
git 从 ODB 逐个文件解压、写盘。工作区越大越慢,而这条路径上每次
`worktree` 工具调用、每次 `enter` 都要走一遍。

## 二、macOS 上这一步可以几乎免费

grok 的 `xai-fast-worktree` 把它拆成两段:

1. `git worktree add --no-checkout` —— 只建元数据,瞬间返回;
2. 并行 **CoW 克隆**文件进去。

CoW 那段在 macOS 上就是 `clonefile(2)`:APFS 原生支持,克隆时不复制数据块,只加引用,
写时才分裂。grok 另外用 BTRFS 快照覆盖 Linux,**那部分与本 plan 无关**——kloop 目前只在
macOS 用,`clonefile` 这一条就够。

`worktree.rs` 全文没有 `clonefile`/CoW 的任何痕迹(grep 为空)。

## 三、做什么

1. `create_managed` 改两段式:`git worktree add --no-checkout` 之后,自己把工作树文件铺进去。
2. 铺文件走 `libc::clonefile`(macOS)。**一定要留非 CoW 回退**:目标不在 APFS 上(外接盘、
   磁盘映像、网络卷)时 `clonefile` 会失败,此时退回普通复制或干脆退回让 git checkout,
   不能让 worktree 创建整体失败。
3. 源从哪儿拿要先定:从主工作区的当前文件克隆(快,但会带上未提交改动),还是从 ODB 展开
   (干净,但就没有 CoW 可用了)。grok 那边是"可选地复制 dirty 文件与 ignored 文件",
   说明它按前者做并把差异当特性。**这是本 plan 最需要先想清楚的一步**,见第五节。
4. 补一条计时断言以外的正确性测试:CoW 建出来的工作树与 git checkout 建出来的,
   `git status` 必须一致。

## 四、坑

- **`clonefile` 要求目标不存在**,不能覆盖;先建目录树再逐文件克隆。
- **`.git` 文件不要克隆**。`git worktree add` 已经写好了 linked worktree 的 `.git` 文件
  (指回 common dir),把主工作区的 `.git` 克隆过去会把它冲掉。
- **ignored 文件的取舍**:`target/` 这类目录 CoW 克隆几乎不要钱,但语义上是不是该进新工作树
  要定。默认不带,和现在 git checkout 的行为一致。
- **权限与 xattr**:`clonefile` 保留元数据,普通复制回退路径要自己保证不丢可执行位。
- 这条只在 `cfg(target_os = "macos")` 上有实现,其余平台走回退路径;别让 `#[cfg]` 把
  `worktree.rs` 劈成两份难读的实现——教训 132(拉取式钩子)可以参考。

## 五、开工时问用户(先问,再动手)

**新工作树的内容,以主工作区的当前文件为准,还是以 HEAD 为准?**

- **当前文件**:才能用 CoW(克隆已经在盘上的文件),这是本 plan 的全部收益来源。代价是
  未提交的改动会跟进新工作树——对"从当前状态开一个分身去试"是特性,对"从干净 HEAD 起一个
  隔离环境"是污染。
- **HEAD**:语义干净,但那就是 git checkout 本来在做的事,CoW 无处可用,本 plan 不成立。

`worktree` 工具现在的语义是哪一种,要连同 `docs/plan/56-worktree-parity.md` 一起确认——
如果 parity 把"新工作树 = 干净 HEAD"钉死了,这个 plan 应该直接撤销,而不是改语义去迁就性能。

## 六、非目标

- **不做 BTRFS / Linux 快照**。macOS-only 前提下那是别人的能力。
- **不改 worktree 的命名、分支前缀或目录布局**(`.kloop/worktrees/`、`kloop-worktree-*`
  是 2026-08-31 那次回退的结论,见 HANDOFF plan 35 修正条)。
- **不做并行度调优与分片**(grok 有 hash 分片)。先让 CoW 这条路通,快多少再说。
- 不碰 `finish`/`enter`/`enter_existing` 的语义。

## 七、验收

**前置**:第五节那个问题必须先有答案。答案是"以 HEAD 为准"则本 plan 撤销,不要往下做。

1. **等价性**:同一个仓库,用 CoW 路径与原 `git worktree add` 路径各建一个工作树,
   两者的 `git status --porcelain` 输出**逐字节相同**。这是本 plan 唯一不能让步的一条。
2. `.git` 文件正确:新工作树里的 `.git` 仍是 `git worktree add` 写的那个 linked 指针,
   不是主工作区 `.git` 的克隆。`git rev-parse --git-common-dir` 在新工作树里指回主仓。
3. **回退路径真的被走过**:构造一个非 APFS 目标(或直接让 `clonefile` 返回错误)的测试,
   断言 worktree 仍然创建成功。不能只在 happy path 上有测试。
4. 可执行位与符号链接在两条路径下一致。
5. 提速有数:记一次两条路径的耗时对比进 plan 的 ✅ 节。**没有提速就不要合**——本 plan
   除了速度没有别的收益,不快就是净增复杂度。
6. 仓库完成标准照旧(fmt / clippy -D warnings / test,各自取退出码)。

## ⛔ 撤销(2026-09-16)

第五节的前置问题——"新工作树的内容,以主工作区的当前文件为准,还是以 HEAD 为准?"——
用户一句「工作树可能是基于某个提交做出来的」点明了,而代码本来就写着答案:

```rust
// core/src/worktree.rs:290-293
BasePolicy::Head  => git rev-parse HEAD                    // task isolation
BasePolicy::Fresh => origin/HEAD(或 origin/<当前分支>)      // session tree,:478
```

**两种 base 都是提交,没有"以当前文件为准"这一档。**按第五节和第七节前置条件的约定,
本 plan 到此撤销。

而且 `BasePolicy::Fresh` 比"HEAD"更彻底地否掉它:session tree 的 base 是 `origin/HEAD`,
与本地工作区可以差任意多个提交。从当前文件 CoW 克隆过去,那些差异会**全部表现为未提交
改动**——而 plan 56 把 base commit 钉进了 provenance 验证(`56-worktree-parity.md:70`)和
删除判定(`:62`,"tracked/staged/unstaged/untracked/ignored 变化或 base 后 commit 均阻止
默认 remove")。第七节第 1 条"两条路径 `git status --porcelain` 逐字节相同"必然失败,
而那条是本 plan 明说不能让步的。

**成本是真的,只是这条路不对。**kloop 仓库 790 个 tracked 文件、147 MB,其中 **139 MB 是
`refs/claude-code-2.1.220/fixtures`**——每建一个托管工作树,git 都要把这堆 parity fixture
重新写一遍,而绝大多数任务碰都不碰它。要省这笔,两条方向都与 CoW-from-dirty-tree 无关:

- **sparse-checkout**:`--no-checkout` 之后用 sparse 规则把 fixtures 排除掉再 checkout。
  几行、无平台代码、不改 base 语义,`git status` 也不受影响(skip-worktree 不报脏)。
  代价是工作树里跑不了 `verify.py --corpus-only`,要先确认没人在托管树里跑它。
- **从提交固定的源 CoW**:克隆源不是脏的主工作区,而是一个已经在该 commit 上的兄弟工作树
  或一次性模板。语义正确,但复杂度远高于上一条,而且收益要先量。

真要做,做第一条,并且**重新立 plan**——它和本 plan 的机制、风险、验收都不是一回事。
