# Plan 146 — 一个把自己名字说了两遍的目录

> 来源:2026-09-15,用户「我想把 `kloop/` 改名为 rust」。我先推荐了 `engine/`(仓库自己的
> 词汇就是"引擎"),并提出若不进非 Rust 代码更该**拉平到仓库根**;用户让我多列几个,又在
> `rs` 与 `rust` 之间问了一次(我答 `rust`:顶层目录是给不熟悉仓库的人读的,`-rs` 惯例
> 是靠挂在产品名后面才好认的),最后拍板 `rust`。同一轮拍板「改 plan、删 target」。

## 一、为什么

仓库根叫 `kloop`,里面的 workspace 也叫 `kloop`。于是绝对路径要写成
`<repo>/kloop`(plan 85/86 的"从哪执行"两行当时就是这么写的),而"仓库根
不是 workspace 根"这句话已经分别写在 `ci.yml` 的注释、AGENTS.md 的验证行、plan 6 的两处、
plan 28 的 cwd 陷阱条里。套娃本身不产生 bug,但它每出现一次就要额外解释一次。

改名不解决嵌套(那得拉平),只解决**重名**。拉平这条留着,见三。

## 二、做什么

`git mv kloop rust`,然后按**引用是不是被程序读**分成两类。

**会真的断的(四处,全部是路径常量)**:

- `.gitignore` 的 `kloop/target/` 与 `kloop/.kloop/` —— 不改,207G 构建产物和 offload
  输出会直接进 `git status`。
- `.github/workflows/ci.yml` 的 `working-directory`(两个 job)和 `Swatinem/rust-cache`
  的 `workspaces` key。
- `refs/claude-code-2.1.220/verify.py` 的 `KLOOP_WORKSPACE = REPO_ROOT / "kloop"` ——
  CI 里 `--corpus-only` 那一步靠它定位工作区。
- `refs/claude-code-2.1.220/static-evidence.jsonl` 里 54 条 `source_type:"kloop"` 的
  `location`:`verify_repo_location` 会**逐条 open 文件并校验行号在范围内**,不是死文本。

**只是给人读的**:`AGENTS.md` 的 `cd kloop`、`rust/README.md`、`refs/claude-code-2.1.220/README.md`,
以及 `docs/` 下 55 个文件约 470 处 `kloop/crates/...`。用户拍板一起改——留一批指向旧路径
的死链,比历史文件的大 diff 更糟。

**顺手修正**:plan 86 的四处 `kloop/docs/...` 本来就是错的(`docs/` 在仓库根,从来不在
workspace 下),改名不会让它变对,所以直接改成 `docs/...`。

## 三、非目标

- **crate 名与包名不动**。`kloop-core`/`kloop-tui`/`kloop` 这些是产品名,不是目录名;
  `cargo run -p kloop` 照旧。
- **不拉平到仓库根**。这次只做改名。真要拉平(`Cargo.toml`/`crates/` 提到根),得先确定
  这仓库以后不进非 Rust 代码——那是另一个决定,不混在改名里做。
- **三类同形字符串不动**:`kloop/worktree/<name>`(git **分支**名,plan 35 记录的历史值,
  现已是扁平的 `kloop-worktree-`)、`kloop/0.1`(`crates/web` 的 User-Agent)、plan 39 里
  `独立 kloop/ 前端 adapter`(那是 app 仓的目录,不是本仓的)。
- **不动 `.kloop/`**(运行期状态目录,跟着产品名走,不是这次改的那个目录)。

## ✅ 已完成(2026-09-15;提交 SHA 以本条所在提交为准)

`git mv kloop rust`,164 个受版本控制的文件全部以 rename 记录。上面二、里的四处路径常量
与全部文档引用已改完;全仓扫一遍 `kloop/` 只剩非目标里列的那三类共 12 处。

`rust/target`(207G)按用户要求删除,构建缓存本来也会因为绝对路径变化而整体失效。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --all-features
-D warnings`、`cargo test --workspace`,各自单独跑并当场取退出码,**全为 0**(target 已删,
这是一次全量重编译)。

`verify.py --corpus-only` **退出码 1,但失败的是一条既有漂移,与本次改名无关**:
`Plan 52 native run_agent/task graph/wait/stop surface drift`。把那条 39 分句的 `require`
拆开逐条求值,红的是两句:

- `depth_zero_native_tools` 少一个 `stop_program`(它挂在 `run_program` 的 surface flag 上,
  见 `builtin.rs:347`);
- `run_agent` schema 的 properties 是 `{agent_type, background, description, isolation,
  model, prompt}`,fixture 期望的是 `{…, max_rounds}` 而没有 `model`。

时间线:这条期望由 **plan 52(`a42f1b8`)** 写下,`max_rounds` 由 **plan 108(`d1f4f31`)**
从 schema 移除,`model` 由 **plan 92(`2e70f22`)** 加入——fixture 从那时起就没跟上。
本次 `.rs` 内容改动为 0(`git diff --numstat` 里全部 `0 0`,只是 rename),`verify.py` 只改了
第 34 行的 `KLOOP_WORKSPACE`,所以这条断言在改名前的 HEAD 上必然同样红。

**留给后续**:这是 fixture 与实现的对账,不是改名的活;要修得先判哪边是对的
(`stop_program` 该不该进 depth-0 名单、`run_agent` 认不认 `model`),属于另一个 plan。
