# Plan 170 — 回环也算网络,于是每条测试都要脱沙箱

> 来源:2026-09-20,用户贴了一张审批截图说「太多这种请求权限了」。截图里那条是
> `cat > /tmp/envprobe8_test.go <<'EOF'`,带 `[no sandbox]` 通告。

## 一、弹窗的锅不在 heredoc

从用户项目桶的 `sessions/20260920-040528.jsonl` 读出的完整因果链:

1. `go test ./gateway/orchestrator/` 在沙箱里炸了:
   `panic: httptest: failed to listen on a port: listen tcp6 [::1]:0: bind: operation not permitted`
2. 下一条调用就带上了 `disable_sandbox: true`,description 写着 "Retry orchestrator test
   without sandbox"。**模型是对的** ——`disable_sandbox` 的参数描述要求"只有在命令因沙箱
   限制失败之后才设",它照做了。
3. 但它此后一直带着:那一会话 151 次 bash 里 **15 次** 带 `disable_sandbox`。
4. 这 15 次里 7 次被用户已有的 `sandbox_escalate(go test *)` 覆盖,不弹;剩下
   **8 次是 `cat > /tmp/envprobeN_test.go <<'EOF'`** —— heredoc 判 `Opaque`,
   opaque 脚本任何规则都命不中(plan 145 连逐字规则都删了),会话缓存按原文记而
   探针文件名每次递增。**8 次全弹**。

所以 heredoc/opaque 只是最后一环。根在第 1 步:`SandboxPolicy` 的网络只有一个二元开关
(`sandbox/mod.rs:361`),禁网把**本机回环**也一起禁了,而 Go 项目的单测大量用 `httptest`。
一旦模型脱了沙箱,`permissions.rs` 第 6 层的沙箱自动放行(本该吃掉所有 opaque 脚本)就
整轮失效。

顺带一提:用户 `permissions.json` 里有 `write_file(/private/tmp/**)`,本能覆盖这类写,
但模型选了 `cat > f <<EOF` —— bash 工具描述的重定向句列了 grep/glob/read_file/edit_file,
**唯独没有 write_file**(非目标,见第五节)。

## 二、seatbelt 能做什么、不能做什么(实测,不是推断)

macOS 25.4,`/usr/bin/sandbox-exec` 直接跑探针:

| 试的东西 | 结果 |
|---|---|
| 禁网基线:bind 127.0.0.1 / ::1 / 0.0.0.0,connect 外网 | 全部 `Operation not permitted`(复现了第 1 步) |
| 加上三行回环规则后 | v4/v6 回环 bind+connect 全通,**`::1` 覆盖了**,外网 `1.1.1.1:443` 仍拒 |
| `(local ip "127.0.0.1:*")` | **profile 语法错误**:`host must be * or localhost in network address` |
| `(local ip "localhost:*")` vs `(local ip "*:*")` 在 bind/inbound 上 | **结果完全一样**,`listen 0.0.0.0` 两种写法都放行 |
| 去掉 `network-inbound`,只留 bind + outbound | `bind` 成功、**`listen` 失败** —— 正是 httptest 那一步 |
| `(remote ip "localhost:*")` 对外网 | **生效**,外网连接被拒 |

两条结论:

- **remote 侧的 filter 区分地址,local 侧不区分。** 所以外发能被真正限制在本机,监听不能。
- **`listen()` 归 `network-inbound` 管**,不给这条就解决不了 httptest —— 而给了它,就无法
  把监听限制在回环。

## 三、做什么

### (a) 禁网时放行回环

`allow_network = false` 时,profile 追加一段回环规则(而不是什么都不追加):

```lisp
(allow network-bind (local ip "localhost:*"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
```

上游 codex `sandboxing/src/seatbelt.rs:336-339` 是同样三行(它只在代理/托管网络模式下发)。

**local 侧写 `localhost:*` 而不是 `*:*`**:两者今天等价,写 `localhost` 是声明意图 ——
哪天 macOS 把 local filter 修得真能区分,kloop 自动收紧;`*:*` 则永远不会。注释里写明
今天它不是保证。

### (b) 禁网时连代理变量一起剥掉

**(a) 单独做是不够的,这是实测出来的。** 放行回环之后,沙箱内:

```
curl -s http://example.com      →  <!doctype html>… 整页拿到
```

因为这台机器(和多数国内开发机)设了 `HTTPS_PROXY=http://127.0.0.1:7897`,**回环代理**。
curl/Go/pip 都会自己去读这些变量,于是"禁网"变成了"经代理上全网"。第一次发现它是因为
Go 的 `http.Get("https://1.1.1.1/")` 报的是 `tls: failed to verify certificate`
而不是 `operation not permitted` —— TLS 握手做了,说明 TCP 连上了。

seatbelt 挡不住这条:

| deny 规则 | 回环 connect | 经代理取外网 |
|---|---|---|
| `(remote ip "localhost:7897")` | 通 | **通** ← 端口粒度是装饰 |
| `(remote ip "localhost:*")` | 拒 | 拒 ← host 粒度才真生效 |

和 local 侧同一个毛病:**filter 只认 host 是不是 localhost,端口写了不算**。而 httptest
的随机端口和代理的 7897 在它眼里是同一件事,放行一个就是放行另一个。

所以这一刀改在进程环境里:`allow_network = false` 时,`shell_spec` 顺手
`env_remove` 掉 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `FTP_PROXY` 的大小写两种拼法
(Go 读大写、curl 偏好小写)。禁网状态下这些变量**没有任何合法用途** —— 那时候代理本来就
连不通,留着它唯一的作用就是穿透。开网时原样保留,那时它们是正常配置。

## 四、这条改动的真实语义

不是"放行回环",是:

> 沙箱内可以连本机、可以监听(**所有网卡,不只回环**),不能直接出这台机器;
> 而"经本机代理出去"这条默认路径被切断了 —— 但**挡不住刻意规避**(`curl -x http://127.0.0.1:7897`
> 仍然能走)。

三条边界,逐条说明为什么停在这里:

1. **监听面收不窄。** seatbelt 的 local filter 不区分地址,`listen 0.0.0.0` 与
   `listen 127.0.0.1` 一视同仁;而不给 `network-inbound` 就连 `listen` 都做不了,httptest
   直接没戏。**用户 2026-09-20 拍板接受**(自己的机器、自己的局域网)。
2. **代理穿透只堵默认路径。** 见上,seatbelt 层面没有能表达它的规则。
3. **刻意规避不在射程内。** 一个 full-disk read 的沙箱里,规避的路本来就不止这一条;
   这道门拦的是"无意中穿透",而实际发生的正是无意的(Go 自己去读了环境变量)。

**用户在看过第 2、3 条之后仍然选了这条路线(A 与 C 之间选 C)**,理由是禁网这个配置得
继续算数,而代价只有五行。

## 五、非目标

- **不给它配开关。** 这是一个本该如此的默认,不是档位;加开关等于把正确行为变成用户要自己
  发现的东西。
- **不碰 heredoc / opaque。** plan 144 的边界(写重定向 → Opaque)是有意的,修完之后这类
  命令根本不需要脱沙箱,第 6 层自动放行会吃掉它们。
- **不给 bash 描述补 write_file。** 那是另一条独立的软引导(plan 98 的续),值得单独一次。
- **不放 DNS。** 上游只在有代理端口时放 `*:53`;回环不需要域名解析,实测印证。
- **不动 `GOPROXY` 一类"模块源"变量。** 它们指向的是要下载的内容,不是出口;禁网时自然连
  不上。
- **不动 Linux/Windows。** 那两个平台还没有沙箱后端。

## 六、验收

- `seatbelt_profile` 在禁网时含三行、在开网时不含(开网路径本来就全放行)。
- `shell_spec` 在禁网时 `env_remove` 掉八个代理变量,开网时一个都不动。
- 真机端到端:同一个 Go `httptest` 用例旧 profile 失败、新 profile 通过;`curl http://example.com`
  在剥变量前拿到整页、剥变量后被拒。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿。

## ✅ 已完成(2026-09-20;提交 SHA 以本条所在提交为准)

`crates/core/src/sandbox/seatbelt_loopback.sbpl`(新)+ `sandbox/mod.rs`:`allow_network = false`
时追加回环三行,段内注释把第二节那张实测表写进代码旁边(包括
`(local ip "127.0.0.1:*")` 是语法错误这条)。

`crates/core/src/tools/bash.rs`:新 `MODEL_SHELL_PROXY_ENV` + `scrub_model_shell_proxy_env`,
挂在 `shell_spec` 里既有的 `if !policy.allow_network` 分支上,紧挨着
`KLOOP_SANDBOX_NETWORK_DISABLED`。

两处模型可见文本同步(否则模型仍以为禁网包含回环,还会为一个 httptest 预防性脱沙箱):
`DENIAL_HINT` 与 `bash` 工具描述都改成"no network beyond this machine",后者明写
"a loopback listener (a test server) works, so do not disable the sandbox for one"。

README 的 OS sandbox 段落重写了 network 那一条(回环例外 + 两条实测边界 + 代理变量剥离)。

### 测试

- `network_denied_still_allows_loopback_but_nothing_off_machine`:禁网含三行、不含无参数的
  `(allow network-outbound)`、**不含 DNS**(回环不解析域名);开网侧反过来。
- `profile_is_base_plus_exact_dynamic_sections`(既有的整对象断言)加上回环段作为尾巴。
- `a_denied_network_scrubs_the_proxy_route_an_allowed_one_keeps_it`:整对象断言
  `spec.env_remove` —— 禁网 = 五个密钥 + 八个代理变量,开网 = 只有五个密钥。
- `network_is_denied_where_the_bare_run_connects` **改写成**
  `loopback_stays_reachable_while_the_machine_boundary_holds`:它原本钉的正是本次要改的旧行为。
  新的两半——真实 listener 上的回环连接必须成功;`192.0.2.1`(TEST-NET-1)必须报
  `Operation not permitted`。**后者不需要网络也不会超时**:seatbelt 在 connect 系统调用上就
  拒了,实测 0.034s。

### 真机验证(scratchpad,未入库)

| | 改动前 | 改动后 |
|---|---|---|
| Go `httptest.NewServer` + 自连 | `bind: operation not permitted` | PASS,`served from http://127.0.0.1:54990` |
| `curl http://example.com`(有代理变量) | 拒 | **整页拿到** ← 这是 (b) 要堵的 |
| `curl http://example.com`(剥掉代理变量) | 拒 | 拒 |
| `exec 3<>/dev/tcp/192.0.2.1/80` | 拒 | 拒 |

`cargo fmt` 干净,`clippy --all-targets -- -D warnings` 全绿,`cargo test` **1600 passed / 0 failed**。

教训 166(两条:filter 的语法粒度 ≠ 判定粒度;放宽一道门先问它顺手放开了哪条既有的路)。

## ⚠️ 后半段已被 plan 173 推翻(2026-09-20 同日)

(b) 那一刀——禁网时 `env_remove` 掉代理变量——**撤掉了**。处境的判断没错(seatbelt 确实表达
不了"放行回环但不放行回环上的代理"),错的是补救的形态:**它靠篡改用户的环境来维持一道边界,
而运行时没有任何线索告诉用户这件事发生了**。同一份代码里,反方向的 `disable_sandbox` 尚且有
`[no sandbox]` 通告。

于是 `allow_network = false` 的承诺缩到它真做得到的那句:封的是直连;一台跑着回环代理的机器
上,读 `HTTPS_PROXY` 的客户端仍然出得去。详见 plan 173。
