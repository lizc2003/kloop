# Plan 59 — 工具对齐总体验收

> 状态：✅ 已完成（2026-08-03）
>
> 母计划：Plan 48
>
> 依赖：Plan 49–58（均已完成）
>
> 固定目标：Claude Code 2.1.220 exact binary（指纹只在本机核验）。
>
> 验收范围：darwin-arm64 本地 CLI、`team=false`、`remote=false` 的 manifest 已执行 profile/condition vector。

## 最终裁决

在上述固定版本、平台、入口和已执行条件内，kloop 对已取证行为达到**受限行为兼容**。

该结论不表示：

- 全工具对齐；
- 名称、schema、wire、UI 或生命周期逐字节一致；
- kloop 是 Claude Code 的 drop-in replacement；
- corpus-only 在 Linux 通过等于目标二进制在 Linux 上已验收；
- 未执行的 team、remote、PowerShell、SendUserFile、真实云服务或账户条件已被覆盖。

`same` 只表示一个具体 matrix cell 被 generated executable pair contract 覆盖。`compatible` 只表示存在可比较行为，不表示 exact parity。安全型 `intentional-diff`、有证据理由的 `unknown`/`n/a` 和 kloop-only 能力继续保留。

## 生成快照

权威数字只来自本次生成产物：

- profiles：14；完整 condition vector 见 `manifest.json`；
- captures：218；65 个 determinism group，每组恰有两次 capture；
- static evidence：214；
- matrix：62 rows × 8 dimensions = 496 cells；
- 状态：`same=24`、`compatible=141`、`intentional-diff=169`、`missing=5`、`unknown=133`、`n/a=24`；
- executable pair contracts：7，精确覆盖全部 24 个 `same` cell；
- exact-bundle profile bridges：108，逐 cell/source profile 固定两侧 condition、diff、fixture hash、bridge evidence 和 dimension-only projection；
- kloop-only：3 rows / 24 cells；从 CC gap 和 parity-success 统计中排除。

五个 `missing` cell 均为 `send-message@clean-cli` 的 registration/schema/parser/executor/output。SendMessage/team messaging 已在开工裁决中排除于本次兼容承诺，故记录为 non-blocking missing，不删除、不改成 parity。其余 `unknown` 均保留原 condition/evidence 边界；平台/入口排除继续保留为 `n/a`。

本次完成门：

- blocking missing：0；
- blocking unknown：0；
- 未裁决 intentional-diff：0；
- cross-chain failure：0；
- 未覆盖 `same`：0；
- 缺失 profile bridge：0。

## 证据闸门

### Static evidence 归属

`build_matrix.py` 和 `verify.py` 都要求每个 generated `static:*` 引用的 `covers` 包含当前 dimension。旧矩阵中跨维度借用的 incidental candidate refs 会在生成时按声明归属剔除，生成产物再执行全局 fail-closed 检查；未知 static id 或 generated 错维度引用直接失败。所有 `bundle` 证据还必须显式声明 locator 模式：字节级 locator 直接在 offset 比较；描述性 locator 必须另带 16–256 bytes 的 exact base64 anchor，full verifier 逐条比较目标二进制 slice，不能只靠自报 SHA/offset 冒充锚定。

### 运行证据

对非 kloop-only、非 `unknown`/`n/a` 的 executor/output/lifecycle cell，verifier 分维度要求 normalized CC fixture 提供 typed witness：tool use 只能来自声明的 model/script/process 路径，result 只能来自真实 `type=tool_result` 结构；复合 `cc_name` 行还必须覆盖 verifier 显式列出的每个 required CC tool，不能用其中一个伴随 Read 冒充 Write/Edit/Search；output 必须绑定该 result content；lifecycle 还必须有有序 process terminal、script final 或 PTY terminal。任意嵌套 metadata `tool_result_ids`、registration capture 或 bundle locator 都不能冒充运行证据。StructuredOutput 因缺少 CC nested schema-child calling fixture，保守裁决为：

`intentional-diff / intentional-diff / unknown / unknown / intentional-diff / intentional-diff / unknown / unknown`。

### 双边证据与 kloop-only

所有 `compatible`/`intentional-diff` cell（严格 kloop-only carveout 除外）必须同时有 CC 与 kloop 证据。kloop-only carveout 固定要求：

- logical id 精确属于 `run-program`、`read-offloaded`、`call-tool` 白名单；
- `surface_kind=family=kloop-only`；
- `cc_name=null`、`kloop_name` 非空；
- 八维全部 `intentional-diff`；
- `child_plan=null`；
- 无 fixture、pair 或 CC static evidence；
- 全部引用均为 kloop evidence。

### Profile bridge

`profile-bridges.json` 是独立 generated artifact。每个跨 profile fixture 都按 `cell + fixture_profile` 生成桥，记录：

- matrix/fixture profile 的 model 与完整 conditions；
- 精确 condition diff；
- fixture case 和 normalized SHA-256，且 bridge fixture id 集合必须与该 cell 在该 source profile 的 matrix fixture refs 精确一致；
- exact-binary bridge evidence；
- `same-binary-dimension-only-v1` projection。

该 projection 不能推出另一 dimension，也不能推出未执行条件。

### Plan 53 漂移纠正

新增 `plan53_parity_tests.rs` executable report，并接入两次 byte-deterministic runner、strict semantic validator 和 negative mutations。Ask parser 的负例已正交拆成 empty/wrong-type/null、1/5 options、各必填子字段 null 与合法请求 unknown-field；generator 只按显式 row→dimension mapping 投影该 report，不再对 `child_plan=53` 全维自动注入。最终五行八维裁决为：

- AskUserQuestion：C/C/C/C/D/C/D/D；
- EnterPlanMode：C/C/U/C/C/C/D/C；
- ExitPlanMode：C/D/D/C/D/N/C/C；
- StructuredOutput：D/D/U/U/D/D/U/U；
- Workflow：C/D/D/C/D/C/C/C。

其中 C=`compatible`、D=`intentional-diff`、U=`unknown`、N=`n/a`。没有新增 `same` 或 pair contract。

## 跨工具链验收

入口：`rust/crates/core/src/tools/plan59_acceptance_tests.rs`。报告由 `KLOOP_PLAN59_ACCEPTANCE_REPORT` 以 create-new 写出，full/corpus verifier 各运行两次并比较原始 bytes，再执行 exact object/type/literal/order 校验和 tamper-negative 检查。报告 scope 分开记录 `target_entrypoint=local-cli` 与 `kloop_entrypoint=core-dispatch-test-harness`：前者是 CC corpus 的验收入口，后者诚实描述 Rust report 的执行路径；该 report 不冒充真实 CLI startup/registry wiring 验收。

1. **file/search → Bash → Worktree**（`full`）：真实 Read/Edit/Glob/Grep/Bash；临时 HOME/XDG/Git config，worktree 的 production Git subprocess 也经 test-only per-repo seam `env_clear`，并用 hostile `.gitconfig` sentinel 证明 global/system/config injection 不可见；fresh worktree FileState；stale-read 拒绝；退出后 cwd/FileState 恢复；branch/path 清理。
2. **background Bash → Agent/Task → scheduler**（`surface-gate`）：完成通知经真实 `run_turn` step boundary 回灌；按各自 framing 精确计数一次并固定 shell→agent→wakeup 完整顺序；Bash timeout；live agent/shell/wakeup 在 session shutdown 时取消并清零。Monitor 在本 profile 无原生注册，不伪造执行。
3. **ToolSearch/dynamic ToolSource/resource → Web**（`seam-only`）：选择性 defer、generation refresh、旧 unlock 失效、共享 typed trace 实测 defer→unlock→approval→dispatch，拒绝审批后 source call 必须为零，成功/失败隔离、本地 Web seam；`external_network=false`、`mcp_transport=false`，不折算为真实 MCP transport parity。
4. **AskUserQuestion → Plan → Workflow**（`surface-gate`）：回答、取消、批准、拒绝、mode 恢复、Workflow 完成通知和 live Workflow shutdown；无原生 PTY，PTY cleanup 明确为 `n/a-no-native-surface`。
5. **Notebook → file mutation / LSP boundary**（`negative-boundary`）：unread/stale 拒绝、Notebook edit、generic write 撤销 notebook qualification、取消不落盘、临时文件清零；无原生 LSP，进程 cleanup 明确为 `n/a-no-native-surface`。

五个 scenario 均在各自声明 scope 内通过；seam-only、surface-gate 和 negative-boundary 不计作缺席 transport/process 的执行通过。

## Fail-closed negative coverage

Verifier 明确拒绝：

- Plan 53/59 缺 scenario、未知字段、bool/int 混淆、缺正交 parser 负例、事件/交付顺序反转或重复交付；
- Plan 59 target/platform/scope 漂移、core harness 冒充 local CLI、seam-only 冒充 full、外网启用、approval/dispatch 反转、deny 后 dispatch、cleanup residue；
- matrix static covers 错维度、未锚定 bundle、metadata result 冒充 runtime、缺 typed dimension witness、registration fixture 冒充 runtime、缺 CC/kloop 单边证据、非白名单 kloop-only、bridge fixture 错绑或缺 profile bridge；
- report 非 byte-deterministic、生成产物不新鲜、fixture/hash/file-set/sensitive scan 漂移。

## 产物与文档

本次同步：

- `refs/claude-code-2.1.220/build_matrix.py`
- `refs/claude-code-2.1.220/verify.py`
- `refs/claude-code-2.1.220/static-evidence.jsonl`
- `refs/claude-code-2.1.220/tool-matrix.json`
- `refs/claude-code-2.1.220/paired-parity.json`
- `refs/claude-code-2.1.220/profile-bridges.json`
- `refs/claude-code-2.1.220/README.md`
- Plan 53/59 Rust reports及 `tools/mod.rs` test registration
- 本 plan、`HANDOFF.md`、`refs/README.md`、`rust/DESIGN.md`、`docs/capability-report.md`

## 验证

```bash
python3 -B refs/claude-code-2.1.220/verify.py
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only
python3 -B refs/claude-code-2.1.220/build_matrix.py --check
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

以上门在完成提交前全部通过；full verifier 读取 exact binary，corpus-only 不读取目标二进制。

## 已拍板裁决

- 最终术语：固定版本、固定平台、已执行 profile 下的“受限行为兼容”；
- 保留有证据的安全 intentional-diff；
- team/remote true、PowerShell、SendUserFile、Monitor、LSP、真实公网/MCP transport 不在本次兼容承诺；
- 不使用“全工具已对齐”“可替换 Claude Code”“drop-in replacement”。

## 完成记录

完成日期：2026-08-03。

提交：本次 Plan 59 验收与文档同步提交，SHA 以本行所在提交为准。
