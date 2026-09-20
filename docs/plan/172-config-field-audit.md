# Plan 172 — 全局配置的一次字段审计

> 来源:2026-09-20,plan 171 收尾后用户问「config 还有其他的根配置吗」,列完 11 个顶层段后接着
> 问「有可以删除的,以及字段名都合理吗」,然后:「改所有需要改的,写个 plan」。

## 一、审计结论

`~/.kloop/config.toml` 顶层 11 个键(`user_config.rs::validate_root`)。逐段读完解析代码,
**五处要动**,其余查过、有理由、不动。

### 要动的五处

1. **`providers.<id>.name` — 死字段,删。**
   `provider_config.rs:571` 把它解析进 `let _display_name` 就丢掉。全仓没有第二处引用,README
   也从没写过它。它今天唯一的效果是:让写了这个键的配置**不报错**——一个只会带来误解的承诺。

2. **`mcp.servers.<name>.bearer_token_env_var` → `bearer_token`(token 本身)。**
   它的注释写着 "secrets don't belong in config; codex form"。这个理由在**同一个文件里已经
   被否掉两次**:provider 的 `auth_header` 直接写秘密(plan 167),`[web].api_key` 刚刚也直接写
   (plan 171)。同一个 0600 文件,三处凭据三种规矩,第三种还是抄来的。
   **开工后发现更漂亮的落点**:`parse_server` 今天会**主动拒绝**明文 `bearer_token`、把用户
   推向 env 变量名。也就是说这个键名早就承认了是用户的自然直觉,只是当时拒绝它——现在把那个
   被拒绝的直觉变成正确答案即可,不必新造名字,也不必把凭据塞进通用的 `http_headers`
   (那是 plan 167 刚从 provider 那边**反向**走过的路)。
   连带删掉 `static_credential_missing`(专门表达"env 变量没设")和 `http_headers_for` 的
   fallibility:token 写在配置里就一定有值。`StaticCredentialUnavailable` 这个状态也因此只剩
   一个成因——服务器拒绝了我们手上这个 token。

3. **`mcp.servers.<name>.readonly` → `readonly_tools`。**
   值是工具名数组(`Vec<String>`),名字却长得像布尔开关:`readonly = ["search"]` 第一眼读成
   "这台 server 是只读的"。

4. **`[codemode]` → `[program]`。**
   同一件事今天有三个名字:配置段 `[codemode]`、工具名 `run_program`、env 前缀
   `KLOOP_PROGRAM_*`。用户在三个地方要换三次说法。**收敛到用户已经见得最多的那个**:工具名是
   模型可见契约(权限规则 `run_program(...)`、历史会话里全是它),不能为配置命名去动;env 已经
   是 `PROGRAM`。所以动段名。`codemode` 退回它本来的身份——实现/概念名(crate `kloop-codemode`、
   CodeAct),不再出现在用户要写的字里。

5. **`sandbox.auto_allow` → `trust_sandboxed`。**
   "自动允许"——允许什么?真实语义是"沙箱里跑的 bash 跳过权限门的询问层"。改名后这句话自己
   站得住:沙箱里的命令,信它。

### 退役键一律显式指路

五个旧拼法不是变成"未知键"就算了,各给一条说明该怎么改。`permissions.allow` 已经是这个先例。
理由:静默的 `unknown key 'bearer_token_env_var'` 说不出替代写法,而这是凭据——排查成本最高的
那一类。

### 查过、不动

- `providers.effort`(用哪一档)vs `models.efforts`(支持哪些档):单复数区分是准的。
- `providers.context_window`(这个网关截到多少)vs `models.context_window`(模型本身多大):
  plan 158 定过,是两个不同的事实。
- `web.search_provider` 与顶层 `provider` 同词不同义:有限定词,plan 158 复核过。
- `hooks.matcher`:只对 `pre_tool`/`post_tool`/`subagent_*` 有效,解析时就校验。
- `[permissions]` 只剩 `deny`/`ask`,`allow` 是显式退役并报错指路,不是遗漏。
- `[program]` 的六个旋钮(`memory_mb`/`stack_kb`/`cpu_secs`/`max_agents`/`max_concurrency`/
  `max_items`)**不收敛**:它是唯一把引擎内部量纲摆给用户的段,但"多数人不碰"是没人用,不是
  错;删掉省不下什么,还要动 `ProgramLimits`。记在这里,不做。

## 二、完成记录

## ✅ 已完成(2026-09-20;提交 SHA 以本条所在提交为准)

### 五处字段

| 旧 | 新 |
|---|---|
| `providers.<id>.name` | 删 |
| `mcp.servers.<n>.bearer_token_env_var` | `bearer_token`(token 本身) |
| `mcp.servers.<n>.readonly` | `readonly_tools` |
| `[codemode]` | `[program]` |
| `sandbox.auto_allow` | `trust_sandboxed` |

`SandboxSettings.trust_sandboxed` 只是配置这一层的拼法;core 的 `SandboxPolicy::auto_allow`
和 `sandbox_auto_allowed()` **不动**——那是引擎的词汇(54 处),不是用户要写的字,赋值处一行注释
说明两者是同一件事。

### 环境变量清零(用户开工中途追加:「我不需要环境变量,config 要自洽」)

删掉的:`KLOOP_PROGRAM_*`(6)、`KLOOP_SANDBOX`、`KLOOP_CONTEXT_WINDOW`、`KLOOP_DEFER_THRESHOLD`、
`KLOOP_DENY`/`KLOOP_ASK`/`KLOOP_ALLOW`、`KLOOP_PROVIDER`/`KLOOP_MODEL`/`KLOOP_EFFORT`、
`ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL`、`OPENAI_API_KEY`/`OPENAI_BASE_URL`/
`OPENAI_MODEL`。**没有 `~/.kloop/config.toml` 现在是启动错误**,不再回落到 `ANTHROPIC_API_KEY`
拼一个 provider。兼容分支一个没留(用户:「不需要考虑兼容性」)。

保留的是**不是配置来源**的那些:`KLOOP_VERSION`/`KLOOP_BUILD_SHA`(构建戳)、
`KLOOP_PRIVATE_*`/`KLOOP_PROCESS_TREE_*`/`KLOOP_PROJECT_STORE_CHILD_*`(测试与子进程传参)、
`KLOOP_SANDBOX=seatbelt`/`KLOOP_SANDBOX_NETWORK_DISABLED`(沙箱**发给**子进程的检测提示,方向
相反)。`core/src/tools/bash.rs` 的 `MODEL_SHELL_SECRET_ENV` 也留着:kloop 不读了,但别的工具
可能仍 export,从模型 shell 里剥掉照样对。

三处连带:

- **`[mcp].defer_threshold` 是新增的**。`KLOOP_DEFER_THRESHOLD` 是唯一一个 config 里**没有**
  等价物的,直接删就是打着"自洽"的旗号丢功能。
- **`KLOOP_CONTEXT_WINDOW` 的 `"off"` 语义没了**:config 的 `context_window` 只能是数字。同时
  `RuntimeSettings.context_window_env` 那个 `Option<Option<u64>>` 三态整个消失——它存在的唯一
  理由就是区分"env 没说"和"env 说了 off"。`ContextBudgetSource::Pinned` 从此只由 `/context`
  产生,配置不再产生它。
- **`resolve_table` 里的 model allowlist 检查成了死代码**并删除:`initial_model` 现在就是
  `profile.model`,而 `parse_profile` 早就拒绝了 `model ∉ models`。

### 凭据进了配置,Debug 就是泄露面

`McpTransport` 撤掉 `derive(Debug)` 改手写:`bearer_token`、`http_headers` 的值、stdio 的 `env`
值全渲染成 `<redacted>`,**只留键名**(什么被配置了是诊断,配成了什么不是)。和 plan 171 的
`WebConfig` 同一处理。

### 测试

- `mcp.rs`:`transport_debug_prints_names_but_never_values`(两条整串断言)、
  `a_configured_bearer_token_becomes_the_authorization_header`、
  `defer_threshold_defaults_and_parses_from_the_mcp_section`(默认/显式/0/负数/类型错);
  解析拒绝表补 `bearer_token = ""`、`= 3`、退役的 `bearer_token_env_var`。
- `provider_config.rs`:`environment_selects_only_declared_initial_routes` 改写成
  `the_file_selects_the_initial_route`(同样的两个断言,配置驱动);
  `selected_environment_credentials_do_not_make_other_profiles_ready` 与
  `environment_credentials_replace_the_secret_not_the_spelling` **整个删除**——它们测的行为不
  存在了;`env()` 测试辅助随之消失。
- **PTY harness(`tui_pty_support`)改写**:原先靠 `KLOOP_PROVIDER`+`OPENAI_*` 四个变量拼
  provider,现在**写一份真的 `~/.kloop/config.toml`**(0700/0600)。checked-in frames 一帧未变。
- **真 key 验收(`real_agent_program_workflow`,`#[ignore]`)改写**:`TestRoot::write_provider_config`
  把操作者的 env 翻译成配置文件再交给子进程。env 仍是**测试自己的**入参约定(交一把真 key 最
  顺手的方式),但被测进程只看配置文件。

`cargo fmt --check` 干净,`clippy --all-targets -- -D warnings` 全绿,`cargo test` **1601
passed / 0 failed**。

教训 168(三条:只让用户填他真有权选择的东西;同一件事在用户面前只该有一个名字;配置来源要
唯一——附带两条经验:删 env 前先确认 config 有等价物,删掉一条来源会让下游状态机塌一层)。

### 使用者须知(破坏性)

1. `~/.kloop/config.toml` **必须存在**,否则启动报错。
2. 段/键改名:`[codemode]` → `[program]`,`sandbox.auto_allow` → `trust_sandboxed`,
   `mcp.servers.<n>.readonly` → `readonly_tools`,`bearer_token_env_var = "VAR"` →
   `bearer_token = "<token 本身>"`;`providers.<id>.name` 删掉那一行。
3. 之前靠 `KLOOP_*` / `ANTHROPIC_*` / `OPENAI_*` 传的东西,现在写进配置文件。
