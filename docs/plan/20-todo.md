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

## ✅ 完成记录(提交 48d0a49)

**拍板(开工时定,plan lean + cc/claw 回源核对)**:
- **形态**:整表替换(cc/claw 形态)。单 `todo_write`,模型每次发全量。
- **字段**:`{content, activeForm, status}`,status ∈ pending|in_progress|completed,**无 id**(整表替换用不上)。回源核对 claw `crates/tools/src/lib.rs` 的 `TodoItem`/`execute_todo_write`/`validate_todos`。
- **校验**:列表非空 + content/activeForm 非空 + status enum(坏 status = serde 反序列化错→is_error)。**多 in_progress 允许**——claw 源码明确注释 "Allow multiple in_progress items for parallel workflows",cc 的"一次一个"是 tool description 引导不是硬规则,故不硬校验(教训 11:回源核对推翻了 plan 里"cc 软约束"的模糊表述,claw 直接允许)。
- **存储**:Config 上 `todos: Arc<Mutex<Vec<TodoItem>>>`,**过程态非历史**,不进 rollout(plan lean)。turn 间存活(前端复用同一 `Arc<Config>`)、resume 从空开始——但模型靠 history 里重放的自身 todo_write 重建,TUI 也把历史 todo_write 重放成 checklist,所以 resume 的"看不到 todo"其实被 history 兜住了。
- **子 agent**:各自 fresh 空表——`task_tool` 在克隆的 sub Config 上 `todos: Arc::new(Mutex::new(Vec::new()))` 重置(否则 `..(*ctx.cfg).clone()` 会共享 Arc)。契约测试:子 agent 走 run_turn 全链 todo_write 后父存量不变。
- **权限/并发**:readonly 自判自动放行(`permissions.rs` CallFacts::is_readonly 加 `todo_write`——无外部副作用,cc 从不问);并发直列(不进 is_concurrency_safe,整表替换有顺序性)。
- **UI 呈现**:plan 自己建议"类似 Cell::Agent 那种活动块",故落 `Cell::Todo` 而非常驻面板。Ui trait 加 `todo_update(agent, &[TodoItem])`(默认 note 一行摘要)。`todo_write` **不走通用工具行**(cc 同款,把 checklist 渲在工具行位置):TUI ToolStart 抑制 `todo_write`、`TodoUpdate` 事件拥有 `Cell::Todo` 就地更新(新 user turn 起新块),plain 打印标记清单、server 发 `todo/updated`。子 agent 的 todo 在 TUI 内部化(ChannelUi 只转发主 agent,教训 3),plain/server 转发(带 agent 字段)。
- **露出**:全 depth(子 agent 也规划);受限 agent 类型不白名单则不给(非 read_offloaded 那种强制例外)。

**结构**:新 `core/src/tools/todo.rs`(TodoItem/TodoStatus/todo_write_def/todo_write_tool/parse_todos + 校验)。接线:`tools/mod.rs`(mod + pub use + tool_defs 全 depth push + dispatch 分发)、`config.rs`(todos 字段)、`agent.rs`(Ui::todo_update 默认)、`permissions.rs`(readonly)、`tools/task.rs`(子 agent 重置)。前端:tui `events.rs`(AgentEvent::TodoUpdate + ChannelUi override)、`app.rs`(Cell::Todo + todo_cell 就地更新 + 抑制工具行 + 新 turn 重置 + resume 重放)、`render.rs`(checklist 渲染);cli `main.rs` StdoutUi(抑制工具行 + 打印清单);server `lib.rs` ThreadUi(todo/updated 通知)。

**验证**:`cargo test`(308,+10)、clippy -D warnings 0、fmt 绿。**挂账**:真 key 双轨"多步任务里模型自发用 todo"验收待做(需代理+key,问用户)——契约层已全覆盖(工具语义/校验/隔离/权限/三前端呈现/server wire)。

**有意偏离/未做**:不抄 claw 的"all-completed 自动清空 list"(纯 UI 细节,且我们的 todo 不回灌模型上下文,清空只影响面板;不清空=完成后仍显示全✓,更好的 UX,更简单);不抄 claw 的落盘 store(改 Config 内存态);不做依赖图 blocks/blockedBy、跨会话 todo 库、list 落 rollout(plan 明确不做)。
