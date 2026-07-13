# Plan 20 — TodoWrite / 计划工具(备忘)

> 备忘,未开工。开工前读 HANDOFF。参考:cc TodoWrite(模型维护结构化任务清单,
> 整表替换、单 in_progress 软约束、UI 呈现进度)。回源核对 cc 字段与语义(教训 11)。

## 目标

模型自维护一个结构化任务清单(pending / in_progress / completed),让多步任务保持
连贯、进度对用户可见。cc 里这是多步任务的承重件,kloop 目前完全没有——高性价比
(一个内置工具 + UI 一行呈现)。

## 关键决定(开工时定)

- **工具形态**:单个 `todo_write` **整表替换**(cc 形态:模型每次发全量清单)还是
  增量 add/update/complete。倾向抄 cc 整表替换——无状态漂移、实现最简。
- **字段**:cc 是 `{content, status, activeForm}`(activeForm = 进行时文案,spinner 用)。
  status ∈ pending|in_progress|completed。要不要 id(整表替换可不用)。
- **存储**:纯内存(挂 Config/一个注册表,turn 间存活)还是随会话落 rollout?过程态
  不是历史,倾向不进 rollout;但进 rollout 便于 resume 看进度——开工时定。子 agent
  要不要独立 todo(倾向不继承,各自空)。
- **UI 呈现**:TUI 可折叠 todo 区(类似 `Cell::Agent` 那种活动块)、plain 打印变更、
  server 通知?最小版:变更时 note 打印;完整版 TUI 常驻小面板。
- **权限/并发**:纯状态无外部副作用——进只读白名单天然放行;但整表替换有顺序性,
  倾向串行不并发。开工时定归类。
- **单 in_progress 约束**:cc 软约束(同时只一个在跑)。做不做校验,开工时定。

## 不做

依赖图 blocks/blockedBy(cc 有,kloop 先不做);跨会话 todo 库;自动从对话推断任务。

## 测试

整表替换语义、status 状态机、(若做)单 in_progress 约束、UI 呈现契约、子 agent
todo 隔离。

## 完成标准

fmt/clippy/test 绿;真 key 一次多步任务里模型自发用 todo 跟踪进度;README、HANDOFF。
