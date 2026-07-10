# Plan 19 — 工具面补全

> 体量偏大,开工时选片,可能不止一个会话。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 Grep/Glob/WebFetch/后台 Bash 形态、codex 的对应工具;实现细节回源核对(教训 11)。注意 P0 决定 1 仍然有效:编辑保持 Edit 形态,不做 apply_patch/多文件 patch。

## 现状(补全的起点)

六个内置:bash、read_file、write_file、edit_file、read_offloaded、task。文件搜索靠 bash 转 rg/grep(能用:word-only 白名单已把它们归只读、可并发),但引号/转义易错、输出无结构、每次占一条 bash 调用。网络完全没有(core 无网络是刻意边界)。长命令只有前台 + 超时。

## 候选切片(开工时和用户定选哪几片、什么顺序)

1. **grep + glob 专用只读工具**(最高频,建议必选):倾向纯 Rust 库实现(ripgrep 的 `grep-searcher`/`ignore` crate 族,零外部二进制依赖,尊重 .gitignore),不倾向 spawn rg(存在性/版本不可控)——开工时定。天然 readonly:进并发批、权限只读自查直接放行。输出形态(文件:行号:内容、命中数上限、超限截断进 offload)对照 cc 定。要不要顺带 list_dir,开工时定。
2. **web_fetch / web_search**:架构决定优先于功能——core 无网络是硬边界(reqwest 独占在 provider),所以两条路:做成 cli 注册的内置 ToolSource(复用 MCP 的缝,core 零改动,倾向);或者用 Anthropic 服务端 web_search 工具(provider 层声明即可,但 OpenAI 轨没有对等物、且不解决 fetch)。开工时定,可能两者都要。fetch 的安全面:SSRF(内网地址拒绝)、大小上限、HTML→文本降噪。
3. **bash 后台任务**:`run_in_background` 参数 + 查询输出/终止的配套工具(cc 形态)。进程生命周期归属(退出时回收、interrupt 语义)、输出缓冲落 offload 目录。价值:dev server、长编译。
4. **图片输入**:read_file 读图片 → 协议要加 image 块(protocol ContentBlock + 双 provider 翻译 + rollout 前向兼容),和 plan 17 的 thinking 是同类协议扩展,开工时定归 17 一起做还是这里做。

## 不做(维持现状)

apply_patch(P0 决定 1);notebook 工具(niche);TodoWrite 类计划工具(价值存疑,备选池观察)。

## 测试

grep/glob:契约测试(命中形态、gitignore 尊重、上限截断、并发批分类、权限只读放行);web:ToolSource 缝的 mock 测试 + SSRF 拒绝表;后台 bash:启动/查询/终止/退出回收、interrupt 不留孤儿进程;图片:协议往返 + 两 provider 翻译契约。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收按所选切片(至少:模型用 grep 工具完成一次真实代码检索任务,对比 bash-rg 无引号转义痛点);README、HANDOFF 更新;未选切片记挂账。
