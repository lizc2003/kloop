# Plan 176 — 没有一道门拦着文件继续长

> **⚠️ 这道门禁已于 2026-09-21 整体删除,本文件只作历史。** `tests/architecture.rs`、
> `architecture-policy.toml`、`architecture-baseline.toml` 与 `make arch-baseline` 都不在了。
> 删的理由不是"卡增量"这个想法错,而是这道门禁**两个方向都报错**:文件变小也失败、
> 要你跑一次 `make arch-baseline` 提交一份基线 diff 才肯绿——于是它恰好在它本该鼓励的
> 事情上发作,而 177–186 那一批的全部目的就是把基线上那 23 个文件改小。
> 完整复盘见 `HANDOFF.md` 教训 172 的后记。

> 来源:2026-09-21 调研 `zai-org/ZCode@872ad960`(Apache-2.0)时,用户看到现状的行数后
> 一句「架构治理值得做啊,现状的文件太大了」。ZCode 那套做法里可移植的是**机制**
> (policy + baseline + 只卡增量),不是它的模块分层规则——后者 Cargo 已经免费给了,见第六节。

## 一、现状:127 个文件里 22 个越界,而没有一条规则是机器判的

口径见第三节(非测试文件、剔掉 `#[cfg(test)]` 块、不数空行与纯注释行):

| code 行 | 文件 |
|---|---|
| 2148 | `rust/crates/tui/src/render.rs` |
| 1910 | `rust/crates/server/src/lib.rs` |
| 1909 | `rust/crates/cli/src/mcp.rs` |
| 1632 | `rust/crates/core/src/permissions.rs` |
| 1584 | `rust/crates/tui/src/app.rs` |
| 1524 | `rust/crates/core/src/tools/fs.rs` |
| 1500 | `rust/crates/core/src/rollout.rs` |
| 1187 | `rust/crates/core/src/scheduler.rs` |
| 1161 | `rust/crates/provider/src/responses.rs` |
| 1152 | `rust/crates/core/src/tools/mod.rs` |
| 1090 | `rust/crates/core/src/process_tree/windows.rs` |
| 1088 | `rust/crates/cli/src/private_store.rs` |
| 1065 | `rust/crates/core/src/agent.rs` |
| 1007 | `rust/crates/core/src/provider_route.rs` |

127 个非测试文件、5.51 万 code 行。按阈值看超标个数:
**500 → 42,600 → 33,700 → 27,800 → 22,1000 → 14,1200 → 7**。

AGENTS.md 里写了一串风格约束,**没有一条是机器判的**。`rust/DESIGN.md` 那个
"3800 行、累计 +4086/−266、只追加正是过期的来路"的毛病是同一个病根:
**约定没有门禁就会被绕过,而且是无声地绕过**。

## 二、做成棘轮,不做成限高杆

一次性拆掉 22 个文件是错的:机械拆分会把 `impl` 的内聚切碎(CodeWhale 那条结论已经写过
"不抄巨型 `turn_loop.rs`/subagent 单文件",判据是职责堆叠,不是行数本身),而且和当前
功能开发抢时间。要的是**让它停止变大**:

- **baseline 冻结存量**:每个超标文件记下当前值,**只许降不许升**。涨了就是门禁失败。
- **新文件必须合规**:不在 baseline 里的文件,超阈值直接失败。
- **降了要能收**:实际值低于 baseline 时报一行提示,由人跑更新命令把水位收紧。
- **CI 从不自动刷新 baseline**(照搬 ZCode 这条规矩)。刷新是一次需要写进提交信息的决定。

棘轮的好处是第一天就能接进门禁——不需要先还完债。

## 三、口径:这是本计划最要紧的设计

三条都必须有,少一条这个门禁就会变成反向激励:

1. **测试文件整体排除**:`tests/` 目录下的、文件名 `tests.rs` 或 `*_tests.rs` 的。
   kloop 13.5 万行里 7.2 万是测试,不排除的话榜首全是测试文件,门禁变成"逼你把测试拆出去"。
2. **文件内 `#[cfg(test)]` 块整段排除**,而且必须**大括号配平**地排除,不能只截断到第一个
   `#[cfg(test)]`——`core/src/tools/mod.rs` 里 test 块不在文件末尾,只截第一个会把它后面
   一千多行 production 代码一起漏掉(4231 总行 → 错算成 1057,实际 code 1152)。
3. **不数空行与纯注释行**。AGENTS.md 要求"注释只写代码看不出来的约束",kloop 的注释密度是
   刻意的;**门禁不能惩罚它**。

口径差多少,看两个例子:`permissions.rs` 总 4177 行 → 剔测试 2233 → 再剔注释空行 1632;
`tui/src/app.rs` 总 4279 → 2057 → 1584。**2.5 倍的差**,所以口径必须先定死再定阈值。

## 四、形态:一个测试,不是一个脚本

**推荐做成 `cargo test` 里的一个测试**,理由是零新增门禁配置:`make test` / `make check` /
CI 三平台矩阵自动全覆盖,不用往 `ci.yml` 加步骤,也不会出现"CI 跑了本地没跑"。

`scripts/` 下那两个 python(`review-pace.py`、`tool-usage.py`)是**分析工具不是门禁**,
别把门禁混进去——它们不进 CI,放进去就等于没有。

实现要点:

- 测试用 `env!("CARGO_MANIFEST_DIR")` 往上定位到 `rust/crates` 再遍历,跨平台且不依赖 cwd。
- policy 与 baseline 都用 toml(kloop 的配置一律 toml),放 `rust/` 下与被检查的代码同层。
  baseline 是 `路径 = 行数` 的平表,diff 可读。
- 失败信息必须同时给**当前值 / baseline 值 / 阈值**三个数和文件路径,否则拿到失败的人
  不知道该拆还是该更新基线。
- 更新基线走一条显式命令(`make arch-baseline` 包一层 `#[ignore]` 的测试或一个 bin),
  不接受环境变量顺手重写。

## 五、阈值:推荐 800

800 让 22 个文件进 baseline——多到值得治理,少到每一个都还认得出是谁。600(33 个)会把
一批本来健康的文件也拖进名单,1000(14 个)则放过了 `agent.rs`、`provider_route.rs`
这一档正在长大的文件。**这是开工时要跟用户确认的点**,数在第一节,改阈值只改一个常量。

## 六、只做体积这一件,不抄模块分层

ZCode 的 `architecture-policy.yaml` 还声明了每个模块的 roots / requires / publicEntrypoints /
layers / layerOrder / owner。**这部分不抄**:

- **crate 之间的依赖 Cargo.toml 已经强制了**,而且是编译期。ZCode 需要这一层,正因为 TS 没有。
- **crate 内部的 domain/app/adapters 分层在 kloop 没有对应概念**。强加会造出一批假边界,
  然后每个新文件都要先回答一个本来不存在的问题。
- `publicEntrypoints` 在 Rust 里是 `pub` 与 `mod` 的可见性,不需要第二套声明。

留一条口子就够:policy 文件的结构要允许以后加规则(比如"某个 crate 不得依赖某个 crate"),
但**这次只实现体积一条**。

## 七、开工时要定的点

1. 阈值取 800 还是别的(第五节)。
2. policy/baseline 文件名与位置。
3. 这个测试挂在哪个 crate 的 `tests/` 下(它不依赖任何 crate 逻辑,只读文件系统)。
4. 超标时是 `panic!` 一条汇总(列出全部越界文件)还是每个文件一条——推荐汇总,
   一次跑出全部问题。

---

## ✅ 已完成(2026-09-21)

用户拍了阈值 **800**(第五节推荐值),其余三个待定点按第七节推荐落地。

- `rust/architecture-policy.toml` — `[file_size] max_code_lines = 800`,分节结构留给后续规则。
- `rust/architecture-baseline.toml` — 23 行 `"路径" = 行数` 的平表,按路径排序。
- `rust/crates/cli/tests/architecture.rs` — 门禁本体 + 更新器 + 8 条口径单测。挂 cli 的
  `tests/` 是照 `doc_placement.rs` 的先例:全仓不变量不属于任何一个 crate。
- `make arch-baseline` — 包 `#[ignore]` 的 `rewrite_the_size_baseline`。

### 第一节那张表是错的,口径必须自己写词法器

plan 的行数是**按行扫**数出来的,而按行扫在这个仓库里两个方向都会错:

- **少排**:`tui/src/render.rs` 的 test 模块里有 `.ends_with("}}]")`,字符串里两个裸 `}`
  把花括号配平提前打平,模块剩下的 ~1200 行测试全被当成 production。**2148 → 真值 1126**,
  它根本不是榜首,`server/src/lib.rs` 1910 才是。
- **多排**:`#[cfg(test)] use super::*;` 这种没有花括号的项,按行扫会一路吞到下一个
  `{...}` 收尾,`process_tree/windows.rs` 因此少算 83 行(1090 → 真值 1173)。

所以计数落在一个小词法器上:先把文件投影成"去掉注释、每个字面量塌成一个字符"的字符流,
再在投影上配平 `#[cfg(test)]` 项(`;` 结尾的项按分号收,括号/方括号计数防住
`const X: [u8; 3]` 提前收尾),最后数投影里非空白的行。`#[cfg(all(test, windows))]`
算测试(谓词按 all/any/not 递归判),`#[cfg(not(test))]` 不算——这两种本仓库都有,
判错任一个都会在门禁上静默地少算或多算。**认不出来的谓词一律当 production**:
多数是响亮的失败,少数是无声的放松。

**真实现状**:127 个非测试文件、**53685** code 行,阈值 800 下 **23** 个超标(比 plan 多
一个 `cli/src/main.rs` 833)。另写了一份独立的 python 实现逐文件对照,23 个数全一致。

### 一处偏离 plan:基线落后于现实是失败,不是提示

第二节写"实际值低于 baseline 时报一行提示"。做不到:libtest 默认吞掉通过测试的 stdout/stderr,
这行提示谁都看不见——正是第一节批的"无声地绕过"。于是改成**失败**,信息里直接给
`make arch-baseline`。这一条之所以不危险,是因为更新器**只降不升、从不新增**:
不在基线里的路径加不进去,所以没人能靠跑一下命令把新的超标文件合法化。

### 验收

`make check` 全绿。五条负向对照手工跑过并复位:① 文件涨过基线 → 失败;② 更新器在有增长时
拒绝写入;③ 基线落后(缩了 / 已低于阈值 / 文件没了)→ 失败并指向 `make arch-baseline`;
④ 新文件一出生就 900 行 → 失败;⑤ `make arch-baseline` 收紧后与原基线逐字节一致(幂等)。
