# Plan 58 — 调度工具对齐

> 状态：✅ 已完成（2026-08-03）
>
> 母计划：Plan 48
>
> 依赖：Plan 51、Plan 52（均已完成）
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 结论

本计划完整交付 kloop 原生调度面：

- depth-0 model-visible 工具 `cron_create`、`cron_delete`、`cron_list`、`schedule_wakeup`；
- `/loop` fixed/dynamic prompt adapter；
- 独立 scheduler、五字段 cron、用户本地时区、可控时钟、session/durable registry；
- owner-only durable 恢复、typed inbox 回灌、scheduler lifecycle event；
- TUI、plain、server 空闲自动 delivery，headless 有界退出；
- exact corpus、native report、3 个 scheduler executable pair、mutation-negative verifier。

不提供 CC 的 PascalCase alias。名称与 `ScheduleWakeup.delaySeconds`/kloop `delay_seconds` 的差异是公开的 native contract，不伪装逐字节兼容。

## Exact 2.1.220 证据边界

Plan 58 新增四组双采 deterministic fixture：

- `scheduler-create-list-{1,2}`；
- `scheduler-delete-durable-{1,2}`；
- `scheduler-parser-{1,2}`；
- `schedule-wakeup-gate-{1,2}`。

它们证明：

- CronCreate/Delete/List strict schema 与坏输入；
- session one-shot Create/List 不写 durable JSON；
- 预置 durable List/Delete 的文件变迁；
- ScheduleWakeup conditional required、stop 和 dynamic gate-off 输出；
- 8-hex job ID 只在 case 明确声明后归一化。

Exact bundle locator 另固定 parser、store、jitter fallback、ScheduleWakeup replace/stop/clamp、`/loop`、runtime worker、React delivery 和 scheduled safety framing。

以下 exact runtime cell仍保持 `unknown`：

- timed fire、DST 与时钟跳变；
- server-selected effective recurring jitter；
- restart/re-arm 与 enabled dynamic-loop 成功路径。

原因是固定目标没有可控 clock/gate seam；注入时钟就不再是同一个二进制。model-facing 描述是 recurring 10%/15min，bundle fallback 是 50%/30min，不能把任一值冒充 exact effective profile。

最终 corpus：

- 218 个 capture；
- 211 条 static evidence；
- 62 行 × 8 维 = 496 cells：125 compatible / 170 intentional-diff / 24 missing / 129 unknown / 24 n/a / 24 same；
- 7 个 executable pair，其中 Plan 58 新增 `scheduler-cron-schema`、`scheduler-cron-contract`、`scheduler-concurrency`。

## kloop 公开契约

### 工具

`cron_create`：

```json
{
  "cron": "*/5 * * * *",
  "prompt": "check status",
  "recurring": true,
  "durable": false
}
```

- `cron`、`prompt` 必填；
- `recurring` 默认 `true`；
- `durable` 默认 `false`；
- recurring job 最多存活七天，最后一个已到期 tick 仍投递后删除。

`cron_delete {id}`、`cron_list {}` 都是 strict object。List 按 `(next_fire_at, created_at, id)` 稳定排序，只展示 ID、human schedule、kind、durability 和截断 prompt，不伪造 runtime status。

`schedule_wakeup`：

- 正常路径条件要求 `delay_seconds`、`reason`、`prompt`；
- delay 先 round，再 clamp 到 60–3600 秒，最后对齐下一分钟；
- 新调用原子替换 owner 的旧 dynamic wakeup；
- `stop=true` 忽略其他字段，只清 dynamic slot，不清 fixed recurring Cron。

四工具仅在 scheduler-capable 的 depth-0 owner surface出现；不进入 `run_program` TypeScript API，不向子 agent、mock surface暴露。`cron_list` 可并发，三项 mutation串行。deny、plan mode和 explicit ask优先；普通 manual/accept-edits/bypass 对安全 owner-scoped scheduler control自动放行。

### Cron 与时间

- 五字段：minute/hour/day-of-month/month/day-of-week；
- 支持 wildcard、list、range、step、range/step；
- DOW 7 归一为 Sunday 0；
- DoM 与 DoW 同时受限时使用 cron OR 语义；
- 从下一分钟起最多扫描 527040 分钟；
- UTC minute scan 转换为 local parts，自然覆盖 DST gap/fold、月末和闰年；
- native recurring jitter采用 deterministic ID-derived 10% period、cap 15min；
- native one-shot 在本地 `:00`/`:30` 最多提前 90s；
- 这些是 kloop 产品行为，不升级为 exact effective jitter 证据。

所有时间测试使用 `ManualClock`；没有等待真实分钟、修改系统时钟或创建系统任务。

## Durability 与 owner

session-only job只在进程内 registry，shutdown时清除。

durable store：

```text
~/.kloop/scheduler/<project-key>/scheduled_tasks.json
~/.kloop/scheduler/<project-key>/scheduled_tasks.lock
```

- Git 项目使用 canonical `git-common-dir` 生成 SHA-256 project key，因此主 checkout和其 worktree共享 base-project identity；非 Git目录使用 canonical cwd；
- JSON 记录 schema version、project key、owner session、generation、created/last-fired/next-fire等恢复字段；
- 目录 0700、文件 0600、`O_NOFOLLOW`、regular-file/4MiB bound、advisory `flock`、同目录 exclusive temp、sync、rename和 parent sync；
- store损坏、schema/project mismatch、symlink与锁失败均 fail closed，不以空表覆盖；
- sibling lock inode可以留在私有 store目录，runtime不会持有残留 advisory lock；temp file不会残留；
- 最多 50 个 owner-visible job。

job绑定创建它的 session/thread。其他 owner的 List/Delete/worker都看不到、领不到该 job；两个同 owner runtime并存时，store claim在跨进程锁内线性化，只投递一次。worker每秒重读 durable store，但没有到期状态变化时不重写 JSON。

late durable one-shot只有在 frontend具备 question capability时才 claim，并回灌专用 framing，要求先调用 `ask_user_question`；无交互的 headless/server session保留 store中的 pending job，等待同 owner在可交互 frontend恢复。recurring missed backlog只 coalesce一次，再推进到下一个未来 occurrence。

## 到期投递与 shutdown

scheduler只向共享 Inbox写入 typed `ScheduledPrompt`/`SchedulerFailure`：

- 不修改 in-flight provider request；
- agent只在 round step boundary或 final boundary drain；
- scheduled prompt与普通 steering、background result使用不同 framing；
- drain时发 `ScheduledTaskUpdated(Fired)`；create/delete/wakeup发 Scheduled/Cancelled；失败发 Failed；
- server映射为无旧 turn owner的 `thread/scheduler/updated`。

frontend边界：

- TUI：Inbox activity在 idle时只发送一个 `WorkerMsg::Wake`；busy时由当前 turn在下一 step boundary吸收；
- plain：`tokio::select!` 同时等待 stdin与 Inbox activity，到期无需用户再按回车；
- server：CAS抢 single-flight，分配真实递增 turn ID，生成完整 `turn/started` → item lifecycle → `turn/completed`；不使用 `turnId: 0`；
- headless：主 turn结束后先关闭 scheduler，再关闭 background task/shell；session-only消失，durable保留。

损坏 durable store不会阻止已经到期的 session-only job投递；相同失败只通知一次，恢复后才允许同类失败再次报告。

## `/loop`

- `/loop 5m check status` 与 `/loop check status every 2h` 生成 fixed cron instructions；同一 turn先执行一次，再要求模型调用 `cron_create`；
- 无显式 interval进入 dynamic模式；每轮若仍有工作，调用 `schedule_wakeup`，完成时 `stop=true`；
- 空 dynamic prompt使用 `<<autonomous-loop-dynamic>>`；fixed sentinel为 `<<autonomous-loop>>`；
- fixed seconds被拒绝，最小 fixed interval为一分钟；
- interval parser接受 1–59m、1–23h、1–28d；无法由五字段稳定表达的值明确报错；
- prompt文本不改写，内部 instructions只使用 kloop snake_case名称。

## 安全与非目标

- 不安装或修改 crontab、launchd、systemd timer、登录项；
- 不访问公网、真实日历、通知服务或远程队列；
- 不创建跨测试长期存活的真实任务；
- durable state不写 rollout/history，不进入 provider context；
- Cron、dynamic wakeup、background Bash和Agent registry保持独立领域。

## 验证

```bash
python3 -B refs/claude-code-2.1.220/collect.py identity
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core scheduler
cargo test -p kloop-core tools::plan58_parity_tests
cargo test -p kloop-server
cargo test -p kloop-tui
cargo test -p kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

完成记录：exact no-wait corpus、native scheduler、`/loop`、durable owner恢复、三种长驻 frontend autowake、native report、paired matrix、文档和全量门禁在一次 `feat(plan58)` 提交中闭合。
