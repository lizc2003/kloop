# Plan 280 ✅ — 分开判断安全性与正常功能的固定对照

> 2026-11-10。用户同意再做一轮，并在无明确收益时暂停增加机制。

## 范围与停止条件

只评估一段通用的错误排除检查要求：分别判断安全边界和正常调用合同，不因安全性
没有下降而排除正常功能回归。沿用结构化文本、完整性/引用校验和实际正常采样，不改
交付 schema、工具、预算或模型。生产入口不动，不追加实验开关。

- 先冻结提示改动，再人工选择三个未用于 plan 272–279 实验调参的历史真实提交。
  每份提供一条有源码与后续回归依据的缺陷候选、一条同路径非缺陷对照。候选编号
  混排；固定原父版本/目标版本代码。后续修复、测试答案、历史审查和标签不进提示。
- 两种提示各跑两次，每次独立，最多 12 个真实请求，每格两次，不重试、换样、取并集
  或按结果修改问题。没有补查/修复，只有最终裁决；正常采样沿用前轮本机捕获，核对
  正常配置摘要。来源、候选、schema、传输预算与执行环境保持一致。
- 分开验收整包交付、实际缺陷保留、误报、unresolved、有效触发与引用、token 和耗时。
  未交付和 unresolved 均未救回缺陷；标签命中而触发或关键论证不成立也不算成功。
- 只有新组有效确认至少增加 2 次、增益分布在至少两个不同提交、误报不增加、完整交付
  不下降，且输出 token 和中位耗时分别不超过旧组 1.5 倍，才保留改动进入后续验证。
  另外单列缓存/非缓存输入，不拿输出减少冒充费用减少。小样本不做统计显著或生产
  召回承诺；人工候选与切片不能代表端到端审查。
- 任何条件不满足或旧组已没有足够漏报可比较，都记录本轮没有明确收益，暂停增加
  提示和机制；保留原实验入口，不将新要求默认接入。所有失败与成本保留。

## 完成记录

已完成 12 个客户端真实请求，没有补样、客户端重试、修复或修改材料。新要求未满足
请求前写定的保留条件，因此不采用；暂停继续增加审查提示和机制。保留 plan 278/279
已有实验入口，生产审查入口和运行代码不变。

### 协议与完整性

- 两组固定使用结构化文本，沿用本机正常采样捕获：`deepseek-v4.1-flash-1017`、
  `max_output_tokens=65536`、`reasoning.effort=xhigh`。开流 / 空闲 / 正文截止分别为
  300 / 900 / 1,800 秒，父进程截止 2,105 秒。没有工具调用或额外修复请求。
- 请求前冻结自有新提示、脚本、helper、来源包与全部请求。事后核对正常配置摘要
  未变，helper 与仓库一致；六对请求只有冻结的新要求不同，同一材料/组别两次的
  请求体完全一致。两版 24 个来源范围逐行匹配 Git，提示中没有本地答案标签。
- 12 份原始流都只有一个终态，与保存的 response 和正文一致。11 包通过原有完整性、
  字段和引用范围校验；另 1 包终态虽为 completed，却缺引用，整包失败，不拆出好
  条目计成功。没有放宽验收。原始捕获、密钥、端点和私有服务标识均未进入仓库。

### 结果与人工验收

这里的“确认”要求核心触发与前后错误结果成立；未接受的附带解释另列，不能把整段
报告都视为正确。每份材料每组只有两次，不把重复当作独立缺陷。

| 材料 | 旧提示两次 | 新提示两次 |
| --- | --- | --- |
| A：项目目录尾斜杠 | 2 次确认；2 次正确排除私有配置对照 | 同旧组 |
| B：xargs 非读取参数角色 | 2 次确认；2 次正确排除 echo 对照 | 同旧组 |
| C：缩小后失效坐标 | 1 包缺引用失败；1 包确认并排除对照 | 1 包缺陷 unresolved；1 包确认；两包均排除对照 |

| 指标 | 旧提示 | 新提示 |
| --- | ---: | ---: |
| 整包通过 | 5/6 | 6/6 |
| 有效核心缺陷确认 | 5/6 | 5/6 |
| 正确排除无缺陷对照 | 5/6 | 6/6 |
| 误报 | 0（实际交付 5 个对照） | 0（实际交付 6 个对照） |
| 已交付 unresolved | 0 | 1 |

- A 的核心链条成立：普通项目 `/.kloop/` 后的空 entry 未被 mask，原文扫描提前
  返回 Unresolved，消费方硬拒绝；元数据路径层本来允许目录本身。私有 config 读取
  两版都被拒。旧组一次把后续 Secret 路径描述成已执行，实际早已在原文扫描返回；
  这处解释不接受，但不改变已核对的核心缺陷与对照结论。
- B 的具体 `notes.txt` / touch 触发在原版可走普通授权，新版被参数改写 guard 硬拒，
  hazard 也错误升级；后续角色回归覆盖相同触发。echo 的明确豁免两版均在。旧组一次
  将原版分析描述成已展开的 touch argv，实际仍是原始 xargs fact；新组一次另举包含
  字面敏感路径的 printf 例子，错误声称原文没有该路径。这些附带解释不接受。不能把
  此例推广为应放行任意未知元数据路径，更不代表未知内容读取应被允许。
- C 旧组首包两个 evidence 都为空，虽然正文写出了失效坐标，仍按失败计。新组首包
  承认返回越界坐标，却因缺 resize 消费方而 unresolved，未救回缺陷。第二对的核心
  确认可由“缩小后返回旧的屏幕外坐标”成立；新组进一步声称所举全高 viewport 序列
  必然锚到屏外，缺少消费方依据，不接受该扩展后果，也不算已证明 panic。真实历史
  UI 回归的触发还包含 grow→commit→shrink，未将后续代码补入请求。

新增有效确认 **0 次、涉及提交 0 个**，未达到至少增加 2 次且分布在至少两个提交的
要求。A/B 旧组已经全中，C 的较好交付没有带来更多有效确认；不依据这批人工候选
声称新提示提高或降低了生产召回，也不换材料继续寻找正结果。

### 用量、耗时与边界

所有客户端请求的终态 usage 均取得，失败包也计入。下表为终态报告的已知用量，
不等于已核实的账单费用。

| 指标 | 旧提示 | 新提示 |
| --- | ---: | ---: |
| 输入 token | 125,394 | 127,494 |
| 缓存输入 token | 55,168 | 65,664 |
| 非缓存输入 token | 70,226 | 61,830 |
| 输出 token | 57,567 | 56,296 |
| 其中推理 token | 52,401 | 48,318 |
| 单次耗时中位 | 65.1 秒 | 63.9 秒 |
| 各请求执行时间合计 | 406.1 秒 | 420.4 秒 |

两组已知输出合计 113,863 token，累计请求执行 826.5 秒；请求成对并发，累计值不是
整轮墙钟时间。新/旧输出比 0.978、中位耗时比 0.982，均在预设 1.5 倍内，但小样本
与缓存不平衡不能证明更快或更便宜。新组两份回执另报告网关内部失败切换，失败端
消耗未知；这不是脚本重试，也不能称为只有 12 次上游模型执行。

同字节旧 C 两次请求的输入计数分别为 7,073 / 7,817，服务端计量或转换差异未定位。
客户端固定协议成立，不代表上游内部路径一致；不能将单包交付差异或 token 波动
直接归因于这段提示。其余限制仍在：人工准备候选/切片、三个历史提交、每格两次，
未计人工准备成本、未运行初审，不能外推端到端审查效果。

本次只有实验与文档记录，未改代码或配置，未编译 Rust、未跑测试套件或 `make check`。
核对完整差异、引用与统计，并运行未暂存/暂存后的差异检查。

## 请求前冻结的材料

- A：`855a9b8`，项目 `.kloop/` 元数据列目录的尾斜杠误拒；无缺陷对照是私有配置
  读取仍被拦。真值按后续修复与回归、原两版读取路径人工核对。
- B：`8db82f7`，字面 xargs 替换进入 touch 等已知命令的数据/元数据位置后，被当作
  敏感读取硬拒并标危险；无缺陷对照是直接 echo 文本替换。真值按参数生成、消费方
  和后续角色回归核对，不把未知实际读取的正确拒绝当缺陷。
- C：`0fba3a9`，窗口长高并把光标放到底部后再缩小，缓存的绝对坐标未更新；无缺陷
  对照是几何不变且通过 backend setter 设置有效坐标。后续回归覆盖 grow→commit→shrink，
  原两版代码可直接说明新版本返回越界位置。

A/C 的缺陷为 C2，B 为 C1。三个目标均未用于此前记录的 plan 272–279 实验；这是新
开发留出材料，不声称模型训练未知。每份问题与切片由人工整理，不包含后续测试、
修复或答案标签。原包提示长度分别约 101,619 / 65,614 / 22,382 字符；旧/新使用相同
片段与候选权限。对照顺序按两次交替并成对并发，不称随机或无缓存偏差。

## 本轮冻结的新要求

以下为本仓库自有实验提示的增量原文，仅记录实际对照，不进入运行提示：

```text

Before choosing a verdict, assess two independent questions for the SAME input:
1. Security: does the change newly permit a forbidden operation, or preserve or
   strengthen that boundary? State not applicable when no security boundary is involved.
2. Functional correctness: for inputs the existing caller contract permits,
   do the result, error, side effect, availability, and cleanup remain correct?
   In the reason, explain both assessments and then derive the verdict.

Preserving security does not discharge functional correctness. If a permitted
input worked before but is now rejected or produces a wrong result, do not call
that harmless just because fewer operations are allowed. Conversely, refusal of
an input outside the supported contract is not itself a bug; identify the actual
contract and consumer, rather than treating every new refusal as a regression.
Intent or newly written comments cannot establish that an existing caller has
ceased to be supported. A preexisting exclusion needs the SAME observable failure
before, not merely the same abstract risk. If a necessary contract or consumer
is absent, use unresolved and name that gap. Do not invent downstream effects.
```
