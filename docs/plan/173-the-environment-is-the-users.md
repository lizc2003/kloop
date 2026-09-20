# Plan 173 — 命令的环境是用户的

> 来源:2026-09-20,plan 172 收尾后用户问"现在还有环境变量吗"。清单里我提到代理变量仍由
> reqwest 自己读,并顺带提了 plan 170 在禁网时剥掉模型 shell 的代理变量。用户三句话:
> 「其实剥掉,是改变了行为,这样是不对的吧」→「总之,我觉得用户不知道你剥掉了,导致行为发生了
> 变化」→「不剥离,是吧」。

## 一、被推翻的是什么

plan 170 的处境是真的,不是误判:

- 沙箱必须放行回环(Go 的 `httptest` 几乎每个 HTTP 测试都要 listen 一个回环端口;不放行,
  一个真实会话因此逃出沙箱并一路带着 `disable_sandbox` 跑了 15 条命令)。
- seatbelt 的网络过滤**只匹配 host 是不是 `localhost`**,它看起来能写的端口根本不参与判定
  (plan 170 实测:`deny … "localhost:7897"` 放代理过去,`"localhost:*"` 连测试服务器一起封)。
- 于是回环上的代理把"禁网"原样变回"通网"。

plan 170 的解法是在 seatbelt 之外补一刀:禁网时把 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/
`FTP_PROXY`(两种拼法)从子进程环境里 `env_remove` 掉。

**这一刀现在撤掉。**

## 二、为什么撤

我先答的是"该不该剥",用户说的是另一件事:**剥了,而用户不知道**。核实下来这话精确到刺眼:

- README 写了,而且写得细(剥哪几个、为什么、`curl -x` 仍能穿透)。
- **运行时一个字都没有。**
- 而反方向的 `disable_sandbox`(放开限制)在 `permissions.rs` 里有 `[no sandbox]` 标记,注释
  写着"值得告诉人类"。**放开要通告,收紧且改环境却不通告**——这个不对称说不通。

我当时提的补救是"失败时在 tool result 里注明"。用户没要这个,要的是不剥。这个取舍成立,而且
比我的更好:

**靠篡改环境维持的边界,比诚实地弱更糟,因为假象会被当成保证。**用户在一台有代理的机器上看到
`allow_network = false`,他以为的是"封住了";实际是"封住了直连,且我们偷偷动了你的环境来让
常见客户端也走不通,但 `curl -x` 照样出去"。这三件事之间的距离,没有任何运行时线索能填。

撤掉之后,`allow_network = false` 的承诺缩到它真正做得到的那一句:**封的是直连**。一台跑着
回环代理的机器上,读 `HTTPS_PROXY` 的客户端仍然出得去——这句话写进 README,不再有假象。

换来的是另一条更根本的性质:**同一条命令,用户手跑和 kloop 里跑,环境一致、结果一致。**

## 三、边界:凭据仍然剥

`scrub_model_shell_env`(`ANTHROPIC_API_KEY` 等五个)**不动**,而且它是无条件的、与禁网无关。
两者看着像,道理不同:

- 代理变量是**用户的配置**,描述这台机器怎么上网,命令有权看见。
- API key 是**本进程的凭据**,从来就不属于模型控制的 shell——那不是"改变用户的环境",是不把
  自己的东西交出去。

判据:**这个变量属于谁。**属于用户的机器 → 原样传;属于 kloop 自己 → 不传。

## 四、完成记录

## ✅ 已完成(2026-09-20;提交 SHA 以本条所在提交为准)

`core/src/tools/bash.rs` 一处:`MODEL_SHELL_PROXY_ENV` 常量、`scrub_model_shell_proxy_env`
函数、禁网分支里的那一次调用,全部删除。`KLOOP_SANDBOX_NETWORK_DISABLED=1` **保留**——它是
个如实的信号(直连确实被封),不是在改环境。

测试 `a_denied_network_scrubs_the_proxy_route_an_allowed_one_keeps_it` 改写成
`only_this_process_credentials_leave_the_shell_environment`:两半整对象断言从"禁网=五个凭据
+八个代理变量 / 开网=五个凭据"变成**两侧都只有五个凭据**——把"禁不禁网都不动用户的环境"直接
钉住。plan 170 的 `loopback_stays_reachable_while_the_machine_boundary_holds` 不受影响:直连
`192.0.2.1` 仍被 seatbelt 在 connect 系统调用上拒掉。

README 沙箱段改写:不再声称禁网带走代理路由,改为说清它真正denies 的是直连,以及为什么不靠
编辑命令环境去补(一条命令必须手跑和这里跑一个样)。plan 170 文件加 ⚠️ 后半段已被推翻。

`cargo fmt --check` 干净,`clippy --all-targets -- -D warnings` 全绿,`cargo test`
**1601 passed / 0 failed**。

教训 169(靠篡改用户环境维持的边界比诚实地弱更糟;同一份代码里的通告不对称是最好的自检;
判据是"这个变量属于谁")。
