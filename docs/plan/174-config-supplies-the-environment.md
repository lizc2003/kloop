# Plan 174 — 配置**供给**环境(方向相反的那一条)

> 来源:2026-09-20,plan 173 之后用户贴出一段代理 env 块:「我想在配置里也支持 env 的定义」。
> 中途改主意「不做 config 里定义,我在命令行里设置这些 env,也起作用吧」(确认:起作用,已回退),
> 随后定稿:「config 里也可以设置 env,这样我在 shell 里没设,也不会出问题」。

## 一、这不违反 plan 172

plan 172 刚把**所有**"从环境读 kloop 的配置"删干净。`[env]` 看着像开倒车,其实是**反方向**的:

- plan 172 删掉的是 **环境 → kloop 的设置**(`ANTHROPIC_API_KEY` 决定用哪个 key,
  `KLOOP_PROVIDER` 决定用哪个 provider)。那是双来源,会随启动它的 shell 变。
- `[env]` 是 **配置 → 环境**。写进去的东西不会被读回来变成 kloop 的任何设置;它只是在进程启动
  时把变量摆进环境,给那些**本来就读环境的东西**用——reqwest 的代理、TLS 的 CA 路径、子进程。

一句话:kloop 的设置只有一个来源(文件);机器的环境仍然是环境,而文件可以**供给**它。

## 二、shell 赢,文件兜底

`[env]` **只填 shell 没设的变量**。理由三条,都指向同一侧:

1. 用户的原话就是兜底——「我在 shell 里没设,也不会出问题」。
2. 环境变量的通行惯例是**离进程越近越赢**:`HTTPS_PROXY=… kloop` 临时换个代理调试,必须压得过
   配置文件,否则那条命令行就是骗人的。
3. **可诊断性**:shell 里有什么,`echo $HTTPS_PROXY` 当场可见;文件里有什么要去翻。让当场可见的
   那个赢,"为什么是这个值"才有最短的答案。

设成空串**算设过**(好几个工具把空串读作"禁用",那是决定,不是缺席)。

## 三、只能在 main 的第一行附近做

`set_var` 只在进程单线程时可靠,而 `#[tokio::main]` 在 main 体执行前就起了 worker 线程。所以:

- `#[tokio::main]` 拆开,原 main 体改名 `run()`,`fn main` 手建 runtime;
- `[env]` 在 `Builder::build()` **之前**应用,那时进程确实还是单线程——这就是那处 `unsafe` 唯一的
  安全依据,写进注释;
- 这也是全仓第一处 `set_var`(plan 79 清掉的是**测试里**的 set_var,理由是测试间竞争;产品代码在
  单线程启动期设置不受那条教训约束)。

配套:解析与"shell 赢"的判定都是纯函数(`load_env_overrides` / `pending_env`),测试只测这两个,
一行 `set_var` 不进测试。

## 四、两个拒绝

- **`HOME`/`USERPROFILE` 拒绝设置。**这个文件刚刚就是从 HOME 找到的;改掉它会让 config 和从它
  派生的一切(sessions、projects、OAuth store)指向两个不同的家。
- **错误信息只出现变量名,不出现值。**`[env]` 是放带密码的代理 URL 的合理位置。

## 五、`--mock` 与坏文件

`--mock` 不读(hermetic 契约)。判断 mock 用的是对 argv 的保守扫描而非真解析器——真解析器需要
`run()` 里的子命令归一化,而**往"是 mock"的方向判错只会导致不读配置,那正是 mock 的行为**。

配置读不出/parse 不了时,pre-runtime 这一读**静默返回空**:`--list-sessions` 必须在文件坏掉时还能
用。代价是 `[env]` 自己的错误会被这一读咽掉,所以 `RuntimeSettings::load` 里**再解析一次**,只为
让它在正常启动路径上报出来——`--list-sessions` 不走那里,两个需求各自成立。

## ✅ 已完成(2026-09-20;提交 SHA 以本条所在提交为准)

`user_config.rs`(`load_env_overrides` / `pending_env` / `config_env` + 顶层白名单加 `env`)、
`main.rs`(拆 `#[tokio::main]`、一处 `unsafe set_var`)、`startup.rs`(一行再校验)。

测试两个,都不碰真环境:
`env_section_parses_pairs_and_refuses_what_would_contradict_itself`(整对象断言三个 pair 含空值;
`env = 3` / `X = 3` / `HOME` / `USERPROFILE` 四种拒绝的整串断言)与
`a_variable_the_shell_already_set_is_left_alone`(注入 `present` 闭包,三种覆盖情形)。

README 在"文件就是全部"那段后面补了反方向的这一条;`config-demo.toml` 带一段**注释掉的**
`[env]` 代理示例(照抄 demo 的人不该被塞一个他没有的代理——和 `api_key` 同一个判断)。

`cargo fmt --check` 干净,`clippy --all-targets -- -D warnings` 全绿,`cargo test`
**1603 passed / 0 failed**。

教训 170。
