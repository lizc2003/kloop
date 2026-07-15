# Plan 37 — plan mode(只读规划 → 批准后动手)

> 一句话定位:一个"先只读探索、产出计划、用户批准后才动手"的工作模式。概念两家收敛
> (cc 权限模式 `plan` + Enter/ExitPlanMode 工具;codex 协作模式 Plan =
> prompt 模板 + 少量硬约束),**强制层形态分歧**:cc 权限层硬强制、codex prompt 软
> 约束。按教训 30 的判据(分歧时按本项目最硬契约仲裁),kloop 取 **cc 的硬强制形**
> ——权限门"门上看到的=实际跑的"是 kloop 最硬的契约,"模型自觉不写"不算隔离。

## 回源结论(2026-07-15,两家真读,file:line)

- **claude-code**:`plan` 是权限模式之一(`types/permissions.ts`;循环 default →
  acceptEdits → plan → auto → bypass,shift+tab,`getNextPermissionMode.ts:9`)。
  入口:CLI `--permission-mode plan`(`main.tsx:1315`)/ settings defaultMode / 模型
  工具 `EnterPlanMode`(isReadOnly,`EnterPlanModeTool.ts:71-94`)。退出走
  `ExitPlanModeV2` 工具:plan 文本存磁盘 `<config>/plans/{slug}.md`
  (`plans.ts:83-133`),`checkPermissions` 恒返 ask "Exit plan mode?"
  (`ExitPlanModeV2Tool.ts:234-238`)——**审批弹层展示计划**;批准 → 恢复
  `prePlanMode ?? default` + tool_result "User has approved your plan…"
  (:357-403/481-491);**拒绝 → 保持 plan mode**,拒绝消息引导继续规划
  (`messages.ts:222`)。
- **codex**:Plan 是协作模式(`collaboration-mode-templates` 的 plan.md 模板 +
  reasoning effort,`models-manager/src/collaboration_mode_presets.rs:16-45`)。
  只读约束**主要在 prompt 层**(plan.md:17-39 禁 mutating);core 硬约束仅三处:禁
  `update_plan`(`handlers/plan.rs:84-88`)、`request_user_input` 仅 Plan 可用、抑制
  mailbox 自动 turn(`tasks/mod.rs:481-505`);产出 `<proposed_plan>` 块流式解析
  (plan.md:96-108)。**read-only 沙箱是解耦的另一套**(`permission_profile_catalog.rs:54`),
  不随 Plan 模式自动开启——即写禁止不硬。
- **收敛点(照抄)**:① plan 模式只能**显式退出**(两家同);② 产出物 = 结构化计划
  文本 + **用户批准环**(cc ExitPlanMode 审批 / codex proposed_plan 块);③ 都配
  system/prompt 层指引(硬门挡行为,提示词教模型"现在该干什么")。
- **分歧(按 kloop 契约选边)**:强制层——取 cc 的权限硬强制;codex 的"软约束 +
  解耦沙箱"对 kloop 是倒退(kloop 权限门已比 codex 严,教训 30 同款仲裁)。

## kloop 现状与落点

- kloop 权限模式现有三档 default/acceptEdits/bypass(`core/src/permissions.rs`,plan 8
  明确"cc 独有暂不做 permission modes 全集"——本 plan 补 plan 档,是当时挂账的兑现)。
- 落点:
  1. **Plan 档进权限管线**:只读自查白名单(grep/glob/read_file/web_fetch/
     bash 只读分类…)照常放行;**写类与不可分析 bash 一律拒**(is_error tool_result +
     "in plan mode" 改道引导,turn 继续);deny/安全检查仍最先。位置在管线哪一层、与
     sandbox auto-allow 的先后,开工定(倾向:plan 拒绝在 auto-allow **之前**——只读
     承诺严于遏制)。
  2. **`exit_plan_mode` 工具**:入参带计划文本(kloop 无 plans 磁盘目录,首片内联;
     cc 的磁盘存储挂账);走审批,**`ConfirmRequest.preview` 白嫖 plan 21 的弹层**展
     示计划全文(plan 25 的可滚弹层正好装长计划);批准 → 切回进入前的模式 +
     tool_result 确认;拒绝 → 留在 plan 档 + 引导继续规划(cc 语义)。
  3. **入口**:CLI `--plan`;TUI 快捷键(kloop 无模式循环键,是否引入 shift+tab 循环
     开工问用户);`enter_plan_mode` 模型工具挂账(cc 有,首片人发起够用)。
  4. 子 agent 继承 plan 档(随 Config clone,现有机制白送);system 注入一段 plan
     mode 指引(硬门 + 提示词两层,收敛点 ③)。
- codex 三个小硬约束的 kloop 对应:todo_write 在 plan 档**不禁**(规划本来要列
  todo);autowake 抑制不适用(kloop 异步子 agent 在 plan 档本就只读)。

## 关键决定(开工时定 / 问用户)

1. **与 bypass/yolo 的交互**:cc 是"bypass 可用则放行全部"——kloop 倾向 `--yolo` 与
   `--plan` 互斥报错(两者语义相反,静默一边赢是坑)。
2. **计划载体**:入参内联(倾向,最小)vs 磁盘 plans 目录(cc 形,挂账;真要复盘时
   rollout 里本来就有)。
3. **TUI 入口**:只 CLI flag 起步,还是加模式切换键 + 状态栏显示当前档位(TUI 现无档
   位显示,这半是独立 UI 活)。
4. **只读边界细则**:task/run_program 在 plan 档是否可用——倾向可用(它们内部每个调
   用再过门,plan 档随 Config 继承进去,写在深处同样被拒;教训 19b 的安全门不豁免)。

## 不做(挂账)

`enter_plan_mode` 模型工具(首片人发起);plans 磁盘目录 + slug;cc 的 auto/分类器模
式与 `allowedPrompts` 入参;plan V2 多 agent 探索(cc `planModeV2.ts`);codex 式
`<proposed_plan>` 流式块解析(kloop 走工具审批环,不解析正文);settings 默认档位。

## 测试

权限管线 plan 档:写类拒(write/edit/不可分析 bash)、只读放行(grep/glob/read/只读
bash)、deny 仍最先、acceptEdits 语义被 plan 覆盖、`--yolo` 互斥报错;`exit_plan_mode`
审批往返:preview 带计划全文、批准切回前模式、拒绝留档 + 引导文案;子 agent 继承
(plan 档下子 agent 的写调用被拒);system 注入含 plan 指引;`--plan` 解析。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 补 plan mode;本文件补完成记录;HANDOFF 补
能力条目(permission modes 从三档到四档)与教训。真 key 验收:`--plan` 下模型探索代码
库产计划 → 弹层批准 → 切回 default 动手改文件闭环;拒绝一次验"留档继续规划"。
